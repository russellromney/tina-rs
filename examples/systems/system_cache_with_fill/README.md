# Cache With Fill

This system is a tiny read-through cache with single-flight fills.

Many callers request the same cold key at once. The cache isolate starts one
upstream fill, parks admitted callers in a bounded `PendingReplies` table, and
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
  explicit waiter qids, one completion message.
- `PendingReplies::try_capture` is the right primitive for bounded callers
  waiting on one downstream fill.
- Stale completion handling is readable when the fill carries a generation.

What felt rough:
- `PendingReplies` owns pending capacity, but the per-key waiter list is still
  hand-rolled. A helper for "capture caller and append qid to this wait list"
  might reduce duplication across cache/fanout/pool frontends.
- The stale-completion path requires discipline: every fill must carry a
  generation, and every invalidation must reply to or reclaim every waiter.
- `CallContext` is the right contract, but it means examples that use
  `call_blocking` must remember to put request/reply variants in `handle_call`;
  putting them in `handle` compiles but gets rejected at runtime as
  `UnsupportedMessage`.
- The host-side concurrent-call script is still boilerplate-heavy for system
  specimens.

Tina capability pulled:
- Bounded deferred replies.
- Explicit call authority via `CallContext`.
- Single-flight fill state.
- Runtime-owned time.
- Stale-result handling.
- Host-side concurrent calls.

Suggested follow-up:
- Consider a tiny `WaitList` or `SingleFlight` helper only if another system
  repeats the same `PendingReplies` plus per-key qid list shape.
- Add host scenario helpers for "burst N call_blocking threads through a
  barrier and classify outcomes."

Verdict:
- keep
