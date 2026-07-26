# specimen_multi_turn_request_context

Caller authority across turns. A `Service` answers `ServiceRequest::Start`
by running a two-step readiness check — probe, then db — where each step
defers a sleep and resumes later. The caller's `RequestContext` is
threaded through every deferral and continuation, so the reply that
arrives several turns later still lands on the original caller's
authority; application code never reconstructs or re-parks the caller.

The Tina side runs deterministically inside `tina_sim::Simulator`; the
Tokio side runs the same readiness shape with `tokio::time::sleep`.

## Run

```sh
cargo run --manifest-path examples/specimen_multi_turn_request_context/Cargo.toml -- tina
cargo run --manifest-path examples/specimen_multi_turn_request_context/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_multi_turn_request_context/Cargo.toml -- both
```

The Tina path prints `tina: ["ready"]` when both dependencies answer in
time and `tina: ["not_ready"]` when either times out.

Run the smoke tests (ready, probe-timeout, db-timeout, tokio control):

```sh
cargo test --manifest-path examples/specimen_multi_turn_request_context/Cargo.toml
```

## Read

- [`src/tina_impl.rs`](src/tina_impl.rs) — the `Probe`/`Db` deferred
  request services, the `tina::flow!` readiness continuation, and the
  simulator runner.
- [`src/tokio_impl.rs`](src/tokio_impl.rs) — the Tokio control.
