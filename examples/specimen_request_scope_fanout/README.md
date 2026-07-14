# Request Scope Fanout

A driver isolate dispatches `FANOUT` worker calls under one
`RequestScope` and cancels the scope a short while later. Every rail
that was still waiting at cancel time is closed; rails that already
replied stay replied; any worker that finishes after the cancel surfaces
its reply as a typed `CallReplyRejected { CallerCancelled }` /
`DeferredReplyRejected { CallerCancelled }` event in the trace.

The point of the specimen is to make the "request went away → these
children were cancelled → these external tasks may still finish late"
story executable, not a docs claim.

## Run

```bash
cargo test --manifest-path examples/specimen_request_scope_fanout/Cargo.toml
```

## What it shows

- `RequestScope::with_child_cap(id, FANOUT)` allocates a scope sized
  exactly for the planned fanout. No "some large number"; the cap is
  the budget.
- `scope.register("worker", handle)` consumes the typed
  `CallHandle<R>` from each `call_cancelable(...)` rail. The worker-
  return path (the `.then(Returned)` continuation) still delivers
  replies normally; the scope is just the canceller.
- `scope.cancel_into_effect(cause, translator)` produces one batched
  `Effect<Self>` with one cancel per rail. The translator routes each
  cancel ack back to the driver as `DriverMsg::ChildCancelled`.
- The synchronous `ScopeCancelReport` from that call names the cause
  and reports `cancelled_count` (waits closed) and
  `already_settled_count` (rails that already replied) separately.
- Fanout effects are built from `BoundedItems` and `bounded_batch` before any
  child call exists. Cancellation is bounded by the scope's child cap and
  emitted by `cancel_into_effect`. Actor-owned cancel/finish timers replace
  host sleeps and trace polling; the report waits for every expected cancel
  acknowledgement and retains every child call terminal, timer error, and
  exact `CancelOutcome`.

## What it does NOT show

- Worker-side cancellation. Workers run to natural completion of their
  current `sleep`; that is the honest semantics of the scope. Killing
  workers would require an application-level `Cancel` message and is
  separate from request-scoped cancellation.
- Bridge late-result columns. The trace fact is the visible truth here;
  bridges that count their own late results layer on top of that and
  are out of scope for this specimen.
