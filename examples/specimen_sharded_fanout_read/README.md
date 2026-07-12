# specimen_sharded_fanout_read

Tokio-vs-Tina sharded fanout read using the sharded service
primitives.

Three shards each own a `u64` counter, seeded with `[100, 200, 300]`.
Both sides issue a fanout read and aggregate `total_sum=600`.

## Run

```sh
cargo run --manifest-path examples/specimen_sharded_fanout_read/Cargo.toml -- both
cargo test --manifest-path examples/specimen_sharded_fanout_read/Cargo.toml
```

Both sides:

```
side=tokio total_sum=600 shards_replied=3 exit_clean=true
side=tina  total_sum=600 shards_replied=3 exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape

```rust
let shards: Vec<Arc<Mutex<u64>>> = ...;
let total: u64 = shards.iter().map(|s| *s.lock().unwrap()).sum();
```

The "shard" is an array index. Nothing structural prevents reading
the wrong slot, splitting load across shards in a different way, or
sneaking a second writer onto a slot that the program treated as
shard-owned. The mutex serializes one shard's value, but the
aggregator is the only thing that knows it's "the sharded reader."

## Tina shape

Three first-class pieces from `tina_runtime::sharded`:

- **`ShardPlacement`** — typed name + ordered shard list. Names the
  hash scheme version so a future placement change fails loudly.
- **`ShardRequestServiceTable<ShardCounterRequest, ShardCounterReply>`** —
  typed `ShardId -> ServiceRequestAddress` map. Built from placement via
  `try_from_placement(...)`.
- **`ScatterGatherOperations` / `ScatterGatherEvent`** — one bounded owner for
  caller authority, child calls, cancellation, aggregate timeout, ordering,
  and retirement.

The fanout itself is small:

```rust
operations.start_service(request, config, targets, |counter, timeout| {
    call_cancelable_request(counter, ShardCounterRequest::Get, timeout)
}, CoordEvent::Scatter)
```

Replies, timeouts, cancellation acknowledgements, and late events use the one
`ScatterGatherEvent` vocabulary. The host calls the coordinator through its
request capability and exhaustively distinguishes coordinator Full, Closed,
Timeout, Rejected, start rejection, and partial target reports.

## Discussion

What feels better:

- **Placement is a typed object.** `ShardPlacement` carries the
  scheme + version + ordered shard list. A future change to the
  hash function fails loudly via the version field. Tokio's `i %
  shards.len()` carries no provenance.
- **Owners are isolates, not slots.** Each shard's counter is a
  `ShardCounter` isolate registered on its shard. There is no
  shared `Arc<Mutex<u64>>` for the value, no second mutator
  squeezing into the same lock, no possibility of a wrong-shard
  write going undetected (the shipped `placement.require_owner_*`
  helper makes that a typed `WrongShard`).
- **Outcomes are typed for the bad case.** The happy-path here
  fills `Replied`; the typed report still reserves slots for
  `Full`, `Closed`, `Timeout`, `AggregateTimeout`,
  `MissingShard`. The user can't accidentally smuggle "a slow shard
  is the same as a missing shard" into the aggregate.

What remains explicit:

- **Targets and time budgets are visible.** The coordinator still builds a
  bounded target list and names per-target plus aggregate deadlines. This is
  intentional pressure policy rather than reply-correlation plumbing.

## What this is not

This example is in-process — not a database, not remoting, not a
distributed keyspace. It exercises the sharded-service primitives
(per-shard placement, cross-shard fan-in) in the small. The richer
pressure scenarios (one shard `Full`, one shard `Closed`, aggregate
timeout, hot-key retry) are proven in
`tina-runtime/tests/sharded_primitives.rs` and `tina-sim/tests/
sharded_dst.rs`.
