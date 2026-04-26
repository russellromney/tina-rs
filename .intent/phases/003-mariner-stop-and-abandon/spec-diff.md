# 003 Mariner Stop And Abandon

## In plain English

Right now, an isolate can stop while old messages are still sitting in its
mailbox. Those messages were accepted earlier, but they never run and the
trace never explains what happened to them.

This slice makes that outcome explicit.

When an isolate stops, the runtime should keep `Stop` immediate, but it should
also drain any leftover buffered messages, drop them, and record that they
were abandoned. That way every accepted message still has a visible ending in
the runtime model.

## What changes

- Extend `CurrentRuntime` in `tina-runtime-current` so `Effect::Stop` does one
  more explicit runtime step after `IsolateStopped`: it drains any messages
  that were already buffered in the stopped isolate's mailbox.
- Add a new runtime trace event, `MessageAbandoned`, for each drained buffered
  message.
- `MessageAbandoned` identifies which isolate's mailbox the message was
  abandoned from, but carries no user payload.
- Drop drained buffered messages in place, without delivering them to the
  stopped isolate's handler.
- Leave the stopped isolate's mailbox empty once stop-abandon processing
  completes.

## What does not change

- `Effect::Stop` is still immediate. The handler that returned `Stop` is still
  the last handler that isolate runs.
- `SingleIsolateRunner` remains the slice-001 trace core. This slice only
  refines `CurrentRuntime`.
- Registration order stepping and local same-shard `Send` dispatch from slice
  002 stay the same.
- Abandonment is a runtime trace fact only. Senders are not notified in this
  slice.
- Sends after `Stop` still become `SendRejected { Closed }`.
- No supervisor mechanism yet.
- No parent notification yet.
- No restart semantics yet.
- No Tokio poll loop yet.
- No cross-shard routing yet.
- No mailbox semantics changes.
- Trace events still do not carry user message payloads.

## How we will verify it

- A `Stop` in front of `N` buffered messages produces `N`
  `MessageAbandoned` trace events.
- The `MessageAbandoned` events are causally linked to the
  `IsolateStopped` event.
- Abandonment runs in the same step round, immediately after
  `IsolateStopped`, before any later isolate handler runs in that round.
- The `MessageAbandoned` events carry runtime facts only, not user message
  payloads.
- The abandoned-message events preserve the mailbox's FIFO order at stop time.
- If two isolates stop in the same step round, abandonment events are emitted
  in registration order because each isolate's drain finishes before the next
  isolate's handler runs.
- The drain loop is unbounded in event count per step. The bound comes from
  mailbox capacity, not from the runtime.
- Repeated identical runs produce the same event sequence and the same causal
  links, including abandonment events.
- The stopped isolate's mailbox is empty after stop-abandon processing
  completes.
- Sends to the stopped isolate still become `SendRejected { Closed }`.
