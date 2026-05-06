# Phase 046 Review

Verdict: complete for the first Baobab readiness-gate slice.

## Positive Review

- Baobab now judges landed code, not roadmap hope.
- The capability matrix is executable Rust. It binds Tina rows to
  `RuntimeCapabilities` and names partial, not-claimed, and platform-gated
  rows. It now explicitly covers cancellation, backpressure, and cost
  reporting.
- The portable service harness now has a broader user-shaped gauntlet:
  TCP listener/session -> Tina timer -> DNS -> bounded process ->
  runtime-owned file -> journal append -> cross-shard isolate call ->
  terminal shutdown truth.
- The portable service harness now also proves live multi-shard failure shape:
  a failed shard does not stop a sibling shard from completing persisted work,
  and failed-target calls resolve visibly.
- The Baobab gate also runs existing focused LocalSystem rail tests for DNS,
  TLS, UDP/process/journal composition, TCP+persistence, and storage pressure.
- Service DST gained saved-seed histories for requester stop after admitted
  side effects, pressure plus shard failure, and deletion shrinking.
- Persistence DST gained saved-seed restart histories for clean recovery,
  truncated-tail recovery, and corrupt recovery.
- Bridge DST gained a saved-seed timeout + retry + shutdown contract history.
- Bridge timeout/cancel semantics are represented by both model DST and the
  hosted Axum bridge cancellation e2e.
- Cost output now measures real Tina smoke paths for local send, live ingress,
  cross-shard send, isolate call, plus local TCP loopback, with explicit
  `not-measured` rows for TLS/bridge. It still says not benchmark.
- CI has one named gate: `make verify`.

## Hostile Review

- Cost rows are still shallow smoke measurements, not serious benchmarks. This
  is acceptable only because the output keeps the no-claim label and
  `not-measured` rows.
- The composed Baobab service includes TCP, but not TLS. `make verify` runs
  existing LocalSystem TLS client/server e2e alongside the composed service.
  That keeps the gauntlet broad without making one test own every sharp rail.
- Glommio is only represented as a platform-gated matrix row. No Glommio code
  is added, intentionally.
- The requester-stop DST records an important truth: admitted worker side
  effects may still run after the requester stops. Tina makes that replayable;
  it is not rollback semantics.
- Baobab is a readiness gate, not proof of production readiness.

## Blast Radius

- Added one runtime integration test file: `readiness_matrix.rs`.
- Extended `portable_service.rs` with one new composed service and one live
  multi-shard failure service.
- Extended `portable_service_dst.rs` with three new Baobab DST tests and two
  small service ops.
- Extended `persistence_simulation.rs` and `bridge_model_dst.rs` with
  Baobab-named saved-seed histories.
- Changed the portable cost example output shape from `allocations,timing` to
  `iterations,timing_ns,status`, and replaced placeholder live rows with small
  real Tina smoke rows.
- Folded the Baobab readiness gate into `make verify` and CI.
- Updated README, SYSTEM, ROADMAP, CHANGELOG, and phase closeout notes.

## Verification

- `cargo test -p tina-runtime --test readiness_matrix`
- `cargo test -p tina-runtime --test portable_service`
- `cargo test -p tina-sim --test portable_service_dst`
- `cargo test -p tina-sim --test persistence_simulation`
- `cargo test -p tina-tokio-bridge --test bridge_model_dst`
- `make verify`

## Final Hostile Fix Pass

- Fixed the Baobab TCP gauntlet so it no longer assumes one `tcp_read` is a
  full request. The service now reads a bounded newline frame across multiple
  small Tina-owned TCP reads and asserts multiple `TcpRead` completions.
- Fixed the cost smoke row so `Tina TCP loopback` is a real Tina-owned
  bind/accept/read/write/close path, not raw `std::net` server work. The row
  also handles split reads and asserts the echoed bytes.
- Added normal Rust build-artifact ignore coverage so closeout does not leave
  local Cargo/native/debug byproducts as future footguns.
- Re-ran `make verify`; it passed.

No unresolved code findings remain for Baobab closeout. Remaining caveats are
intentional non-claims: cost rows are smoke, TLS/bridge cost rows are not
measured, Glommio is platform-gated, and Baobab is a readiness gate rather than
production readiness.
