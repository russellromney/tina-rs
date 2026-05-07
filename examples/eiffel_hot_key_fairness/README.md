# eiffel_hot_key_fairness

Sharded keyspace under skewed traffic: 60 writes to one hot key,
6 writes each to two cold keys. Each shard has a 4-slot mailbox and
processes at one write per 5 ms. The hot shard must reject some
writes; the cold shards stay responsive.

## Run

```sh
cargo test --manifest-path examples/eiffel_hot_key_fairness/Cargo.toml
```

Both sides:

- Tokio: bounded `mpsc::channel(SHARD_MAILBOX)` per shard, worker
  task per shard sleeping `PER_WRITE_MS` per item. Producer uses
  `try_send`.
- Tina: `Store` isolate per shard, mailbox cap = `SHARD_MAILBOX`,
  rate-limited via `sleep().reply(Tick)`. Producer uses
  `try_send_and_observe_with` so per-send `MailboxFull` is visible.

The smoke test asserts:

- every burst write is accounted for;
- the hot shard rejected at least one write (overload was visible);
- the cold shards admitted everything (fairness held).

## What feels good

- Per-shard pressure surfaces at the producer. The host knows which
  shard is overloaded without inspecting trace.
- Cold shards are unaffected by the hot shard's overload. The
  bounded mailbox is per-isolate, not global.

## What feels worse

- Same `Drain` + `expected` pattern as `eiffel_graceful_drain_server`
  to know when each store finishes its admitted backlog. With three
  stores, that's three `Drain` sends + three
  `observe_isolate_complete().wait(...)` calls. The closure in
  `try_send_and_observe_with` and the per-shard counter triple
  (`admitted` / `full` / `observed`) is a lot of bookkeeping for a
  fairness probe.
