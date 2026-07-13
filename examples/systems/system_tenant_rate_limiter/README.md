# system_tenant_rate_limiter

A request-only edge gateway where each tenant owns a fixed-capacity token
bucket.

The request API carries only tenant intent. `Gateway` stamps admission with
`RequestCall::now()`, so callers cannot mint credit or regress limiter time.
It consumes
`RateLimitDecision::{Admitted, RateLimited, KeyCapacityFull, Closed}`
exhaustively and exposes the same typed vocabulary in live Tina and the
simulator.

The tests prove:

- a hot tenant is limited while a cold tenant still progresses;
- key-capacity-full and closed are distinct typed replies;
- `Admitted` means the token was already consumed, with no permit or grant
  cleanup required;
- refill follows runtime-owned time in live Tina and virtual time in the sim;
- the request service produces byte-identical decisions across simulator
  replays and seeds;
- invalid zero, oversized, and overflowing configurations fail before runtime
  construction or request-sized allocation;
- host/runtime failures and every `CallOutcome` remain available in typed run
  errors; and
- `LocalSystem::run_to_shutdown_reported` retains workload and shutdown truth
  while owning bounded shutdown.

The limiter never retries, grows its key table, or runs a background sweeper.
Callers decide whether to wait after `RateLimited { retry_after }`; policy code
must explicitly decide if and when old tenant state is evicted.

```sh
cargo test --manifest-path examples/systems/system_tenant_rate_limiter/Cargo.toml --all-targets
```
