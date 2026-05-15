# Session Auth

This system is a tiny sharded session table with a recurring expiry sweep.

`Login` admits a session into one of N buckets selected by `ShardPlacement`.
`Touch` updates the row's last-touched time. `Logout` drops the slot. A
recurring sweep, driven by runtime-owned `sleep_then`, walks every bucket
each tick and expires rows whose last-touched timestamp is older than the
idle timeout. Every bucket has a hard cap; overflow replies `Full`.

## Run

```bash
cargo run --manifest-path examples/systems/system_session_auth/Cargo.toml
cargo test --manifest-path examples/systems/system_session_auth/Cargo.toml
```

## Findings

What felt good:
- `ShardPlacement::owner_for_str` is the right shape for routing keys to
  buckets. A `ShardId -> bucket_index` table on the side is cheap and
  obvious.
- `sleep_then(d, Msg::Sweep)` returning a `tina::Effect` from inside the
  isolate makes the recurring timer one line at the end of the sweep
  handler. No external scheduler to wire up.
- `ThreadedRuntime::call_blocking` made the smoke test read like the spec:
  login, touch, sleep, touch, assert.
- Synchronous `CallContext::reply` from `handle_call` covered every public
  op. No `PendingReplies` needed.

What felt rough:
- "Sharded" here is in-isolate. One isolate holds N HashMap buckets keyed
  by `ShardId`. The blessed cross-isolate sharded runtime is
  `ThreadedMultiShardRuntime`, which does not expose a host-side
  `call_blocking`. The cache/job-queue specimens both reach for
  `ThreadedRuntime` (single shard) for the same reason. A
  `ThreadedMultiShardRuntime::call_blocking_on(shard, addr, msg, t)` is
  the missing piece for "real sharded placement" specimens.
- Bootstrap is still a public message variant the host has to remember to
  `try_send` after register. Every system that needs a recurring timer
  pays this ceremony. A `register_with_bootstrap_message` or an
  `on_start` hook would remove a footgun (forgetting it leaves a quiet
  service with no sweep).
- The sweep handler walks every bucket every tick. There is no
  `Effect::After(Duration, Effect)` to express "do this work every N ms"
  directly. The recurring shape is `sleep_then(d, Msg::Sweep)` returned
  from the sweep handler — readable, but it does mean the timer is
  re-armed only after the handler returns, so a long handler skews the
  next tick. Fine for sessions, would matter for sub-millisecond cadence.

Tina capability pulled:
- `ShardPlacement` for keyed bucket routing.
- Runtime-owned `sleep_then` for the recurring sweep tick.
- `CallContext` for `Login` / `Touch` / `Logout` / `Stats`.
- `ThreadedRuntime::call_blocking` for host-driven scenarios.

Suggested follow-up:
- Add `ThreadedMultiShardRuntime::call_blocking_on(shard, ...)` (or a
  blessed host-side request/reply driver) so a follow-up version of this
  specimen can use real cross-shard placement.
- Consider a tiny `Bootstrap` hook on `register` so isolates with a
  startup effect (timer, supervisor spawn) do not depend on the host
  remembering to send one message after register.

Verdict:
- keep
