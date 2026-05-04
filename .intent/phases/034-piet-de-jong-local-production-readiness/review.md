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
