# 034 Piet de Jong Plan Review 3

Verdict: ready to execute. The previous review findings are closed with enough
rails to keep implementation from wandering into three app owners, two bridge
surfaces, or foggy closeout.

## What Changed

- Added a pinned intended surface: `tina_runtime::LocalApp` is the preferred
  live app owner; `BetelgeuseBackedRuntime` and
  `BetelgeuseBackedMultiShardRuntime` stay lower-level backend-honest runners;
  `BridgeHost` wraps the app path at the bridge edge.
- Added user-shaped sketches for single-shard, multi-shard, and bridge usage.
- Pinned Tower `Service` as the canonical bridge boundary, with Axum as the
  first proof adapter.
- Added bridge cancellation truth table, including the hard rule that a running
  synchronous Tina handler is not preempted mid-turn.
- Pinned CI to the existing `.github/workflows/verify.yml` / `make verify`
  required gate, with stress/Loom/Miri classified as separate manual or nightly
  gates unless proven cheap.
- Pinned performance method: existing global-allocator probes for allocation,
  custom release-mode harness first for wall-clock, recorded evidence before
  flaky CI thresholds.
- Added local-service support table: time/TCP/Tower/Axum/health/shutdown/metrics
  in Piet; broader I/O in Jelle Zijlstra; persistence in Wim Kok.
- Named required e2e workloads:
  `llama_http_bridge_service`, `llama_tcp_timer_service`,
  `llama_supervised_worker_service`, and `llama_sim_dst_parity_service`.

## Remaining Risks

- This is a large phase. It should land in reviewable commits: app owner first,
  bridge breadth second, hardening/perf/e2e after the public surface settles.
- `LocalApp` is an intended surface, not existing code. Implementation must
  still check whether Rust generics/coherence make that exact shape awkward. If
  names change, update the plan before shipping a different owner story.
- Tower-first bridge is right, but `poll_ready` must stay honest: health is not
  queue admission unless the runtime grows a real capacity probe.
- Performance numbers should not become fake precision. Allocation/no-growth
  can be test-gated; wall-clock should be recorded first.

## Launch Guidance

1. Start with an audit of existing `BetelgeuseBackedRuntime`, `BridgeHost`,
   CI, and application-surface tests.
2. Build `LocalApp` or the closest code-shaped equivalent, then immediately
   migrate one existing user-shaped test to it.
3. Keep Tower as the bridge source of truth; add Axum only as adapter/proof.
4. Add cancellation and shutdown tests before broadening bridge helpers.
5. Add e2e workloads by name, then use them to decide whether any API helper is
   genuinely missing.

Grug says ready. Big fire, but ring of stones now exists.

## Implementation Review 1

Status: first execution slice landed locally; not the whole phase yet.

What changed:

- Added `tina_runtime::LocalApp` as the canonical single-shard live app owner.
  It wraps `BetelgeuseBackedRuntime` rather than replacing the proven runner.
- Added `LocalApp::single_shard(...)` and `LocalApp::multi_shard(...)` entry
  points. Multi-shard currently returns `LocalMultiShardApp`, preserving one
  `LocalApp` entry name while keeping the concrete owner honest.
- Added terminal lifecycle types: `LocalAppState`, `LocalAppTerminalReport`,
  `LocalAppShutdown`, and `LocalMultiShardAppShutdown`.
- Added `BridgeHost::from_app(app)` so bridge-hosted services can start from
  the canonical `LocalApp` path instead of constructing the lower-level runner
  directly.
- Added `BridgeBackpressure::retry_within(...)` so retry policy can have both a
  per-attempt timeout and a total policy deadline.

Proof added:

- `tina-runtime/tests/local_app.rs`
  - `local_app_single_shard_is_canonical_live_owner`
  - `local_app_multi_shard_uses_same_entry_name_for_topology`
  - `llama_tcp_timer_service_uses_local_app_runtime_owned_time`
- `tina-tokio-bridge/tests/axum_bridge.rs`
  - `bridge_host_can_be_built_from_canonical_local_app`
  - `bridge_retry_policy_can_have_total_deadline`

Targeted verification:

- `cargo test -p tina-runtime --test local_app -p tina-tokio-bridge --test axum_bridge`
  passes locally.
- `make verify` passes locally.

Self-review fix:

- `LocalAppMultiShardBuilder::shard_pair_capacity(...)` initially risked being
  a pretend knob. The live multi-shard substrate currently routes remote sends
  through the target worker's bounded command queue, so the method now sets the
  same underlying capacity and documents that limitation instead of doing
  nothing.

Remaining for Piet:

- stronger lifecycle failure/worker-panic terminal proof;
- performance-envelope artifact;
- any API tightening discovered while moving more tests to `LocalApp`.

## Implementation Review 2

Status: named workload rails are now visible in the test suite.

What changed:

- Renamed the Axum/Tower bridge proof to
  `llama_http_bridge_service_routes_axum_to_tower_bridge`.
- Renamed the local supervision proof to
  `llama_supervised_worker_service_restarts_worker_and_rejects_stale_address`.
- Renamed the simulator oracle proof to
  `llama_sim_dst_parity_service_replays_bounded_worker_pressure_and_partial_writes`.
- `llama_tcp_timer_service_uses_local_app_runtime_owned_time` already landed in
  the new `LocalApp` test file.

Targeted verification:

- `cargo test -p tina-runtime --test local_app`
- `cargo test -p tina-runtime --test application_surface llama_supervised_worker_service`
- `cargo test -p tina-sim --test application_surface llama_sim_dst_parity_service`
- `cargo test -p tina-tokio-bridge --test axum_bridge llama_http_bridge_service`
- `make verify`

All pass locally.

Remaining for Piet:

- stronger lifecycle failure/worker-panic terminal proof;
- performance-envelope artifact;
- any API tightening discovered while moving more tests to `LocalApp`.
