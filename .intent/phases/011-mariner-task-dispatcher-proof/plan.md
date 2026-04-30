# 011 Mariner Task Dispatcher Proof Plan

Session:

- A

## Goal

Add assertion-backed task-dispatcher workloads that prove the current
single-shard runtime can keep useful work moving after supervised worker
panics across all restart policies, and can visibly stop restarting when a
budget is exhausted.

This slice should make the Tina-Odin "dead worker is not a dead system" story
real in `tina-rs` without adding I/O or a simulator.

## Context

`CurrentRuntime` already supports:

- typed isolate registration and runtime ingress
- deterministic registration-order stepping
- local send dispatch
- local spawn dispatch
- runtime-owned parent-child lineage
- restartable child records
- address generations and stale-address closure
- direct `RestartChildren`
- supervised panic restart with `RestartPolicy` and runtime-lifetime
  `RestartBudget`
- deterministic causal trace events

What is missing is a reference-shaped workload package that composes those
pieces in one place and proves the user-facing claim across the policy surface
instead of only in narrow unit tests.

## References

- `.intent/SYSTEM.md`
- `.intent/phases/011-mariner-task-dispatcher-proof/spec-diff.md`
- `ROADMAP.md`, "Mariner supervision and dispatcher proof"
- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/runtime_properties.rs`
- `tina-runtime-current/tests/spawn_dispatch.rs`
- `tina-runtime-current/tests/address_liveness.rs`

## Mapping From Spec Diff To Implementation

- Task-dispatcher workload -> new black-box runtime integration test plus a
  small runnable example that mirrors the same workload shape.
- Dispatcher isolate -> parent isolate that can spawn restartable worker
  children and accept user-facing `Submit` messages, delegating slot
  resolution to a registry isolate.
- Registry isolate -> ordinary user-space isolate that stores current worker
  addresses, forwards to the current worker incarnation, panics on missing
  slots, and can be refreshed by the test harness after observing restart
  trace events.
- Worker children -> restartable isolates that record successful task handling
  and can panic on a specific message.
- Supervisor config -> `CurrentRuntime::supervise(dispatcher,
  SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(...)))`.
- Stale worker proof -> keep the old worker `Address<WorkerMsg>` and assert
  `runtime.try_send(old, ...) == Err(TrySendError::Closed(...))`.
- Replacement proof -> infer replacement isolate id from
  `RestartChildCompleted` and construct a typed `Address<WorkerMsg>` with
  `AddressGeneration::new(0)`. Use a helper shaped like
  `replacement_address_for(failed_isolate, trace) -> Option<Address<WorkerMsg>>`.
- Trace proof -> assert exact or filtered event sequences for panic,
  supervised trigger, restart attempt, completion, and later handling.

## Phase Decisions

- This slice is an e2e-style runtime proof, not a simulator slice.
- The proof belongs in `tina-runtime-current/tests/task_dispatcher.rs` unless
  implementation uncovers a strong reason to use unit tests for private state.
- Use black-box trace assertions; do not add crate-private snapshots for this
  slice.
- Replacement address discovery is done from trace events and public address
  constructors, not from a runtime API.
- Cover `RestartPolicy::OneForOne`, `RestartPolicy::OneForAll`, and
  `RestartPolicy::RestForOne` in the task-dispatcher workload.
- Use a generous `RestartBudget` in the policy-shape tests so budget exhaustion
  does not obscure policy behavior.
- Add a separate small-budget dispatcher proof for exhaustion visibility.
- Do not add timed budget-window semantics here.
- Do not add a logical-name registry to the runtime. Use a tiny user-space
  registry isolate as the faithful reference pattern instead.
- Do not treat this as a product API for refreshing addresses. Address-refresh
  and logical naming remain user-space/runtime-design topics for later slices.
- Clients send work to the dispatcher, not directly to the registry.
- The dispatcher delegates slot resolution to the registry isolate, not to a
  shared test-owned address table.
- Tests may observe stale-address `Closed` rejection before registry refresh;
  the runtime does not auto-route around stale addresses.
- A missing registry slot is a loud programmer error in this workload. Panic
  visibly instead of returning `Effect::Noop` and losing work silently.
- One-for-all may abandon buffered worker messages as part of stopping the old
  worker set. The tests should script ordering so the intended causal story is
  unambiguous.
- The proof may use deterministic test mailboxes rather than the SPSC mailbox.
  The goal is runtime semantics, not mailbox implementation coupling.
- Add `examples/task_dispatcher.rs` in this slice if it can stay aligned with
  the integration-test workload. The integration test is still the proof
  surface.

## Proposed Implementation Approach

1. Add `tina-runtime-current/tests/task_dispatcher.rs`.
2. Define test infrastructure locally:
   - `TestShard`
   - deterministic `TestMailbox<T>`
   - `TestMailboxFactory`
3. Define workload messages:
   - `DispatcherMsg::SpawnWorkers`
   - `DispatcherMsg::Submit(Task)`
   - `RegistryMsg::{Set, Get}` or equivalent tiny address-book verbs
   - `WorkerMsg::Run(Task)`
4. Define task shapes:
   - normal task records completion
   - poison task panics in worker handler
5. Define `Dispatcher` isolate:
   - owns a shared completed-work log
   - owns configured worker count/capacity
   - owns the registry isolate address
   - returns `Effect::Spawn(RestartableSpawnSpec<Worker>)` for setup messages
   - returns `Effect::Send(...)` to the registry for routed tasks
6. Define `Registry` isolate:
   - stores current worker addresses by logical slot/index
   - has no special runtime privileges
   - panics on missing slots instead of silently dropping work
   - is tiny enough that downstream users can copy the pattern
7. Define `Worker` isolate:
   - records successful task handling into shared test state
   - returns `Effect::Noop` on normal completion
   - panics on poison task
   - is constructible through `RestartableSpawnSpec` factory
8. Add a harness helper that can:
   - register a dispatcher
   - register a registry isolate
   - configure the requested restart policy/budget
   - spawn N restartable workers
   - capture worker addresses from `Spawned` events
   - route tasks through the dispatcher
   - refresh selected registry entries from
     `RestartChildCompleted` trace events
9. Add one-for-one proof:
   - register dispatcher
   - register registry
   - configure dispatcher as one-for-one supervisor
   - spawn two restartable workers via dispatcher messages
   - capture their addresses from `Spawned` trace events and seed the registry
   - send normal work to worker A and B and prove completion
   - send poison work to worker A
   - step runtime and prove worker A panicked/restarted while worker B
     identity survives
   - assert old worker A address is closed
   - update the registry entry for worker A from the
     `RestartChildCompleted` trace event
   - send later normal work through the dispatcher to replacement worker A and
     worker B
   - prove both complete
10. Add one-for-all proof:
   - spawn at least three restartable workers
   - panic one worker
   - prove all worker records get fresh isolate ids
   - prove all old worker addresses return `Closed`
   - refresh all registry entries from restart-completion trace events
   - prove later routed work completes on every replacement
11. Add rest-for-one proof:
   - spawn at least three restartable workers
   - panic the middle worker
   - prove the older worker keeps its isolate id
   - prove failed and younger workers get fresh isolate ids
   - prove old failed/younger addresses return `Closed`
   - refresh failed/younger registry entries
   - prove later routed work completes on surviving and replacement workers
12. Add budget-exhaustion proof:
   - configure a small budget
   - trigger one supervised restart successfully
   - trigger another failure that exceeds the budget
   - prove `SupervisorRestartRejected { BudgetExceeded { .. } }`
   - prove no replacement worker is created for the rejected response
   - prove the rejected worker's stale address is closed
13. Add repeated-run test:
   - run the same scripted workload twice
   - compare trace, completed-work log, and key isolate ids
14. Add `examples/task_dispatcher.rs`:
   - keep it aligned with the test workload
   - keep it runnable and readable
   - avoid making it the only proof surface
15. Keep generated-history property tests unchanged unless this workload exposes
   a missing general invariant that belongs there.

## Acceptance

- `task_dispatcher.rs` proves a worker panic does not prevent later work from
  completing when the configured policy allows restart.
- Under one-for-one, the failed worker gets a fresh isolate id, siblings keep
  their original isolate ids, the stale failed-worker address returns
  `TrySendError::Closed`, and later routed work completes on the replacement
  and sibling workers.
- Under one-for-all, all workers get fresh isolate ids, all old worker
  addresses return `TrySendError::Closed`, and later routed work completes on
  all replacements.
- Under rest-for-one, the older sibling keeps its isolate id, the failed and
  younger workers get fresh isolate ids, stale restarted-worker addresses
  return `TrySendError::Closed`, and later routed work completes on all usable
  addresses.
- Under budget exhaustion, `SupervisorRestartRejected` is emitted and no
  replacement worker is created for the rejected response.
- The dispatcher workload uses a registry isolate rather than a shared mutable
  address table.
- Clients submit work to the dispatcher, and the dispatcher delegates
  forwarding to the registry isolate.
- Submitting to an unregistered slot panics visibly instead of silently
  dropping work.
- `examples/task_dispatcher.rs` exists and mirrors the workload shape closely
  enough that it is a useful smoke surface without becoming the proof.
- Trace assertions prove:
  - `HandlerPanicked`
  - `IsolateStopped`
  - `SupervisorRestartTriggered`
  - `SupervisorRestartRejected`
  - `RestartChildAttempted`
  - `RestartChildCompleted`
  - later `HandlerFinished { effect: EffectKind::Noop }` for replacement work
- Repeated identical runs produce identical traces and completed-work logs.
- `make test` passes.
- `make verify` passes before commit.

## Tests And Evidence

- New integration tests in
  `tina-runtime-current/tests/task_dispatcher.rs`.
- Runnable smoke example in
  `tina-runtime-current/examples/task_dispatcher.rs`.
- Existing runtime tests remain unchanged except for incidental import cleanup
  if needed.
- Verification commands:
  - `make test`
  - `make verify`

## Traps

- Do not add simulator vocabulary or claim this is deterministic simulation.
- Do not add I/O or async driver behavior.
- Do not add public replacement-address refresh APIs.
- Do not add a runtime logical-name registry.
- Do not inspect private runtime internals; trace events are enough.
- Do not let the registry isolate quietly become runtime infrastructure.
- Do not weaken stale-address behavior to make the example easier.
- Do not make `OneForOne` restart siblings.
- Do not rely on logs or printed output as proof.
- Do not make the task-dispatcher proof the only test of supervision.
- Do not touch mailbox unsafe code.
- Do not touch `tina.png`.

## Files Likely To Change

- `tina-runtime-current/tests/task_dispatcher.rs`
- `tina-runtime-current/examples/task_dispatcher.rs`
- Possibly `README.md` or `ROADMAP.md` after implementation is accepted, if
  the proof closes a roadmap item.
- Possibly `.intent/SYSTEM.md` after proof, if the system baseline should say
  this reference-shaped workload exists.
- `CHANGELOG.md` during phase closeout.
- `.intent/phases/011-mariner-task-dispatcher-proof/commits.txt` during
  phase closeout.

## Areas That Should Not Be Touched

- `tina` public trait vocabulary
- `tina-mailbox-spsc`
- `tina-supervisor` API
- runtime public API, unless review identifies a necessary gap
- simulator/driver crates that do not exist yet
- `tina.png`

## Assumptions And Risks

- The test can construct typed replacement addresses from trace-observed
  `RestartChildCompleted` ids and `AddressGeneration::new(0)`.
- The dispatcher routes through explicit worker addresses stored in a
  user-space registry isolate. That is acceptable for this proof; the goal is
  supervised worker replacement, not logical-name ergonomics.
- Exact whole-trace assertions may be brittle as the runtime grows. Prefer exact
  assertions for key causal subtrees plus broad repeated-run equality for the
  whole trace.

## Commands

- `make test`
- `make verify`

## Ambiguities Noticed During Planning

- The remaining judgment call is how small the runnable example can stay while
  still honestly mirroring the tested workload. If it starts dragging in too
  much scaffolding, keep it minimal and leave richer ergonomics to a later
  slice.
