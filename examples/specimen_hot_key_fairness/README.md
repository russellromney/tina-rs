# specimen_hot_key_fairness

Sharded keyspace under skewed traffic: 30 writes to one hot key,
4 writes each to two cold keys. Each shard has a 4-slot mailbox and
processes at one write per 5 ms. The hot shard must reject some
writes; the cold shards stay responsive.

## Run

```sh
cargo test --manifest-path examples/specimen_hot_key_fairness/Cargo.toml
```

Both sides:

- Tokio: bounded `mpsc::channel(SHARD_MAILBOX)` per shard, worker
  task per shard sleeping `PER_WRITE_MS` per item. Producer uses
  `try_send`.
- Tina: `Store` isolate per shard, mailbox cap = `SHARD_MAILBOX`,
  rate-limited via `sleep().then(Tick)`. Producer uses one
  `HostBurstOutcomes` per shard plus `runtime.try_send_outcome` —
  the typed snapshot reports `admitted` / `mailbox_full` /
  `ingress_full` per shard with no observer closure.

The smoke test asserts:

- every burst write is accounted for;
- the hot shard rejected at least one write (overload was visible);
- the cold shards admitted everything (fairness held).

## What feels good

- Per-shard pressure surfaces at the producer. The host reads
  `HostBurstOutcomes::snapshot()` per shard and gets typed counts
  for free; no Arc-cloned counters, no manual barrier loop.
- `send_observed_until` carries the `Drain(admitted)` control
  message through the same bounded data mailbox without a hand-
  rolled retry loop.
- Cold shards are unaffected by the hot shard's overload. The
  bounded mailbox is per-isolate, not global.

## What feels worse

- The `Drain(admitted)` envelope is still domain-specific. Each
  store needs a `Drain` variant to know when it has fully drained
  its admitted backlog. The same shape shows up in
  `specimen_graceful_pool_shutdown` — every long-lived isolate that
  the host wants to stop gracefully has to define its own
  control message. See FINDINGS finding 9 (drain helper for
  `PendingReplies` at service stop) for the related product gap.
