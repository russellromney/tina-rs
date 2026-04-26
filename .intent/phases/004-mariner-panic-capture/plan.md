# Plan: 004 Mariner Panic Capture

## Goal

Make one handler panic a deterministic runtime event in `CurrentRuntime`
instead of a whole-round unwind, without introducing supervision or restart
behavior yet.

## Decisions

- This slice changes `CurrentRuntime` only. `SingleIsolateRunner` stays as the
  narrow trace-core helper from slice 001.
- `CurrentRuntime` catches handler panics at the dispatcher boundary with
  `catch_unwind`, but only around the handler call itself, not around the whole
  dispatcher arm.
- Panic capture uses `AssertUnwindSafe` at the call site. This slice does not
  add `UnwindSafe` bounds to `Isolate`, `Shard`, or runtime registration APIs.
- A panicking handler records `HandlerPanicked` instead of `HandlerFinished`.
- After `HandlerPanicked`, the runtime immediately marks the isolate stopped,
  closes its mailbox, records `IsolateStopped`, and drains buffered messages as
  `MessageAbandoned`.
- In the panic path, `IsolateStopped` has exactly one cause:
  `HandlerPanicked`.
- `MessageAbandoned` keeps the slice-003 rule: each abandonment event has
  exactly one cause, the matching `IsolateStopped`.
- A panic does not abort the whole step round. Later isolates in registration
  order still get their one delivery chance in that same round.
- This slice does not record panic payloads in the trace. The trace carries
  runtime facts only.
- This slice preserves the existing programmer-error panic for sends to unknown
  isolate IDs.
- If dropping an abandoned buffered message itself panics, that panic still
  escapes. This slice only captures handler unwinds, not destructor panics
  during cleanup.
- Panic capture in this slice depends on unwinding panics. `panic = "abort"`
  stays out of scope and is documented rather than simulated.

## Scope

1. Add `HandlerPanicked` to `RuntimeEventKind`.
2. Update `CurrentRuntime::step()` so handler invocation is panic-safe and
   panic turns into runtime state changes and trace events instead of unwinding
   out of `step()`.
3. Reuse the existing stop-and-abandon path after panic so buffered messages
   are drained and traced consistently.
4. Add integration tests in `tina-runtime-current/tests/` for panic capture,
   abandonment-after-panic, same-round continuation, later-send rejection,
   preserved unknown-target panic, and deterministic repeated runs.

## Acceptance

- A panic in one handler does not unwind out of `CurrentRuntime::step()`.
- The trace shows `MailboxAccepted -> HandlerStarted -> HandlerPanicked ->
  IsolateStopped` for the failed isolate.
- `IsolateStopped` in the panic path is caused by `HandlerPanicked`.
- Buffered messages in the failed isolate's mailbox become
  `MessageAbandoned` in FIFO order.
- A later isolate in the same round still runs after an earlier isolate
  panics.
- Later sends to the failed isolate become `SendRejected { Closed }`.
- A send to an unknown isolate ID still panics instead of becoming a traced
  handler failure.
- Two identical runs that include the same panic produce identical traces and
  causal links.
- `make verify` passes.

## Traps

- Do not introduce restart behavior, restart budgets, or parent/supervisor
  notification in this slice.
- Do not swallow panic by quietly returning `EffectObserved`; panic must become
  its own runtime event.
- Do not widen the catch scope to the whole dispatcher arm, or unknown-target
  and other programmer-error panics will be misclassified as handler failures.
- Do not add public `UnwindSafe` bounds just to make panic capture compile.
- Do not carry panic payload strings or arbitrary panic objects into the trace.
- Do not let one panic abort the rest of the round.
- Do not change explicit `Effect::Stop` semantics while adding panic handling.
- Do not invent a request/reply caller model in this slice.
- Do not invent a runtime-owned mailbox factory or execute `Spawn` in this
  slice.

## Verify

```bash
make verify
```
