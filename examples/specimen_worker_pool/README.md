# specimen_worker_pool

Frontend captures a `DeferredReply` per client, dispatches to the
next worker round-robin, replies through the matching slot when the
worker finishes. Workers have varied work times so replies arrive
out of order.

Primitives used:

- `PendingReplies::park_request` — consume the original request
  authority and capture its caller as a one-shot deferred slot.
- `PendingReplies::take(qid)` — pull the slot back when the worker
  reply matches.
- `reply_to(slot, value)` — answer the original caller through the
  captured slot.
- `BoundedItems::try_from_iter` / `bounded_batch` — cap the driver
  burst before per-item call effects exist.

## Run

```sh
cargo test --manifest-path examples/specimen_worker_pool/Cargo.toml
```

The smoke test asserts every client got the right reply: `payload +
worker_id`. The dispatch is round-robin so the test knows which
worker each client mapped to.

## What feels good

- One `PendingReplies::with_capacity(MAX_PENDING)` field is the
  whole pending box. No `Arc<Mutex<HashMap>>`, no eviction logic.
- Out-of-order completion is invisible at the call sites — the slot
  carries the correlation, not the timing.
- Pending-table pressure and every worker terminal outcome remain
  distinct: `PendingFull`, `WorkerFull`, `WorkerClosed`,
  `WorkerTimeout`, and `WorkerRejected(reason)` are not coalesced.
- The driver workload passes through a producer-owned cap aligned with
  the frontend pending cap before it becomes a call batch, so this
  specimen does not teach a raw request-sized `Effect::Batch` as the
  copied path.

## What feels worse

- The frontend has to thread `qid` through a closure passed to
  `call_request(worker, ..., timeout).then(move |outcome| FrontendEvent::WorkerDone(qid, outcome))`.
  The closure-form `.then` is the price for stuffing a correlator into
  the continuation event.
- The `FrontendRequest::Submit` / `FrontendEvent::WorkerDone` enums
  still carry the message shape; a sugar that hides "id-correlated
  dispatch" behind one helper would help.
