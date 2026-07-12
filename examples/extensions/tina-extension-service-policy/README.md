# tina-extension-service-policy

A **custom admission policy** implementing the public
`tina_runtime::ServicePolicy` seam, with only public APIs.

## The hook

`ServicePolicy` is the open extension trait for service-pressure policies:

```rust
fn decide(&mut self, key: &Self::Key, now: Instant) -> AdmissionDecision<Self::Permit>;
fn report(&self) -> AdmissionReport;
```

This crate's `PerTenantWindow` is a per-tenant fixed-window rate limiter keyed
by an external natural key (a tenant id). Its fallible `try_new` rejects a zero
limit or zero-length window before the policy can contradict its own cap.

## What it proves

- **Returns typed decisions; never acts.** `decide` returns
  `AdmissionDecision` (`Admitted` / `RateLimited { retry_after }` / `Full`). It
  never sends, spawns, sleeps, retries, or hides a queue. The caller owns any
  wait.
- **Replayable.** `decide` is a pure function of `(config, now, key history)`
  and never reads the wall clock — `now` is supplied (`ctx.now()` live, or the
  simulator on replay). The smoke test runs the same `(tenant, time)` script
  twice and asserts byte-identical decisions.
- **Bounded.** Per-key state is a fixed-capacity slot table; a new tenant when
  full is a typed `Full`, never a silent eviction.
- **Honest reports.** `report()` reflects real accumulated rejections.

## Run the smoke test

```sh
cargo test --manifest-path examples/extensions/tina-extension-service-policy/Cargo.toml
```
