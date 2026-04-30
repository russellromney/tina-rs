# 011 Mariner Task Dispatcher Proof Reviews And Decisions

This file is append-only.

- Review rounds are written in Session B.
- Decision rounds are written in Session A in response to findings.
- Do not rewrite earlier review text to make it look resolved.

---

## Round 1: Spec Diff Review

Artifact reviewed:

- `.intent/phases/011-mariner-task-dispatcher-proof/spec-diff.md`

Read against:

- `.intent/SYSTEM.md` (just-updated to include slice 010's
  supervised-restart capability)
- `.intent/phases/005-010` (the Mariner groundwork this slice
  exercises)
- `ROADMAP.md` ("Mariner supervision and dispatcher proof" —
  this slice is the explicit done-when item)
- `tina-runtime-current/tests/runtime_properties.rs` (existing
  proptest infrastructure)
- `tina-runtime-current/tests/spawn_dispatch.rs` and
  `address_liveness.rs` (existing integration test patterns)

### Positive conformance review

Judgment: passes

- **P1.** Right slice in the right place. Mariner's ROADMAP
  done-when criteria call for "task-dispatcher proves stale
  worker identity/restart behavior with trace assertions" —
  this is that slice. It's the user-facing payoff for slices
  005-010.
- **P2.** Covers all three restart policies (OneForOne,
  OneForAll, RestForOne) plus budget exhaustion in one
  workload. Comprehensive across the supervisor surface that
  landed in slice 010.
- **P3.** Black-box trace assertions, no private snapshots.
  Right shape for an integration-style proof — matches the
  slice 003/004/005 `tests/<slicename>.rs` pattern, not slice
  006/008's lib.rs unit-test pattern.
- **P4.** User-space address refresh from trace events is
  consistent with slice 007's "logical naming is user-space"
  commitment. The proof intentionally does not invent a runtime
  API for replacement-address discovery.
- **P5.** Workload exercises `RestartableSpawnSpec<I>` factory
  captures (slice 008) in a non-trivial way. Worker creation
  via `move || Worker::new(state)` is the canonical pattern.
- **P6.** Repeated-run determinism is asserted (spec line
  103-104). Standard pattern carried forward from earlier
  slices.
- **P7.** Negative-space discipline is broad: no I/O, no
  simulator, no public replacement-address API, no logical-name
  registry, no timed budget windows, no cross-shard, no new
  scheduler behavior, no examples that aren't assertion-backed.
- **P8.** Spec is honest that this proof is not the only
  evidence for supervision correctness — generated-history
  property tests and focused unit tests remain the primary
  semantic surface (line 78-80). Avoids the common mistake of
  letting one big workload test stand in for narrow tests.

### Negative conformance review

Judgment: passes with four things to pin

- **N1. The user-space address-table ownership is unstated.**
  Spec line 30 says "user-space address table owned by the
  proof workload," but doesn't say whether the dispatcher
  *reads* the table (shared `Rc<RefCell<...>>` between test
  and dispatcher) or whether the test passes addresses
  directly to the dispatcher in each `RouteTo` message. Plan
  implies the former at line 99. Both work; the latter is
  simpler. Recommend pinning that the dispatcher receives
  addresses directly via `RouteTo { worker: Address<WorkerMsg>,
  task }` — keeps the dispatcher pure and the test's address
  table stays test-side.
  *Class: agent-resolvable.*

- **N2. Spec's "public trace data" framing is loose.** Spec
  line 112-113 says "the proof may update a user-space address
  table from public trace data in this slice." "Public trace
  data" here means available via `CurrentRuntime::trace()` to
  test code. Real apps should not parse trace event IDs because
  IDs are not stable across runtime versions and trace event
  variants will grow in future slices. Add a one-liner: "this
  address-refresh pattern is test-only; production code should
  use a registry isolate or wait for a future address-refresh
  API."
  *Class: agent-resolvable.*

