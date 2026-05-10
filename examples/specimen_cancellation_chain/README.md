# specimen_cancellation_chain

Mid-flight cancellation of a fan-out. The driver dispatches `FANOUT`
slow worker calls (each takes `WORK_MS` ≫ `CANCEL_AFTER_MS`), then
the host asks for cancellation before any worker has finished.

## What this teaches

Tina's first-form external cancellation primitive is
`call_with_handle(addr, msg, t).reply(...)` which returns a
caller-owned `CallHandle`, plus `cancel_call(handle).reply(...)`
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
  the driver stores in a `PendingCallSet` keyed by worker index. The
  driver drains the set on cancel and fans the cancels back out as a
  `Batch`, one cancel per stored handle. There is no one-shot
  `JoinSet::abort_all()` analogue — and there will not be: explicit
  drain-and-cancel keeps each per-call outcome typed.

## What changed in 072

The driver used to keep a `Vec<CallHandle<WorkerReply>>` and
`drain(..)` it on cancel. It now uses
`PendingCallSet<u32, WorkerReply>::with_capacity(FANOUT)` — bounded
storage, typed `Full` / `DuplicateKey` errors on insert, explicit
`remove(&key)` on each completion. The cancel-all pattern is the
same drain-and-cancel that the old shape did by hand; the difference
is that the slot table cannot grow past `FANOUT`, and the cleanup
contract is now spelled out in the type rather than buried in a
`Vec` convention.

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
  the first-form primitive (`call_with_handle` + `cancel_call`).
