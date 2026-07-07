# system_tenant_rate_limiter

Edge-service rate limiter where every tenant owns its own token bucket.

The specimen proves two truths the admission policy layer cares about:

1. **Replay determinism.** `retry_after` is a pure function of `(rate,
   burst, now, key history)`. Two runs over the same script produce
   byte-identical `retry_after` values; sim and live make the same call.
2. **Hot tenant cannot starve cold tenants.** A tenant that exhausts its
   bucket sees `Limited { retry_after }`; a different tenant arriving at
   the same moment still gets `Ok`. Pressure stays per-key.

What it does not do:

- No retry. The reply is `Ok` or `Limited { retry_after }`. The caller
  decides whether to sleep and try again; the gateway never retries.
- No growing key map. The tenant table is fixed-capacity; the third
  distinct tenant on a `max_tenants=2` configuration is rejected with a
  typed `TenantTableFull` reply.
- No background sweeper. State drops with the runtime.

Run it:

```sh
cargo test --manifest-path examples/systems/system_tenant_rate_limiter/Cargo.toml
```

## Findings

What felt good: `try_admit(&tenant, ctx.now())` borrows the key (alloc-free
hot path) and threads time explicitly, so the same code is deterministic
under sim replay; `RateLimited { retry_after }` is exact, not approximate.

What felt rough: a policy that only ever yields `Ok` / `RateLimited` /
`TenantTableFull` still forces an exhaustive match over all
`AdmissionDecision` variants (the rest are `unreachable!()`). See the
cross-specimen "Admission and rate policy ergonomics" entry in
[`examples/FINDINGS.md`](../../FINDINGS.md).
