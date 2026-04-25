# Plan: 002 Mariner Local Send Dispatch

## Context

- Slice 001 added `tina-runtime-current` with deterministic event IDs, causal
  links, and a one-step single-isolate runner.
- Slice 001 deliberately kept all non-stop effects observed-only.
- Mariner in the roadmap requires real dispatcher behavior, explicit send
  rejection, and deterministic trace semantics on one shard.
- `SYSTEM.md` says runtime scheduling, polling, effect execution, and runtime
  traces belong in runtime crates, not in `tina`.
- The approved spec diff for this slice is to add real local same-shard `Send`
  dispatch while keeping the rest of Mariner out of scope.

## References

- `.intent/SYSTEM.md`: current system intent and crate-boundary rules
- `.intent/phases/002-mariner-local-send-dispatch/spec-diff.md`: approved
  intent change for this slice
- `.intent/phases/001-mariner-trace-core/spec-diff.md`: prior slice intent
- `.intent/phases/001-mariner-trace-core/plan.md`: prior slice execution shape
- `.intent/phases/001-mariner-trace-core/review.md`: follow-up risk about
  `EffectObserved` being too generic once real dispatch lands
- `ROADMAP.md`: Mariner proof goals and future phase boundaries
- `tina-runtime-current/src/lib.rs`: current trace core implementation
- `tina-runtime-current/tests/trace_core.rs`: current runtime test style

## Decisions

- No Tokio yet. This slice still does not add a Tokio dependency or poll loop.
- The runtime keeps an internal erased-runner registry. This is private
  implementation detail inside `tina-runtime-current`, not a new public
  extensibility point.
- Registering an isolate assigns its `IsolateId` deterministically and returns a
  typed `Address<I::Message>` for that isolate.
- `IsolateId` assignment is sequential starting from `IsolateId::new(1)` and
  increments by one per registration.
- Registration order defines stepping order.
- One runtime `step()` walks registered isolates in registration order and gives
  each isolate at most one mailbox receive/handle chance per round.
- A local same-shard `Send` enqueues into the target mailbox during dispatch.
  The target isolate runs on a later runtime step, not recursively inside the
  sender's handler.
- Missing-target `Send` is a panic in this slice. Do not silently drop it and
  do not normalize it into a traced runtime event yet.
- `Send` gets specialized dispatch trace events in this slice.
- `Reply`, `Spawn`, and `RestartChildren` remain observed and traced, not
  executed.

## Scope

1. Extend `tina-runtime-current` from a single-isolate runner to a tiny
   single-shard runtime core with internal isolate registration.
2. Add private erased-runner machinery inside `tina-runtime-current` so the
   runtime can host more than one isolate with different concrete message and
   effect payload types.
3. Add public registration API that:
   - takes isolate state plus mailbox
   - assigns a deterministic isolate ID
   - returns the typed address for that isolate
4. Add public stepping API that runs one deterministic round over all
   registered isolates in registration order, with at most one delivery chance
   per isolate per call.
5. Extend the trace model with send-specific events. At minimum cover:
   - local send dispatch attempted
   - local send accepted
   - local send rejected
   - these send-specific events carry routing facts only:
     - source isolate ID
     - target isolate ID
     - target shard ID
     - for rejection, the mailbox rejection reason (`Full` or `Closed`)
   - they do not carry user message payloads
6. Keep generic observed-only trace behavior for `Reply`, `Spawn`, and
   `RestartChildren`.
7. Execute local same-shard `Effect::Send` only.
8. Preserve slice-001 semantics:
   - `Stop` remains the only runner-level state transition already supported
   - non-send, non-stop effects are still not executed
9. Add integration tests in `tina-runtime-current/tests/` with real multiple
   registered isolates and deterministic test mailboxes.

## Traps

- Do not add runtime helpers, registry types, or scheduler abstractions to
  `tina`.
- Do not add a public `Scheduler` trait.
- Do not add a public `IsolateRegistry` extensibility surface. Keep the erased
  registry private to this crate.
- Do not use the generic `EffectObserved` event for `Send` anymore. This slice
  exists to make send execution more specific in the trace.
- Do not accidentally execute `Reply`, `Spawn`, or `RestartChildren`.
- Do not recurse directly into the target handler during `Send` dispatch. The
  target must run on a later runtime step.
- Do not make stepping order depend on hash-map iteration or any unordered
  collection.
- Do not make tests choose synthetic target addresses by hand. Tests should get
  typed addresses back from registration.
- Do not silently drop a `Send` to an unknown isolate. Panic in this slice.
- Do not use the real SPSC mailbox crate for correctness tests unless mailbox
  behavior is the point of the test.
- Do not add Tokio, async tasks, background threads, supervisor code, sockets,
  or simulator hooks "for later."

## Acceptance

- `tina-runtime-current` can register more than one isolate on one shard.
- Registering an isolate returns its typed address.
- One runtime `step()` walks isolates in registration order and gives each at
  most one delivery chance.
- A local same-shard `Send` accepted by the target mailbox leads to exactly one
  later handler invocation on the target isolate.
- A rejected local same-shard `Send` is visible in the trace and is not
  silently buffered.
- A `Send` to an unknown isolate ID panics rather than silently dropping or
  being traced as a normal runtime event.
- The trace distinguishes send dispatch attempted, send accepted, and send
  rejected.
- `Reply`, `Spawn`, and `RestartChildren` remain observed and traced, not
  executed.
- Repeated identical runs produce equal event sequences and equal causal links.
- `make verify` passes.

## Out of scope

- Supervisor mechanism
- Tokio poll loop
- TCP echo server
- Cross-shard routing
- Simulator work
- Mailbox semantics changes
- Missing-target trace events
- Bridge work
- Publish/docs/changelog work beyond what this slice needs

## Commands

- `source ~/.zshrc && make verify`
