# specimen_sharded_fanout_read

Tokio-vs-Tina sharded fanout read using the sharded primitives.

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

Three pieces from `tina_runtime::sharded`:

- **`ShardPlacement`** — typed name + ordered shard list. Names the
  hash scheme version so a future placement change fails loudly.
- **`ShardServiceTable<ShardCounterMsg>`** — typed `ShardId ->
  Address` map. Built directly from the placement via
  `try_from_placement(...)`.
- **`ScatterGatherConfig` / `ScatterGatherReport<u64>`** — partial-
  aggregate report shape. Covers `Replied`, `Full`, `Closed`,
  `Timeout`, `AggregateTimeout`, `MissingShard`.

The fanout uses `call` — request/reply between isolates:

```rust
ScatterCoordMsg::Start => {
    for shard in placement.shards() {
        effects.push(
            call(table.address_for(shard)?, ShardCounterMsg::Get, timeout)
                .reply(|outcome| ScatterCoordMsg::CallResult { shard, outcome })
        );
    }
    batch(effects)
}
ScatterCoordMsg::CallResult { shard, outcome } => {
    let sg = match outcome {
        CallOutcome::Replied(ShardCounterReply { value, .. }) => {
            ScatterGatherTargetOutcome::Replied(value)
        }
        CallOutcome::Timeout => ScatterGatherTargetOutcome::Timeout,
        CallOutcome::Full => ScatterGatherTargetOutcome::Full,
        CallOutcome::Closed => ScatterGatherTargetOutcome::Closed,
    };
    self.outcomes.push((shard, sg));
    if self.pending.is_empty() {
        stop_with(ScatterGatherReport { config, outcomes })
    } else { noop() }
}
```

`ShardCounter` replies directly to the caller:

```rust
ShardCounterMsg::Get => reply(ShardCounterReply {
    shard: ctx.shard_id(),
    value: self.value,
})
```

No `ReplyAdapter`, no `Bind` message, no chicken-and-egg. The coord
tracks which shards have responded with a `Vec<ShardId>`; when every
shard has replied or timed out, it publishes the report via
`stop_with(...)` and the host reads it through
`observe_result`.

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
  write going undetected.
- **`call` replaces send + reply adapter.** No extra isolate
  registration, no `Bind` message, no `From` impl. The request/
  reply path is one `call(...)` effect per target.
- **Outcomes are typed for the bad case.** The happy-path here
  fills `Replied`; the typed report still reserves slots for
  `Full`, `Closed`, `Timeout`, `AggregateTimeout`,
  `MissingShard`. The user can't accidentally smuggle "a slow shard
  is the same as a missing shard" into the aggregate.

What feels worse:

- **`ScatterCoord` state is still heavier than a lock array.**
  `table`, `targets_in_order`, `pending`, `outcomes` — four
  fields for a three-shard read. The state is real because each
  target gets its own `call` with its own timeout; the aggregate
  report must preserve target order and name partial outcomes.
  Tokio's `map(lock).sum()` is shorter because it hides all of
  that.
- **No aggregate timeout enforcement.** The specimen uses
  `per_target_timeout` on each `call`, but `aggregate_timeout` is
  only a config field — the coord does not enforce it. The richer
  pressure form (per-target timer + aggregate deadline via
  `Deadline` / `PendingCallSet`) lives in
  `tina-runtime/tests/sharded_primitives.rs`.
- **`observe_result` on multi-shard runtimes.** The host waits for
  the coord to `stop_with(report)`. This is correct but the
  registration order matters: `observe_result` must be called
  *before* sending `Start`, or the result may arrive before the
  waiter is registered.

## What this is not

This example is in-process — not a database, not remoting, not a
distributed keyspace. See the phase plan
(`.intent/phases/053-sharded-service-primitives/plan.md`) for the
non-goals. The richer pressure scenarios (one shard `Full`, one
shard `Closed`, aggregate timeout, hot-key retry) are proven in
`tina-runtime/tests/sharded_primitives.rs` and `tina-sim/tests/
sharded_dst.rs`.
