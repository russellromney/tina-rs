# specimen_dynamic_worker_pool

Tokio-vs-Tina dynamic worker pool with join-all aggregation. A coordinator
spawns 4 workers, gives each a 4-element slice of a fixed input, and joins
their partial sums into `136 = sum(1..=16)`.

## Run

```sh
cargo run --manifest-path examples/specimen_dynamic_worker_pool/Cargo.toml -- both
cargo test --manifest-path examples/specimen_dynamic_worker_pool/Cargo.toml
```

## Tokio shape

Tokio uses the standard `JoinSet` loop. Each `join_next()` yields either a
partial sum or a `JoinError`.

```rust
let mut joins: JoinSet<u64> = JoinSet::new();
for slice in chunks {
    joins.spawn(async move { slice.iter().sum() });
}
while let Some(joined) = joins.join_next().await {
    total += joined?;
}
```

## Tina shape

The coordinator bounded-batches observed child spawns. A successful spawn
returns a typed `ChildRef`; the coordinator converts that raw service address
to its request capability and calls the request-only worker.

```rust
spawn_observed(ChildDefinition::new(Worker { chunk }, 1))
    .then(CoordMsg::WorkerStarted)

let worker = SplitServiceHandle::from_address(child.address).requests;
call_request(worker, WorkerRequest::Compute, CALL_TIMEOUT)
    .then(CoordMsg::WorkerDone)
```

The worker replies and stops in one explicit effect:

```rust
call.reply_and(WorkerReply::Partial(partial), vec![stop()])
```

`WorkerStarted(Err(_))` preserves all six current spawn-rejection reasons in
distinct counters (plus a future non-exhaustive bucket). `WorkerDone` exhaustively
accounts for `Replied`, `Full`, `Closed`, `Timeout`, and all four
`CallRejectedReason` variants. Every child therefore settles exactly one report
bucket. There is no parent address in the
worker, bootstrap compute message, result side channel, child map, or missing
partial hang.

The failure smoke test injects a child handler panic. The runtime captures the
panic and the outstanding request settles as
`Rejected(CallRejectedReason::HandlerPanicked)`, so the coordinator still
reports all four workers, preserves the exact cause, and exits cleanly.

The host uses `LocalSystem::run_to_shutdown_reported`, so startup, workload,
result observation, and shutdown share one fallible application lifetime.
