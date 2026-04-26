# 004 Mariner Panic Capture

## In plain English

Right now, if an isolate handler panics inside `CurrentRuntime`, the panic
escapes out of `step()` and tears down the whole round.

That is too early to build supervision on top of. Before the runtime can
restart children or apply restart budgets, it has to be able to catch one
isolate failure, record it in the trace, stop that isolate, and keep the rest
of the shard behaving deterministically.

This slice makes handler panic a runtime event instead of an uncontrolled
unwind.

## What changes

- Extend `CurrentRuntime` in `tina-runtime-current` so handler invocation is
  wrapped in panic capture.
- Add a new runtime trace event, `HandlerPanicked`, emitted when a handler
  unwinds instead of returning an `Effect`.
- When a handler panics, the runtime marks that isolate stopped, closes its
  mailbox, records `IsolateStopped`, and drains any already-buffered messages
  as `MessageAbandoned`.
- Panic capture applies only to the handler call itself. It does not turn other
  runtime or programmer-error panics into `HandlerPanicked`.
- After one isolate panics, the runtime continues the same step round for later
  isolates in registration order.

## What does not change

- This slice does not introduce supervision policy execution, restart behavior,
  restart budgets, or parent notification.
- This slice does not execute `Reply`, `Spawn`, or `RestartChildren`.
- Explicit `Effect::Stop` behavior from slice 003 stays the same.
- `MessageAbandoned` still carries runtime facts only, not user payloads.
- The runtime trace still uses one causal link per event.
- `IsolateStopped` in the panic path is still caused by exactly one earlier
  event: `HandlerPanicked`.
- Sends to an unknown isolate ID remain a programmer-error panic, not a runtime
  trace event.
- This slice does not add `UnwindSafe` bounds to public traits or runtime
  registration APIs.
- If dropping an abandoned buffered message itself panics, that panic still
  escapes. This slice only captures handler unwinds.
- Panic capture depends on unwinding panics. With `panic = "abort"`, process
  abort remains out of scope for this slice.
- No Tokio poll loop yet.
- No cross-shard routing yet.
- No mailbox semantics changes.
- `SingleIsolateRunner` stays the narrow trace-core helper from slice 001.

## How we will verify it

- A panicking handler produces `MailboxAccepted`, `HandlerStarted`,
  `HandlerPanicked`, and then `IsolateStopped` for that isolate.
- `IsolateStopped` in the panic path is causally linked to
  `HandlerPanicked`.
- A panicking isolate is stopped exactly once and does not run later messages.
- Buffered messages left in the panicking isolate's mailbox are drained in FIFO
  order and traced as `MessageAbandoned`.
- The `MessageAbandoned` events remain causally linked to `IsolateStopped`.
- Later isolates in the same step round still run in registration order after
  an earlier isolate panics.
- Later sends to the panicked isolate become `SendRejected { Closed }`.
- Sends to an unknown isolate ID still panic instead of becoming
  `HandlerPanicked`.
- Repeated identical runs produce the same event sequence and the same causal
  links, including panic events.
