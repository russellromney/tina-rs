# specimen_sharded_fanout_read

Tokio-vs-Tina sharded fanout read using the phase-053 sharded
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
- **`ShardServiceTable<ShardCounterMsg>`** — typed `ShardId ->
  Address` map. Built directly from the placement via
  `try_from_placement(...)`.
- **`ScatterGatherConfig` / `ScatterGatherReport<u64>`** — partial-
  aggregate report shape. Covers `Replied`, `Full`, `Closed`,
  `Timeout`, `AggregateTimeout`, `MissingShard`. The happy-path
  specimen here only fills `Replied`, but the typed slots are
  reserved.

The fanout itself is small:

```rust
ScatterCoordMsg::Start => {
    for shard in placement.shards() {
        effects.push(send(table.address_for(shard), ShardCounterMsg::Get { reply_to: bridge }));
    }
    batch(effects)
}
ScatterCoordMsg::Reply(ShardCounterReply { shard, value }) => {
    self.outcomes.push((shard, ScatterGatherTargetOutcome::Replied(value)));
    if self.pending_targets.is_empty() {
        publish_report(ScatterGatherReport { config, outcomes });
        stop()
    } else { noop() }
}
```

Replies translate through `ReplyAdapter<ShardCounterReply,
ScatterCoordMsg, AppShard>`. The user provides one
`impl From<ShardCounterReply> for ScatterCoordMsg`; the adapter is
the shipped primitive that takes care of the address translation.
Registration uses `runtime.register_reply_adapter_on(shard,
target, capacity)`, which exists on the multi-shard runtimes
(threaded, explicit-step, and the sim). The adapter still lives
in its own bounded mailbox; the helper just removes the doubled
turbofish — the adapter type and the outbound payload type —
that registering by hand required.

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

What feels worse:

- **`ScatterCoord` is a lot of state.** `table`, `bridge`,
  `targets_in_order`, `pending_targets`, `outcomes`, `report_into`,
  plus a bind/start/reply variant trio. For a three-shard happy-path
  read, that's heavier than `[shard.lock()? for shard in shards]`.
  The richer pressure form (with `send_observed`, per-target timer,
  aggregate timer) lives in `tina-runtime/tests/sharded_primitives.rs`
  and is heavier still.
- **`ReplyAdapter` is one isolate per fanout.** It's small and the
  `From` impl is one line, but every fanout site registers an
  adapter alongside the coord. A "use this address as the reply
  channel and translate replies through `From`" sugar at registration
  time would shrink the setup.
- **`Bind` then `Start` is two messages.** The coord needs the
  reply-adapter address, and the adapter needs the coord's address
  — chicken-and-egg. The current shape sends `Bind { bridge }`
  first, then `Start`. A `register_with_self_address` hook would
  remove one variant.

## What this is not

This example is in-process — not a database, not remoting, not a
distributed keyspace. See the phase plan
(`.intent/phases/053-sharded-service-primitives/plan.md`) for the
non-goals. The richer pressure scenarios (one shard `Full`, one
shard `Closed`, aggregate timeout, hot-key retry) are proven in
`tina-runtime/tests/sharded_primitives.rs` and `tina-sim/tests/
sharded_dst.rs`.
