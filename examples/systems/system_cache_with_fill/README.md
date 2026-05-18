# Cache With Fill

This system is a tiny read-through cache with single-flight fills.

Many callers request the same cold key at once. The cache isolate starts one
upstream fill, parks admitted callers in a bounded `WaitList` table, and
replies `Busy` to overflow instead of creating a hidden wait queue.

It also tests stale fill handling: an invalidation during a fill replies
`Stale` to the original caller, ignores the late fill completion, and requires a
fresh fill before the key is cached.

## Run

```bash
cargo run --manifest-path examples/systems/system_cache_with_fill/Cargo.toml
cargo test --manifest-path examples/systems/system_cache_with_fill/Cargo.toml
```

## Findings

What felt good:
- The single-flight state machine is very natural Tina: one key-owned fill,
  one bounded waiter list, one completion message.
- Split service authoring now matches the domain: `CacheRequest` is the public
  callable surface, while private `CacheEvent::FillDone` is only an internal
  continuation. Host code uses `call_blocking_request`, not raw
  `ServiceMessage`.
- `WaitList::park` is the right primitive for bounded callers waiting on one
  downstream fill keyed by the cache key.
- Stale completion handling is readable when the fill carries a generation.

What felt rough:
- The stale-completion path requires discipline: every fill must carry a
  generation, and every invalidation must reply to or reclaim every waiter.
- `CallContext` is the right contract. The split-service macro now removes the
  old "put request variants in `handle`" trap for this specimen.
- The host-side concurrent-call script is still boilerplate-heavy for system
  specimens.

Tina capability pulled:
- Bounded deferred replies.
- Explicit call authority via `CallContext`.
- Keyed waiter lists.
- Split public requests from private internal events.
- Single-flight fill state.
- Runtime-owned time.
- Stale-result handling.
- Host-side concurrent calls.

Suggested follow-up:
- Consider a tiny `SingleFlight` helper only if more systems repeat the same
  `WaitList` plus fill-generation shape.
- Add host scenario helpers for "burst N call_blocking threads through a
  barrier and classify outcomes."

Verdict:
- keep
