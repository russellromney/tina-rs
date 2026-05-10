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

- Cancellation is bookkeeping: each pending call needs its own
  `CallHandle` stored in isolate state. The driver fans the cancels
  back out as a `Batch`, one cancel per stored handle. There is no
  one-shot `JoinSet::abort_all()` analogue — see Rock 4 of
  `.intent/phases/066-cancellation-and-deadline-model/plan.md` for
  the bounded-set helper that closes that gap.

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
