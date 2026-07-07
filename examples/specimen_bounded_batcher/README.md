# specimen_bounded_batcher

Many callers each `call(batcher, Submit(item))`. The batcher captures
every caller's `DeferredReply`, holds them in `PendingReplies`, and
replies to all of them with the batch total when either:

- the batch hits `BATCH_SIZE`, or
- `BATCH_TIMEOUT_MS` elapses since the first item.

## Phase-061 primitives used

- `PendingReplies::with_capacity(MAX_PENDING)` — bounded promise box.
- `Context::take_request_context` via `try_capture` — capture each
  caller as a deferred slot.
- `take(qid)` + `reply_to(slot, BatcherReply::Batched(total))` —
  flush the batch in one effect batch.
- `sleep(interval).then(move |_| Tick(gen))` with a generation
  counter — invalidate the timer when a size-flush beats it.

## Run

```sh
cargo test --manifest-path examples/specimen_bounded_batcher/Cargo.toml
```

## What feels good

- A flush is one `Effect::Batch(reply_to(slot, ...))` per pending
  caller. The batcher does not have to track addresses, type
  parameters, or correlation — `PendingReplies` owns it.
- Caller timeout is the runtime's job. If a caller's
  `call(...).then(...)` deadline fires first, the slot becomes
  `Closed`; the next `pending.sweep()` (called inside `try_capture`)
  reclaims it.

## What feels worse

- The batcher mixes "rate-limit timer" state with "pending replies"
  state. The generation counter for the timer (`timer_gen` /
  `pending_timer_gen`) handles the case where a size flush
  invalidates a still-pending Tick. `SingleCallGate` (Phase 062
  Rock 5) names "one timer in flight, plus N queued" but does not
  cover stale-tick invalidation, so it does not apply here.
- `Effect::Batch(reply_to(...))` constructs a Vec of effects per
  flush. Fine for small batches; for thousand-caller batches this
  is real allocation.
