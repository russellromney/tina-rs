# Plan: 003 Mariner Stop And Abandon

## Context

- Slice 002 added local same-shard `Send` dispatch with deterministic stepping
  and send-specific trace events.
- Slice 002 also preserved immediate `Effect::Stop` semantics: a stopped
  isolate never handles another message.
- The slice-002 review found one semantic gap: messages that were already
  accepted into a mailbox before that isolate stopped can remain buffered
  forever with no later handler run and no trace event explaining their fate.
- Tina's model wants accepted work to have a visible outcome. Silent stranded
  messages are too implicit for replay, simulation, and operational reasoning.

## References

- `.intent/SYSTEM.md`: current system intent and crate-boundary rules
- `.intent/phases/002-mariner-local-send-dispatch/spec-diff.md`
- `.intent/phases/002-mariner-local-send-dispatch/plan.md`
- `.intent/phases/002-mariner-local-send-dispatch/reviews.md`
- `.intent/phases/003-mariner-stop-and-abandon/spec-diff.md`
- `ROADMAP.md`
- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/local_send_dispatch.rs`
- `tina-mailbox-spsc/src/lib.rs`

## Decisions

- `Effect::Stop` stays immediate. It does not become "stop after draining."
- The handler that returned `Stop` is the last handler invocation for that
  isolate.
- After the runtime records `IsolateStopped`, it drains any already-buffered
  messages from that isolate's mailbox immediately, before any later isolate's
  handler runs in the same step round.
- Each drained buffered message produces one `MessageAbandoned` trace event.
- `MessageAbandoned` has exactly one causal link, to the matching
  `IsolateStopped` event.
- `MessageAbandoned` identifies which isolate's mailbox the message was
  abandoned from and carries no user payload.
- Abandonment is a runtime trace fact only. Senders are not notified.
- Drained buffered messages are dropped in place and never delivered to a
  handler.
- Draining order is FIFO of the mailbox contents at stop time.
- If two isolates stop in the same step round, abandonment events are
  interleaved by registration order because each isolate's drain finishes
  before the next isolate's handler runs.
- The drain loop is unbounded in event count for one step. The bound comes
  from mailbox capacity, not from the runtime.
- Sends after stop still become `SendRejected { Closed }`.
- This slice depends on the mailbox contract that `close()` does not discard
  buffered values and `recv()` continues to return them in FIFO order after
  close.

## Scope

1. Extend `RuntimeEventKind` with `MessageAbandoned`.
2. Update `CurrentRuntime` stop handling so it:
   - preserves slice-002 behavior that marks the isolate stopped, closes the
     isolate mailbox, and records `IsolateStopped`
   - drains all still-buffered messages
   - records one `MessageAbandoned` event per drained message
3. Keep slice-002 stepping rules unchanged.
4. Keep send dispatch rules unchanged.
5. Add integration tests in `tina-runtime-current/tests/` that prove:
   - buffered messages become `MessageAbandoned`
   - abandonment order is FIFO
   - abandonment is causally linked to `IsolateStopped`
   - the mailbox is empty after stop-abandon processing
   - later sends are still rejected with `Closed`
   - repeated identical runs produce identical traces

## Traps

- Do not change `Stop` into deferred draining semantics.
- Do not run handlers for abandoned messages.
- Do not add supervisor or parent-notification behavior.
- Do not carry user message payloads in trace events.
- Do not add a second causal edge from `MessageAbandoned` back to the original
  `SendAccepted`.
- Do not widen this slice into restart policy or child-stop semantics.
- Do not touch cross-shard behavior.
- Do not change mailbox backpressure rules.

## Acceptance

- A stopped isolate never handles another message.
- Any messages already buffered in that isolate's mailbox at stop time are
  drained and recorded as `MessageAbandoned`.
- `MessageAbandoned` events are caused by the matching `IsolateStopped` event.
- `MessageAbandoned` has exactly one cause: the matching `IsolateStopped`.
- `MessageAbandoned` order matches the mailbox FIFO order at stop time.
- Abandonment runs in the same step round, before any later isolate handler in
  that round.
- The stopped isolate's mailbox is empty after stop-abandon processing.
- Sends after stop still become `SendRejected { Closed }`.
- The drain loop may emit many abandonment events in one step, bounded only by
  mailbox capacity.
- Repeated identical runs produce equal event sequences and equal causal links.
- `make verify` passes.

## Out Of Scope

- Supervisor mechanism
- Parent notification
- Restart semantics
- Cross-shard routing
- Tokio poll loop
- Simulator work beyond consuming the resulting runtime events later

## Commands

- `source ~/.zshrc && make verify`
