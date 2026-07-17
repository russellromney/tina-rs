# Session Auth

Sharded session table with a recurring expiry sweep, hosted on
`LocalMultiShardSystem` with one split-service bucket per shard.

`Login` mints a token, picks the owning shard via `ShardPlacement`, and
calls that shard's bucket through
`LocalMultiShardSystem::call_blocking_request`. `Touch` and `Logout`
route the same way. Session idle timestamps use owner-provided time
(`call.now()` / `ctx.now()`), so the same bucket logic runs under live
wall time and simulator virtual time. A recurring sweep, driven by
runtime-owned `sleep(...).then_service_event`, walks each shard's
bucket every tick and expires rows older than the idle timeout. Every
bucket has a hard cap; overflow replies `Full`.

`RunConfig::validate` rejects zero, oversized, and non-convertible
shard counts before any worker thread or mailbox exists. Public runners
always consume the owner through `run_to_shutdown_reported`.

There is no router or tracker isolate. The host routes by placement
and calls the right shard directly.

## Run

```bash
cargo run --manifest-path examples/systems/system_session_auth/Cargo.toml
cargo test --manifest-path examples/systems/system_session_auth/Cargo.toml --test public_smoke public_smoke -- --exact
cargo test --manifest-path examples/systems/system_session_auth/Cargo.toml --test public_smoke public_characterization -- --exact
cargo test --manifest-path examples/systems/system_session_auth/Cargo.toml --all-targets
```

## What This Pulls On

- `LocalMultiShardSystem` as the canonical multi-shard live host.
- Checked shard conversion (`usize` → contiguous `u32` shard ids).
- `ShardPlacement::owner_for_str` for keyed routing from token string
  to shard id.
- `LocalMultiShardSystem::call_blocking_request` for host-driven
  login/touch/logout/stats calls. Exact `Full`, `Closed`, `Timeout`,
  and `Rejected` host outcomes stay distinct.
- Owner-provided time for login/touch stamps and sweep expiry.
- Runtime-owned `sleep(...).then_service_event` for the recurring
  sweep, with timer dependency failures counted and re-armed.
- `register_split_service_with_bootstrap_on` to start each bucket's
  sweep without a public service-envelope alias.
- `run_to_shutdown_reported` so workload and terminal shutdown failures
  remain separate.

## Findings

What felt good:
- Per-shard buckets still read as one isolate per shard, host picks
  the owner, host calls the right address.
- Owner time makes idle expiry deterministic under the simulator with
  `advance_time`, matching live semantics without wall sleeps in sim.
- Validated config fails closed before topology construction.

What felt rough:
- The sweep handler walks the local bucket each tick; the timer is
  re-armed after the handler returns, so a long handler skews the next
  tick. Fine for sessions, would matter for sub-millisecond cadence.

Verdict:
- keep
