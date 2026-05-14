# specimen_periodic_batcher

Tokio-vs-Tina periodic batcher: items in, batches of N or every T ms
out. Common production pattern (log shipper, metrics aggregator,
Kafka producer). Tests how each runtime composes a timer with a
bounded buffer + state in a single unit.

The script is timed so both sides produce exactly the same
[`Report`]: 12 items, 2 size flushes (items 0–4 and 5–9), 1 timer
flush (items 10–11 after a quiet pause).

## Run

```sh
cargo run --manifest-path examples/specimen_periodic_batcher/Cargo.toml -- both
cargo test --manifest-path examples/specimen_periodic_batcher/Cargo.toml
```

```
side=tokio items_seen=12 size_flushes=2 timer_flushes=1 exit_clean=true
side=tina  items_seen=12 size_flushes=2 timer_flushes=1 exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape

Canonical `tokio::select!` with two arms:

```rust
loop {
    tokio::select! {
        biased;
        item = rx.recv() => { /* push, maybe flush by size, arm timer if first */ }
        _ = wait_until(deadline) => { /* flush by timer */ }
    }
}
```

`deadline` is an `Option<Instant>`; when `None` (buffer empty), the
timer arm uses `std::future::pending::<()>()` so the recv arm is the
only thing that wakes us. The `biased` flag preserves a stable arm
order under simultaneous wake.

## Tina shape

One isolate. The two events become two messages: `Submit(item)` and
`Tick(tick_number, SleepReply)`. The buffer, the deadline-equivalent
("is a timer in flight?"), and the size-vs-timer decision live in
plain isolate state.

The old pain was the local generation counter: each sleep carried a
`gen: u64`, and a size flush manually bumped `timer_gen` so the
still-pending sleep's later `Tick` was silently discarded.

Now the interval helper owns only the timer math and tick numbering.
The user code still returns the visible sleep effect, and stale work
is still explicit:

```rust
match msg {
    BatcherMsg::Submit(item) => {
        self.buffer.push(item);
        if self.buffer.len() >= BATCH_SIZE {
            self.buffer.clear();
            self.pending_tick = None;       // invalidate pending Tick
            self.interval.clear();          // next item starts a fresh period
            noop()
        } else if self.pending_tick.is_none() {
            let decision = self.interval.next_delay(ctx.now());
            let tick = decision.tick_number();
            self.pending_tick = Some(tick);
            sleep(decision.delay()).then(move |reply| BatcherMsg::Tick(tick, reply))
        } else {
            noop()
        }
    }
    BatcherMsg::Tick(tick, reply) => {
        if self.pending_tick != Some(tick) || reply.is_err() {
            return noop();    // stale or cancelled
        }
        self.pending_tick = None;
        if !self.buffer.is_empty() {
            self.buffer.clear();
            self.report.timer_flushes += 1;
        }
        noop()
    }
    BatcherMsg::BurstClosed => stop_with(self.report),
}
```

## Discussion

What feels better:

- **No `select!`, no future to drop.** The batcher is one synchronous
  handler over an enum. Reasoning is local: every Tick arrival
  shows up as a real trace event with a stable id; nothing happens
  "between awaits."
- **State that survives timer cancellation is just isolate state.**
  Tokio's `deadline: Option<Instant>` and the timer-arm future have
  to compose under `select!`; Tina's `pending_tick: Option<u64>`
  is a plain field.
- **End-of-burst is a Tina message.** `BurstClosed` flows through the
  same mailbox as items. The Tokio side does the same thing via
  `rx.recv()` returning `None` after `drop(tx)`, but only because
  channel-close is a magic value at that boundary.

What feels worse:

- **Stale timer filtering is still user state.** Tina's `sleep(...)`
  has no cancel API: an in-flight timer always fires. `TimerInterval`
  removes the local generation arithmetic, but `pending_tick` is still
  explicit because invisible dropped work would lie.
- **Timer `Tick` carries `(u64, SleepReply)`.** Two unrelated
  payloads — the tick number we want and the canonical reply alias —
  travel together. A built-in `cancellable_sleep` that returns a cancel
  handle would replace both with a typed `Cancelled` arm.

## What this suggests

`TimerInterval` names the repeated-delay state, missed-tick policy,
and tick numbering. It deliberately does not cancel a runtime sleep or
queue work. This specimen still keeps `pending_tick` explicit so a
size-triggered flush can invalidate the already-returned sleep without
making a hidden scheduler.

A future "cancellable sleep" primitive (returning a cancel handle that
produces a typed `Cancelled` reply arm) would replace the manual Tick
filter without hiding any trace event.
