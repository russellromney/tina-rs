# specimen_idempotent_retry

A tiny outbound-edge relay that shows **bounded, caller-owned retry** with
**idempotency named in the message**.

The point Phase 118 cares about: when a downstream replies `Full`, the
relay does not retry on its own. It consults an explicit
`tina_runtime::FullHandling` budget (built from a `tina::time::Backoff`),
and retry is only safe because the request carries an `idempotency_key`
the caller stamped. The downstream dedups on that key, so a retry is the
same logical operation — never a double-charge.

What it proves:

- Retry is a named budget, not a default — `FullHandling::retry_backoff`
  requires an explicit `Backoff`.
- Idempotency is the caller's claim, named on `Deliver { idempotency_key }`.
- A `Delivered` outcome charges the downstream exactly once across retries.
- Budget exhaustion is a typed `Exhausted` outcome, never a silent
  give-up, and an exhausted delivery never charges.

Run it:

```sh
cargo test --manifest-path examples/specimen_idempotent_retry/Cargo.toml
cargo run --manifest-path examples/specimen_idempotent_retry/Cargo.toml
```
