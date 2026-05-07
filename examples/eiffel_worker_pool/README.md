# eiffel_worker_pool

Frontend captures a `DeferredReply` per client, dispatches to the
next worker round-robin, replies through the matching slot when the
worker finishes. Workers have varied work times so replies arrive
out of order.

Phase-061 primitives used:

- `Context::take_reply_slot` / `PendingReplies::try_capture` —
  capture the original caller as a one-shot deferred slot.
- `PendingReplies::take(qid)` — pull the slot back when the worker
  reply matches.
- `reply_to(slot, value)` — answer the original caller through the
  captured slot.

## Run

```sh
cargo test --manifest-path examples/eiffel_worker_pool/Cargo.toml
```

The smoke test asserts every client got the right reply: `payload +
worker_id`. The dispatch is round-robin so the test knows which
worker each client mapped to.

## What feels good

- One `PendingReplies::with_capacity(MAX_PENDING)` field is the
  whole pending box. No `Arc<Mutex<HashMap>>`, no eviction logic.
- Out-of-order completion is invisible at the call sites — the slot
  carries the correlation, not the timing.
- `Full` admission is a typed `FrontendReply::Full` reply; the
  driver bucket is distinct from a successful result.

## What feels worse

- The frontend has to thread `qid` through a closure passed to
  `call(worker, ..., timeout).reply(move |outcome| FrontendMsg::WorkerDone(qid, outcome))`.
  The closure-form `.reply` is the price for stuffing a correlator
  into the continuation message.
- `Frontend::Submit / WorkerDone` enum still carries the message
  shape; a sugar that hides "id-correlated dispatch" behind one
  helper would help.
