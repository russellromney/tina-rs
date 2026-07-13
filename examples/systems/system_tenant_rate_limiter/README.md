# system_tenant_rate_limiter

Edge-service rate limiter where every tenant owns its own token bucket.

The specimen proves two truths the admission policy layer cares about:

1. **Replayable policy time.** `retry_after` is a pure function of `(rate,
   burst, now, key history)`. The live gateway supplies owner time from
   `call.now()`; simulator tests use the same API with virtual time.
2. **Hot tenant cannot starve cold tenants.** A tenant that exhausts its
   bucket sees `Limited { retry_after }`; a different tenant arriving at
   the same moment still gets `Ok`. Pressure stays per-key.

What it does not do:

- No retry. The reply is `Ok` or `Limited { retry_after }`. The caller
  decides whether to sleep and try again; the gateway never retries.
- No growing key map. The tenant table is fixed-capacity; the third
  distinct tenant on a `max_tenants=2` configuration is rejected with a
  typed `TenantCapacityFull` reply.
- No background sweeper. State drops with the runtime.

Run it:

```sh
cargo test --manifest-path examples/systems/system_tenant_rate_limiter/Cargo.toml
```

## Findings

What felt good: `try_admit_at(&tenant, call.now())` borrows the key
(allocation-free hot path), names the explicit logical-time boundary, and
keeps timestamp authority with the gateway owner. The same call is
deterministic under simulator virtual time.

The narrow `RateLimitDecision::{Admitted, RateLimited, KeyCapacityFull,
Closed}` match contains only outcomes this policy can produce. `Admitted`
has no payload because the token is already consumed and nothing remains to
release.
