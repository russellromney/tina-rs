# Mailbox Capacity Truth

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
2. **Calls `A` issued via `call(...).then(...)`.** When the callee replies
   (or the call times out / fails), the runtime delivers the translated
   message into `A`'s mailbox. That message uses one slot.
3. **Observed sends `A` issued via `send_observed(...).then(...)`.** Same
   shape: the outcome (`Accepted` / `Full` / `Closed`) lands in `A`'s
   mailbox as a message.
4. **Runtime calls `A` issued (TCP, sleep, persistence, signals, etc.).**
   The completion translator turns the runtime's `CallOutput` into a
   message and that message uses one slot.
5. **Bridge ingress** (the Tokio bridge) consumes a slot per accepted
   `BridgeHandle::call(...)` request that targets `A`.

What does **not** count:

- Effects `A` returns to the runtime (`Effect::Send`, `Effect::Spawn`,
  `Effect::Io`, ...). Those are interpreted by the runtime; they do not
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
| Fanout isolate          | `max_broadcast_targets + 1` if it observes every send.            |

The `+1` slots are reserved for the isolate's own outstanding
continuations — that's the load-bearing part.

## Runtime-call continuations never drop

A runtime call (`sleep`, TCP/TLS I/O, persistence, signals) keeps a *held
resource* alive through its continuation — most sharply a bridge's poll loop,
where the `sleep().then(Poll)` self-wakeup is the only thing that ever frees
the leased slot. Dropping that continuation on a full mailbox would leak the
slot forever and walk the bridge down to `Full` for everything.

So the runtime never drops a runtime-call continuation. It tries the bounded
mailbox first; if the mailbox is full it parks the continuation in a per-
isolate **priority overflow** and emits
`RuntimeEventKind::CallContinuationOverflowed { call_kind, ... }`. The
overflow drains ahead of the mailbox, so the continuation arrives in order
with other overflowed continuations and the call still completes
(`CallCompleted`). This is a priority lane, not FIFO with ordinary mailbox
traffic: a runtime-owned liveness wakeup can run before older queued ingress
when the mailbox is saturated. The overflow is bounded by the isolate's own
outstanding runtime calls, so it cannot grow without bound.

`CallContinuationOverflowed` is a backpressure signal, not a loss: seeing it
means the mailbox is under-sized for the isolate's outstanding work, but no
continuation was lost.

## Observed-spawn lifecycle continuations

`spawn_observed(...).then(...)` / restart refresh / terminal-admission errors
are parent lifecycle facts. When the parent's bounded mailbox is full —
including when a **terminal-delivery reservation** holds the last free slot —
the runtime parks that one fact in the same **priority overflow** lane and
drains it on a later step ahead of ordinary ingress. Simulator owners force-
admit the same fact into the front of the inbox. That keeps initial success,
admission `ParentMailboxFull` / `ParentMailboxClosed`, and restart refresh
visible under reservation pressure without a hidden unbounded queue and
without changing ordinary send semantics for application traffic.

Terminal **result** delivery itself still consumes the reserved slot (or
disposes with a typed reason). Cross-shard observed delivery does not use
the overflow lane.

## Diagnosing under-capacity

When the runtime cannot enqueue a *best-effort* reply because the requester's
mailbox is full, it emits one of:

- `RuntimeEventKind::CallCompletionRejected { reason: MailboxFull, ... }`
  — an observed-send outcome (or isolate-call reply) could not land in a full
  mailbox. (Runtime-call continuations overflow instead; see above.)
- `RuntimeEventKind::CallCompletionRejected { reason: RequesterClosed, ... }`
  — the requester had already stopped or its incarnation was replaced.
- `RuntimeEventKind::SendRejected { reason: SendRejectedReason::Full, ... }`
  — a direct send hit a full target mailbox.

These events are deterministically ordered by `EventId`, so trace consumers
can ask exact questions like "did this isolate ever lose a reply?" without
ambiguity.

The `RuntimeEvent::stable_hash` method produces a content-defined fingerprint
that includes these rejection reasons, so
two replays of the same workload are guaranteed to produce the same
fingerprint when no overload happens, and a different fingerprint if any
rejection appears.

## Worked example: the chat-room miss

`examples/specimen_real_io_chat`'s first draft sized its connection mailbox
at the obvious "one slot per concurrent operation" value. Each connection
issued a burst of 64 observed sends into a fanout. The isolate could not
absorb 64 reply messages before it could finish writing its response, so
the fanout reply path saw `MailboxFull` rejections in the trace.

The current specimen uses `BroadcastTargets` / `broadcast_observed` and
sizes the connection mailbox to `max_broadcast_targets + slack`. If a
request asks for more targets than the service cap, the extra targets are
counted as visible `Full` before they become effects. Admitted targets still
produce observed-send replies, so the same capacity rule applies.

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