- **N3. Spec verification doesn't pin dispatcher behavior with
  a stale-but-not-refreshed address.** If the test forgets to
  update `addresses[failed_index]` between the panic step and
  the next `RouteTo` message, the dispatcher sends to the
  closed old address. Runtime returns `SendRejected{Closed}`.
  Worth pinning: "tests may observe `SendRejected{Closed}` if
  they send to a stale address before refreshing the table;
  the runtime does not auto-route to replacements."
  *Class: agent-resolvable.*

- **N4. OneForAll mid-flight messages are unspecified.** Under
  OneForAll, when one worker panics, all siblings are stopped
  and restarted. Messages buffered in sibling mailboxes get
  drained as `MessageAbandoned` during the restart. Spec
  doesn't address what the test does about this. Pin:
  "OneForAll restarts cause sibling workers' mailbox contents
  to be abandoned; tests script message ordering to make
  outcomes deterministic."
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, with two angles worth being honest about

- **A1. The first reference-shaped workload proof becomes the
  canonical example shape, whether intended or not.** Spec
  line 109-111 says "this is the first reference-shaped
  workload proof package for Mariner." Downstream developers
  will copy its patterns. If the test demonstrates a
  `Rc<RefCell<Vec<Address>>>` shared between test and
  dispatcher, that's the pattern they'll copy — even though
  it's a test convenience and real apps would use a registry
  isolate or pass addresses through messages. Spec should
  explicitly say what the test's pattern *is not*: not a
  production routing pattern, not a state-sharing
  recommendation.
  *Class: human-escalation.* Affects how tina-rs feels to
  adopt and is hard to reverse once the slice ships as
  reference material.

- **A2. The "registry isolate" alternative.** Plan ambiguity 2
  asks: "Should this proof also include a tiny user-space
  registry isolate, or is a shared test-owned address table
  enough?" The shared address table is simpler. But the
  registry isolate is more faithful to how Tina-Odin / Erlang
  OTP actually do supervised dispatch — a "name server"
  isolate that holds current incarnation addresses, and clients
  send "ask name server, then send to current worker" message
  pairs.
  *Class: human-escalation.* Real product-shape question.

What I checked and found defended:

- Slice 011 doesn't add public API. The proof works against
  existing public surface (register, supervise, spawn,
  RestartableSpawnSpec, try_send, trace).
- Doesn't break any prior slice's invariants. Manual
  RestartChildren keeps slice 009 semantics; supervised
  restart keeps slice 010 semantics.
- Black-box assertions don't depend on runtime internals.
- Three-policy coverage is exhaustive of the policies that
  exist today.
- Determinism asserted via repeated-run equality.

---

## Round 2: Plan Review

Artifact reviewed:

- `.intent/phases/011-mariner-task-dispatcher-proof/plan.md`

### Positive conformance review

Judgment: passes

- **P1.** Plan correctly maps spec items to implementation:
  workload structure, isolate types, policy/budget configs,
  trace-event lookup for replacement addresses.
- **P2.** Plan correctly chooses integration test
  (`tests/task_dispatcher.rs`) over unit tests, matching the
  slice 003/004/005 pattern.
- **P3.** Plan correctly forbids: simulator vocabulary, I/O,
  public replacement-address APIs, runtime logical-name
  registry, private internals, weakened stale-address
  behavior, OneForOne sibling restart, log-based proof,
  task-dispatcher-as-only-supervision-test, mailbox unsafe.
- **P4.** Plan honestly lists 2 ambiguities at lines 254-261
  (runnable example, registry isolate). Self-aware about
  open questions.
- **P5.** Plan acknowledges whole-trace-assertion brittleness
  at line 245-247 and recommends key-subtree assertions plus
  repeated-run whole-equality. Right discipline.

### Negative conformance review

Judgment: passes with three things to tighten

- **N1. Plan does not pin the address-table-sharing
  mechanism.** Lines 99-102 mention `RouteTo { worker_index,
  task }` as one option, implying the dispatcher reads a
  shared address table by index. The alternative is `RouteTo
  { worker: Address<WorkerMsg>, task }`. Pin which. Recommend
  the latter — keeps the dispatcher pure, no shared
  Rc<RefCell> coupling between test and dispatcher.
  *Class: agent-resolvable.*

