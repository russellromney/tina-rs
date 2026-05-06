# Phase 045 Review

Verdict: complete for the portable-local-runtime slice.

## Positive Review

- The phase is now code-built, not roadmap theater.
- `portable_service.rs` is a copyable public-path harness:
  `LocalMultiShardSystem`, budget builders, root registration, router, shard
  workers, cross-shard isolate calls, journal-before-reply, shutdown, terminal
  topology, trace, and journal replay.
- The harness found a real semantic bug: a service could receive an isolate
  call, perform runtime-owned persistence, then lose the original call context
  before replying. Live runtime and simulator now preserve call context across
  runtime-owned call completions and observed-send completions.
- Follow-up e2e now directly proves the observed-send side too: a called
  service can use an audit send outcome to reply to the original call, and
  `Full` on that observed send returns a typed failure without mutating the
  audit target.
- Backpressure is visible. Busy retry schedules a Tina-owned timer and then
  returns typed rejection; no hidden queue or hidden retry loop.
- Placement is visible. Wrong shard/key rejects before work runs. Unknown shard
  setup returns `ThreadedRuntimeError::UnknownShard`.
- Budget manifest is executable from the user path. DNS/TLS/process/signal and
  shutdown-drain knobs are available on builders and visible after shutdown.
- Service-level DST exists with saved seed, replay equality, common invariants,
  observed-send continuation, observed-send full before persistence, closed-worker
  outcomes, journal append, and deletion shrinking.
- Bridge cancellation model remains in the focused gate.
- `make verify-portable-runtime` is small and specific.

## Hostile Review

- Cost smoke is not a benchmark and has no real numbers. This is okay only
  because it says `local machine / not benchmark` and CI checks command shape,
  not speed.
- The canonical harness uses persistence and cross-shard calls, but not every
  I/O rail. Broader rail composition remains covered by existing `local_system`
  tests and should be judged hard in Baobab.
- `UnknownShard` is now typed for owner operations, but `try_send` to a manually
  forged unknown-shard address still returns the narrower ingress error shape.
  This is acceptable for 045 because the public placement/setup proof is typed;
  future API polish may split unknown-shard ingress separately.
- Service DST uses virtual persistence paths and semantic projections. It does
  not compare live event ids or OS timing, correctly. It also means it is not a
  live filesystem stress test.
- Runtime-call continuation context is now preserved through normal completion
  paths. The scary future edge is nested call cancellation plus requester stop;
  existing call-dispatch tests cover requester-stop/late-completion basics, but
  Baobab should keep this in its pressure matrix.

## Blast Radius

- Changed live runtime and simulator call bookkeeping:
  `InFlightCall` and `PendingIsolateCall` now carry continuation call context.
  This affects backend calls, observed sends, isolate calls, timeout/full/closed
  outcomes, and completion delivery.
- Existing isolate-call, call-dispatch, local-system cross-shard, consumer API,
  and portable service tests pass.
- Added `ThreadedRuntimeError::UnknownShard`; workspace check passes.
- CI now runs an extra focused gate after `make verify`.

## Verification

- `cargo check --workspace`
- `cargo test -p tina-runtime --test portable_service`
- `cargo test -p tina-runtime --test local_system builder_exposes_complete_budget_manifest`
- `cargo test -p tina-runtime --test local_system cross_shard_call -- --nocapture`
- `cargo test -p tina-runtime --test call_dispatch -- --nocapture`
- `cargo test -p tina-sim --test consumer_api isolate_call -- --nocapture`
- `cargo test -p tina-sim --test portable_service_dst`
- `cargo test -p tina-tokio-bridge --test bridge_model_dst`
- `cargo test -p tina-tokio-bridge --test axum_bridge bridge_host_skips_cancelled_queued_request_before_user_state_mutates`
- `make verify-portable-runtime`
- `make verify`

Full closeout verification passes.
