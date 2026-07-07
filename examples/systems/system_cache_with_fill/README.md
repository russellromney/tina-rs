# Cache With Fill

This system is a tiny read-through cache with single-flight fills.

Many callers request the same cold key at once. The cache isolate starts
one upstream fill, parks admitted callers in a bounded `SharedWork` table
keyed by the cache key, and replies `Busy` to overflow instead of
creating a hidden wait queue.

It also tests stale fill handling: an invalidation during a fill replies
`Stale` to the original caller, ignores the late fill completion, and
requires a fresh fill before the key is cached.

## Run

```bash
cargo run --manifest-path examples/systems/system_cache_with_fill/Cargo.toml
cargo test --manifest-path examples/systems/system_cache_with_fill/Cargo.toml
```

## What this specimen says about Tina

The copied path is now `SharedWork`: many callers wait for one result
keyed by a cache key. `SharedWork::wait(key, call)` consumes the caller's
authority and returns a move-only ticket; `request_effect_after_shared_wait(&ticket, fill_effect)`
proves admission happened before the upstream fill is scheduled.
`SharedWork::reply_all_with(&key, ...)` settles every parked caller when
the fill returns.

What got shorter or safer:

- The service no longer hand-rolls a `HashMap<key, VecDeque<id>>` next
  to `PendingReplies` to coalesce callers.
- Caller authority is consumed exactly once; the ticket is move-only and
  cannot be forged, so the request-lane effect cannot escape admission.

What still stays explicit:

- Fill-in-flight flag and stale fill generation. `SharedWork` owns the
  parked callers only.
- The upstream call/timer (here, `sleep(fill_delay)`).
- The retry/invalidation policy.

Copied helpers this specimen uses:

- `SharedWork::with_capacity(N).named(...)` for bounded waiters;
- `SharedWork::wait(...)` for admission with caller authority returned
  on overload;
- `request_effect_after_shared_wait(&ticket, fill_effect)` for the
  request-lane effect that schedules the upstream work;
- `SharedWork::reply_all_with(&key, ...)` for fanout on fill;
- `SharedWork::close_all_clone(&key, ...)` for invalidation reply.

What not to use:

- Hand-rolled `HashMap<key, VecDeque<id>>` plus `PendingReplies` for
  multi-caller coalescing — that is what `SharedWork` exists to replace.

Remaining rough bits:

- The host-side concurrent-call script is still boilerplate-heavy for
  system specimens.
- Fill generations are still per-service truth; `SharedWork` cannot own
  staleness because the service is the one calling the upstream.

Verdict:

- keep.