- **N2. Plan does not pin which trace events drive
  replacement-address discovery.** Plan line 58-60 says "infer
  replacement isolate id from `RestartChildCompleted`" — that
  carries `new_isolate` and `new_generation`, so the test can
  construct
  `Address::new_with_generation(shard, new_isolate, new_generation)`.
  Pin the helper shape: `fn replacement_address_for(
  failed_isolate: IsolateId, trace: &[RuntimeEvent]) ->
  Option<Address<WorkerMsg>>`.
  *Class: agent-resolvable.*

- **N3. Plan acceptance line 189 says "later
  `HandlerFinished { effect: EffectKind::Noop }` for
  replacement work" — but Worker effect contract is
  unstated.** A worker handling a task could also return
  Effect::Reply or Effect::Send. Pin: "Worker handlers return
  Effect::Noop for normal task completion (recording success
  in shared state) and panic on poison tasks. No Reply, no
  Send."
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, with two design questions

- **A1. Plan ambiguity 1 (runnable example).** Plan defers
  this. Reasonable defer for slice 011 — get the integration
  test landing first.
  *Class: human-escalation.* Touches on slice positioning.

- **A2. Plan ambiguity 2 (registry isolate).** Same as Round
  1 A2. Real product-shape question.
  *Class: human-escalation.*

What I checked and found defended:

- Plan doesn't change any public API.
- Plan correctly forbids OneForOne sibling restart (slice 010
  semantics).
- Plan uses existing test infrastructure (TestShard,
  TestMailbox, TestMailboxFactory).
- The 11-step implementation sequence is concrete and
  reviewable.
- Plan correctly notes `AddressGeneration::new(0)` for
  replacement addresses (slice 010 fresh-incarnation rule).
- Plan cites slice 010's supervised restart as proven, not
  something to re-prove.

---

## Open Decisions

Two human-escalation decisions before slice 011 is implementable
as the canonical reference workload. Both are positioning
choices about how this slice will read to future contributors
and users.

