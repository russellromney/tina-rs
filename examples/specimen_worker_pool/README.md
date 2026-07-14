# specimen_worker_pool

The frontend dispatches each request to the next worker round-robin and moves
the caller's typed `RequestContext` into that worker call's continuation.
Workers have varied work times, so replies arrive out of order without an
application-level request ID or correlation table.

Primitives used:

- `RequestCall::defer(call_request(...))` — move the original caller authority
  into exactly one worker call.
- `reply_service_event` — deliver the typed `RequestContext` and exhaustive
  worker `CallOutcome` to the frontend continuation without exposing a
  `ServiceMessage` envelope.
- `reply_to(req, value)` — answer the original caller through that authority.
- `BoundedItems::try_from_iter` / `bounded_batch` — cap the driver
  burst before per-item call effects exist.
- `LocalSystem` — own registration, observation, typed ingress, and truthful
  terminal shutdown through `run_to_shutdown_reported` on the public
  application facade.

## Run

```sh
cargo test --manifest-path examples/specimen_worker_pool/Cargo.toml
```

The smoke test asserts every client got the right reply: `payload +
worker_id`. The dispatch is round-robin so the test knows which
worker each client mapped to.

## What feels good

- Out-of-order completion is invisible at the call sites: the move-only caller
  authority carries correlation, not an ID or sidecar map.
- Every worker terminal outcome remains distinct: `WorkerFull`,
  `WorkerClosed`, `WorkerTimeout`, and `WorkerRejected(reason)` are not
  coalesced, timer setup failures retain their typed `CallError`, and the
  driver report keeps worker and frontend terminal categories separate.
- The driver workload passes through a producer-owned cap before it becomes a
  call batch, so this specimen does not teach a raw request-sized
  `Effect::Batch` as the copied path.

The explicit `FrontendEvent::WorkerDone(RequestContext<_>, CallOutcome<_>)`
still names the suspension point and all terminal outcomes, but it contains no
authority-shaped workaround: `reply_service_event` constructs that typed
continuation directly.
