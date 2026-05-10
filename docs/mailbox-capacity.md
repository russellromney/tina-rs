# Mailbox Capacity Truth

Phase 047 Rock 2.

This page is the load-bearing rule for sizing isolate mailboxes in Tina:

> **Runtime-call replies, isolate-call replies, and observed-send replies
> all land in the requester's mailbox.** Capacity is the maximum number of
> messages an isolate can hold before backpressure kicks in. That maximum
> is *inbound traffic plus outstanding continuations*, not just inbound
> traffic.

If a user expects an `Address` to behave like a Tokio `mpsc::Sender`, they
will undersize the mailbox. The reply-as-message rule is what makes the
Tina trace stay clean — every continuation is an ordinary later-turn
message, not a hidden notification.

## What counts against an isolate's capacity

For an isolate `A` with mailbox capacity `cap`:

1. **Direct sends to `A`.** Every `send(addr_A, ...)` from any other
   isolate consumes one slot until `A` handles it.
2. **Calls `A` issued via `call(...).reply(...)`.** When the callee replies
   (or the call times out / fails), the runtime delivers the translated
   message into `A`'s mailbox. That message uses one slot.
3. **Observed sends `A` issued via `send_observed(...).reply(...)`.** Same
   shape: the outcome (`Accepted` / `Full` / `Closed`) lands in `A`'s
   mailbox as a message.
4. **Runtime calls `A` issued (TCP, sleep, persistence, signals, etc.).**
   The completion translator turns the runtime's `CallOutput` into a
   message and that message uses one slot.
5. **Bridge ingress** (the Tokio bridge) consumes a slot per accepted
   `BridgeHandle::call(...)` request that targets `A`.

What does **not** count:

- Effects `A` returns to the runtime (`Effect::Send`, `Effect::Spawn`,
  `Effect::Call`, ...). Those are interpreted by the runtime; they do not
  re-enter `A`'s own mailbox.
- Messages `A` sends to *other* isolates. They go in the *target's*
  mailbox.

## Sizing rules of thumb

| Role                    | Suggested capacity                                                |
| ----------------------- | ----------------------------------------------------------------- |
| Listener isolate        | `accept_burst + 1` — the +1 is the next `tcp_accept` reply.       |
| Connection isolate      | `1 (incoming start) + max_pending_calls + max_pending_observed`. |
| Store isolate (callee)  | `expected_concurrent_callers + 1`.                                |
| Worker pool member      | `1` per concurrent job + 1 for the bootstrap message.             |
| Fanout isolate          | `fanout_width + 1` if it observes every send.                     |

The `+1` slots are reserved for the isolate's own outstanding
continuations — that's the load-bearing part.

## Diagnosing under-capacity

When the runtime cannot enqueue a reply because the requester's mailbox is
full, it emits one of:

- `RuntimeEventKind::CallCompletionRejected { reason: MailboxFull, ... }`
  — the requester's mailbox was full when the runtime tried to deliver a
  call completion.
- `RuntimeEventKind::CallCompletionRejected { reason: RequesterClosed, ... }`
  — the requester had already stopped or its incarnation was replaced.
- `RuntimeEventKind::SendRejected { reason: SendRejectedReason::Full, ... }`
  — a direct send hit a full target mailbox.

These events are deterministically ordered by `EventId`, so trace consumers
can ask exact questions like "did this isolate ever lose a reply?" without
ambiguity.

The `RuntimeEvent::stable_hash` method (Phase 047 Rock 3) produces a
content-defined fingerprint that includes these rejection reasons, so
two replays of the same workload are guaranteed to produce the same
fingerprint when no overload happens, and a different fingerprint if any
rejection appears.

## Worked example: the chat-room miss

`examples/specimen_real_io_chat`'s first draft sized its connection mailbox
at the obvious "one slot per concurrent operation" value. Each connection
issued a burst of 64 `send_observed(...).reply(...)` calls into a fanout.
The isolate could not absorb 64 reply messages before it could finish
writing its response, so the fanout reply path saw `MailboxFull`
rejections in the trace. The fix was to size the connection mailbox at
*64 + the small inbound traffic budget*, which is exactly the
"reply slots count against the caller" rule above.

## Why this is the rule

Tina's promise is that effects are the only language between user code and
the runtime. Replies have to enter the user's program *somewhere* without
becoming a second language; they enter as ordinary messages. That choice
makes capacity a function of in-flight outstanding work as well as
incoming traffic. That is the price of a single trace truth.

## Related

- [`tina_runtime::DefaultMailboxFactory`] / [`DefaultThreadedMailboxFactory`]
  — blessed bounded in-process factories. Capacity stays explicit at
  registration time.
- [`tina_runtime::observe_isolate_complete`] / `observe_operation_done`
  — typed waiters that *do not* count against any isolate's mailbox; they
  are bounded one-slot side observers owned by the host.
- [`tina_runtime::send_and_observe`] — strict, message-recoverable
  ingress that distinguishes mailbox `Full` from `Closed` outcomes.

[`tina_runtime::DefaultMailboxFactory`]: https://docs.rs/tina-runtime
[`DefaultThreadedMailboxFactory`]: https://docs.rs/tina-runtime
[`tina_runtime::observe_isolate_complete`]: https://docs.rs/tina-runtime
[`tina_runtime::send_and_observe`]: https://docs.rs/tina-runtime
