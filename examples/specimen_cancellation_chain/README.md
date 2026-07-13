# specimen_cancellation_chain

Mid-flight cancellation of a fan-out. The driver dispatches `FANOUT`
slow worker calls (each takes `WORK_MS` ≫ `CANCEL_AFTER_MS`), then
the host asks for cancellation before any worker has finished.

## What this teaches

Tina's first-form typed-service cancellation primitive is
`call_cancelable_request(addr, request, t).then(...)`, which returns a
caller-owned `CallHandle`, plus `cancel_call(handle).then(...)`
which closes the wait. The handle is move-only and not `Clone`: one
handle, one cancel.

Cancellation closes the *wait*, not the *work*. Workers that already
accepted their request may still finish; their replies become typed
`CallReplyRejected { CallerCancelled }` events in the trace and never
reach the driver's handler. The driver stays alive and can keep
serving other messages.

Tokio side: `JoinSet::abort_all()`. Preempts at the next await
boundary; aborted tasks never deliver.

## Run

```sh
cargo run --manifest-path examples/specimen_cancellation_chain/Cargo.toml -- both
cargo test --manifest-path examples/specimen_cancellation_chain/Cargo.toml
```

## What feels worse than Tokio

- Cancellation is bookkeeping: each pending call has a `CallHandle`
  stored by a bounded `CallGroup` keyed by worker index. The driver
  drains the group on cancel and fans the cancels back out through
  `BoundedItems` / `bounded_batch`, one cancel per stored handle. There is no one-shot
  `JoinSet::abort_all()` analogue — and there will not be: explicit
  drain-and-cancel keeps each per-call outcome typed.

## Bounded cancel storage

The driver uses `CallGroup<u32, WorkerReply>::with_capacity(FANOUT)`.
`start_cancelable` allocates a bounded, generation-stamped slot and
returns the typed continuation effect. `record_reply` and
`record_cancel` settle exactly that generation; stale or duplicate
continuations are errors rather than accidental cleanup of a reused
slot. `drain_pending_for_cancel` transfers each move-only handle to
one explicit `cancel_call` effect without growing past `FANOUT`. The
driver emits its report only after `CallGroup::report_ready()` says every
reply and cancel fact is recorded; no host delay or finish message stands
in for settlement.

## What feels better

- Late replies are visibly accounted for. Every worker reply that
  arrives after cancellation produces a typed
  `CallReplyRejected { CallerCancelled }` event. No silent task-leak.
- The driver does not have to stop itself to cancel its calls.
  `cancel_call(handle)` closes one wait without killing the isolate.
- `CancelOutcome` is `#[must_use]`: ignoring the truth ("did the
  cancel reclaim a wait, or was the call already done?") is a
  compile-time lint.

## Findings touched

- See FINDINGS finding 8 (external cancellation API) — Tina now ships
  the first-form primitives (`call_cancelable_request` + `cancel_call`).
