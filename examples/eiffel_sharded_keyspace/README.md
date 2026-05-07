# eiffel-sharded-keyspace

Tokio-vs-Tina paired sharded keyspace.

Both sides run the same script (`SET / GET / DEL / SUM / QUIT`) over a 3-shard
keyspace and produce the same [`Report`]. The script is in `src/lib.rs` as a
constant; `expected_report()` enumerates the expected counts.

## What this compares

`tokio_impl.rs` is what most "sharded" Tokio code actually looks like:

```rust
let shards: Vec<Arc<Mutex<HashMap<String, String>>>> = …;

// caller is responsible for placement on every operation
let i = fnv1a64(key.as_bytes()) as usize % shards.len();
shards[i].lock()?.insert(key, value);
```

The shard concept is just an array index. Nothing prevents a caller from
writing to the wrong shard — there is no per-shard owner, no typed
`WrongShard`, and no enforcement.

`tina_impl.rs` uses the phase-053 sharded primitives:

```rust
let placement = ShardPlacement::new("eiffel-sharded-keyspace", shard_ids)?;

// One Store isolate per shard; the table is built straight from the
// placement via the registration closure. No manual entries vec, no
// .clone() dance.
let table = ShardServiceTable::try_from_placement(placement.clone(), |shard| {
    runtime.register_with_capacity_on::<Store, _>(shard, Store::new(), 32)
})?;

// Driver routes keyed requests through the table:
call(table.address_for_str(&key), StoreMsg::Set { key, value }, timeout)
    .reply(DriverMsg::StoreReturned)
```

Each shard is a real `Store` isolate that owns its own `BTreeMap`. Owner
re-check uses the shipped helper, which folds the canonical
`if owner != ctx.shard_id()` pattern into one call:

```rust
if let Err(w) = self.placement.require_owner_str(&key, ctx.shard_id()) {
    return reply(StoreReply::WrongShard(w));
}
```

A routing bug becomes a typed `WrongShard` reply, not silent corruption.

## SUM (scatter / gather)

`SUM` totals every shard's entry count. The Tokio side iterates the array;
the Tina side fans out one `call(...)` per shard and accumulates the running
total in the `Driver`. The simpler form makes the pattern legible. The
parallel `send_observed` + bridge form (with `ScatterGatherConfig` /
`ScatterGatherReport`) is proven in
`tina-runtime/tests/sharded_primitives.rs`.

## Run

```bash
cargo run -p eiffel-sharded-keyspace
cargo test -p eiffel-sharded-keyspace
```

## What this is not

This example is in-process for clarity — not a database, not remoting,
not a distributed keyspace. See the phase plan
(`.intent/phases/053-sharded-service-primitives/plan.md`) for the
non-goals.
