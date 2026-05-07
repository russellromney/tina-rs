# eiffel_bounded_batcher

Many callers each `call(batcher, Submit(item))`. The batcher captures
every caller's `DeferredReply`, holds them in `PendingReplies`, and
replies to all of them with the batch total when either:

- the batch hits `BATCH_SIZE`, or
- `BATCH_TIMEOUT_MS` elapses since the first item.

## Phase-061 primitives used

- `PendingReplies::with_capacity(MAX_PENDING)` — bounded promise box.
- `Context::take_reply_slot` via `try_capture` — capture each
  caller as a deferred slot.
- `take(qid)` + `reply_to(slot, BatcherReply::Batched(total))` —
  flush the batch in one effect batch.
- `sleep(interval).reply(move |_| Tick(gen))` with a generation
  counter — invalidate the timer when a size-flush beats it.

## Run

```sh
cargo test --manifest-path examples/eiffel_bounded_batcher/Cargo.toml
```

## What feels good

- A flush is one `Effect::Batch(reply_to(slot, ...))` per pending
  caller. The batcher does not have to track addresses, type
  parameters, or correlation — `PendingReplies` owns it.
- Caller timeout is the runtime's job. If a caller's
  `call(...).reply(...)` deadline fires first, the slot becomes
  `Closed`; the next `pending.sweep()` (called inside `try_capture`)
  reclaims it.

## What feels worse

- The batcher mixes "rate-limit timer" state with "pending replies"
  state. The generation counter for the timer (`timer_gen` /
  `pending_timer_gen`) is the same shape as in
  `eiffel_periodic_batcher` and `eiffel_rate_limited_worker`. See
  Round 2 finding 5: a `SingleSleepGate` helper would shrink this.
- `Effect::Batch(reply_to(...))` constructs a Vec of effects per
  flush. Fine for small batches; for thousand-caller batches this
  is real allocation.