1. **Reference workload shape: shared address table vs
   registry isolate.** The slice will be the first
   reference-shaped workload for Mariner. Downstream developers
   will copy its patterns. Two options:
   - **Shared `Rc<RefCell<Vec<Address>>>` table between test
     and dispatcher** (plan's current direction): simpler test,
     but uses a state-sharing pattern that real applications
     shouldn't copy.
   - **Tiny registry isolate**: ~30 extra lines for an isolate
     that holds current worker addresses and answers
     "give me current worker N" requests. Closer to Tina-Odin
     / Erlang OTP idioms.
   Recommend the **registry isolate**, even at the ~30-line
   cost — it's the faithful pattern, and shipping the simpler
   pattern as the canonical reference will mislead future
   adopters. (Round 1 A1, Round 1 A2, Round 2 A2.)

2. **Runnable example or just integration test?** Plan defers
   this. Recommend **start with integration test only;
   promote to example in a follow-up slice if the test ends
   up tight enough**. Keeps slice 011 focused. (Round 2 A1.)

Agent-resolvable findings codex can fold autonomously:

- Pin in plan: dispatcher receives addresses directly via
  `RouteTo { worker: Address<WorkerMsg>, task }`; no shared
  address table between dispatcher and test. (Round 1 N1,
  Round 2 N1.)
- Pin in spec: "this address-refresh pattern is test-only;
  production code should use a registry isolate or wait for
  a future address-refresh API." (Round 1 N2.)
- Pin in spec verification list: "tests may observe
  `SendRejected{Closed}` if they send to a stale address
  before refreshing the table; the runtime does not
  auto-route." (Round 1 N3.)
- Pin in spec: "OneForAll restarts cause sibling workers'
  mailbox contents to be abandoned; tests script message
  ordering to make outcomes deterministic." (Round 1 N4.)
- Pin in plan: helper shape
  `fn replacement_address_for(failed_isolate: IsolateId,
  trace: &[RuntimeEvent]) -> Option<Address<WorkerMsg>>`.
  (Round 2 N2.)
- Pin in plan: "Worker handlers return Effect::Noop for
  normal task completion and panic on poison tasks. No
  Reply, no Send." (Round 2 N3.)

If you take the two human-escalation decisions and codex folds
the six agent-resolvable items, slice 011 is ready to implement
under autonomous bucket mode.

---

## Round 3: Implementation Review

Artifacts reviewed:

- `tina-runtime-current/tests/task_dispatcher.rs`
- `tina-runtime-current/examples/task_dispatcher.rs`
- `.intent/phases/011-mariner-task-dispatcher-proof/spec-diff.md`
- `.intent/phases/011-mariner-task-dispatcher-proof/plan.md`

### Review findings

Judgment: did not yet pass

- **F1. The dispatcher proof was routing around the dispatcher.**
  The implementation's first draft had the dispatcher only
  handling `SpawnWorker` while the harness and example sent
  `RegistryMsg::Forward` directly to the registry. That proved
  "registry-forwarded work survives restart," not the intended
  "dispatcher-routed work survives restart." This was a real
  artifact-vs-implementation gap and needed code changes, not
  wording-only cleanup.
- **F2. The registry silently dropped work for missing slots.**
  A missing `slot -> Address` entry returned `Effect::Noop`.
  That let work disappear in the first user-facing reference
  workload, which is the opposite of the proof discipline this
  slice is meant to establish.

### Session A decision

Resolve both in the active slice.

- Change the workload shape so clients submit tasks to the
  dispatcher. The dispatcher delegates slot resolution to the
  registry isolate with `DispatcherMsg::Submit { slot, task }`.
- Change missing-slot behavior from silent `Noop` to a loud
  panic in the registry isolate, and add a proof test that the
  failure is visible and completes no work.
- Update the spec and plan so they match the corrected shape.

---

## Round 4: Final Implementation Review

Artifacts reviewed:

- `tina-runtime-current/tests/task_dispatcher.rs`
- `tina-runtime-current/examples/task_dispatcher.rs`
- `tina-runtime-current/tests/runtime_properties.rs`
- `tina-runtime-current/tests/runtime_properties.proptest-regressions`
- `.intent/phases/011-mariner-task-dispatcher-proof/spec-diff.md`
- `.intent/phases/011-mariner-task-dispatcher-proof/plan.md`

### Final judgment

Judgment: passes

What landed:

- The dispatcher is now the actual task ingress for the
  workload. Clients send `DispatcherMsg::Submit`, the
  dispatcher sends `RegistryMsg::Forward`, and the registry
  resolves the current worker address.
- Missing registry slots now panic visibly instead of silently
  dropping work.
- The integration test covers `OneForOne`, `OneForAll`,
  `RestForOne`, budget exhaustion, stale-address `Closed`, and
  repeated-run determinism.
- The runnable example mirrors the same workload honestly.
- `runtime_properties.rs` now includes generated dispatcher
  workloads and a replay-style proof that reconstructs worker
  completions, panics, stops, and replacements from the trace
  alone and checks them against live completed-work evidence.
- The new proptest regression seeds are checked in so future
  runs replay the discovered edge cases first.

Notable implementation note:

- The generated dispatcher workload immediately found a bad
  harness assumption during development: registry refresh was
  assuming `runtime.step() == 1`, but after a panic the
  replacement worker can also run in that round. The harness
  was corrected to assert the real invariant instead. This is
  exactly the kind of proof-tightening this slice was meant to
  add.

Verification observed green:

- `cargo test -p tina-runtime-current --test task_dispatcher`
- `cargo run -p tina-runtime-current --example task_dispatcher`
- `cargo test -p tina-runtime-current --test runtime_properties`
- `make verify`
