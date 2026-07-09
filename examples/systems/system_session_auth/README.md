# Session Auth

This system is a sharded session table with a recurring expiry sweep,
hosted on a real `ThreadedMultiShardRuntime` with one bucket isolate
per shard.

`Login` mints a token, picks the owning shard via `ShardPlacement`,
and calls that shard's bucket directly through
`ThreadedMultiShardRuntime::call_blocking`. `Touch` and `Logout` route
the same way using `placement.owner_for_str(&token.0)`. A recurring
sweep, driven by runtime-owned `sleep_then`, walks each shard's
bucket every tick and expires rows older than the idle timeout. Every
bucket has a hard cap; overflow replies `Full`.

There is no router or tracker isolate. The host routes by placement
and calls the right shard directly.

## Run

```bash
cargo run --manifest-path examples/systems/system_session_auth/Cargo.toml
cargo test --manifest-path examples/systems/system_session_auth/Cargo.toml
```

## What This Pulls On

- `ThreadedMultiShardRuntime` for one worker per shard.
- `ShardPlacement::owner_for_str` for keyed routing from token
  string to shard id.
- `ThreadedMultiShardRuntime::call_blocking(addr, msg, timeout)` for
  host-driven login/touch/logout/stats calls on the live owning
  shard. Bounded admission means a saturated worker command queue
  surfaces as `ThreadedRuntimeError::CommandFull`, not a hang.
- Runtime-owned `sleep_then` for the recurring sweep tick.
- `CallContext` for caller authority on every public op.
- `register_with_capacity_and_bootstrap_on` to register each shard's
  bucket with its startup `Bootstrap` message prefilled atomically,
  instead of a separate post-register `try_send`.

## Findings

What felt good:
- Per-shard buckets read like the spec: one isolate per shard, host
  picks the owner, host calls the right address. No in-isolate
  N-bucket HashMap juggling.
- Aggregating stats across shards is a tight loop of `call_blocking`
  on each address. The reply type carries one shard's slice; the host
  sums into the final `SessionStats`.
- No `Arc::try_unwrap(runtime)` shutdown dance.
- `register_with_capacity_and_bootstrap_on` removed the
  register-then-remember-to-send-Bootstrap footgun this README used to
  flag — one call now does both atomically, with a typed error if the
  prefill is rejected.

What felt rough:
- The sweep handler walks the local bucket each tick; the timer is
  re-armed after the handler returns, so a long handler skews the
  next tick. Fine for sessions, would matter for sub-millisecond
  cadence.

Verdict:
- keep
