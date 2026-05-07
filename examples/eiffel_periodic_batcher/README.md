# eiffel_periodic_batcher

Tokio-vs-Tina periodic batcher: items in, batches of N or every T ms
out. Common production pattern (log shipper, metrics aggregator,
Kafka producer). Tests how each runtime composes a timer with a
bounded buffer + state in a single unit.

The script is timed so both sides produce exactly the same
[`Report`]: 12 items, 2 size flushes (items 0–4 and 5–9), 1 timer
flush (items 10–11 after a quiet pause).

## Run

```sh
cargo run --manifest-path examples/eiffel_periodic_batcher/Cargo.toml -- both
cargo test --manifest-path examples/eiffel_periodic_batcher/Cargo.toml
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
`Tick(gen, SleepReply)`. The buffer, the deadline-equivalent
("is a timer in flight?"), and the size-vs-timer decision live in
plain isolate state.

Cancellation of an in-flight `sleep` is the one piece of ceremony
Tina has no built-in for: each sleep carries a `gen: u64`, and the
handler ignores any `Tick` whose generation does not match the
latest. A size flush bumps `timer_gen` so the still-pending sleep's
later `Tick` is silently discarded.

```rust
match msg {
    BatcherMsg::Submit(item) => {
        self.buffer.push(item);
        if self.buffer.len() >= BATCH_SIZE {
            self.buffer.clear();
            self.timer_gen += 1;            // invalidate pending Tick
            self.pending_timer_gen = None;
            noop()
        } else if self.pending_timer_gen.is_none() {
            self.timer_gen += 1;
            let g = self.timer_gen;
            self.pending_timer_gen = Some(g);
            sleep(self.interval).reply(move |reply| BatcherMsg::Tick(g, reply))
        } else {
            noop()
        }
    }
    BatcherMsg::Tick(g, reply) => {
        if self.pending_timer_gen != Some(g) || reply.is_err() {
            return noop();    // stale or cancelled
        }
        self.pending_timer_gen = None;
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
  to compose under `select!`; Tina's `pending_timer_gen: Option<u64>`
  is a plain field.
- **End-of-burst is a Tina message.** `BurstClosed` flows through the
  same mailbox as items. The Tokio side does the same thing via
  `rx.recv()` returning `None` after `drop(tx)`, but only because
  channel-close is a magic value at that boundary.

What feels worse:

- **Generation counter to cancel a stale timer.** Tina's `sleep(...)`
  has no cancel API: an in-flight timer always fires. The
  `(timer_gen, pending_timer_gen)` pair is a workaround. The
  classifier is small (5 lines) but the same pattern shows up
  wherever an isolate must invalidate a previously-scheduled timer.
- **Timer `Tick` carries `(u64, SleepReply)`.** Two unrelated
  payloads — the generation we want and the canonical reply alias —
  travel together. A built-in `cancellable_sleep` that returns a
  cancel handle would replace both with a typed `Cancelled` arm.

## What this suggests

The "single in-flight timer with stale-Tick filter" pattern shows up
here, in `eiffel_rate_limited_worker`, and in any isolate that uses
`sleep().reply()` to gate its own work. A small primitive — call it
`SingleSleepGate` — that owns the generation counter and offers
`schedule(duration) -> Effect` and `try_take(reply) ->
Option<TickReply>` would replace this hand-rolled bookkeeping.

(See FINDINGS Round 2 finding 5; this specimen reinforces it.)
