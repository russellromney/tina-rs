# Phase 082: Capacity Modeling Round 2

## Status Note

- Existing count-report surfaces: `WorkerPool` waiter count via
  `PoolPressureReport::to_waiters_capacity_report`, and
  `PendingReplies` slot count via `capacity_report()`.
- Chosen weighted surface: `tina-http::BodyMetrics` request and
  response body bytes. Weight is user-declared body-byte cost, not
  heap memory.
- Chosen shared-scope surfaces: `http.request_body` and
  `http.response_body` reports both charge one shard-local
  `http.bodies` scope. The scope is the cloned `BodyMetrics`
  instance threaded through one listener's connection isolates, not
  a process-global budget.

## Scope

- Add `CapacityWeight` vocabulary in `tina::capacity`.
- Add weight and shared-scope fields to `CapacitySurfaceReport`.
- Add explicit expiring `UnboundedForNow(reason)` and the ugly
  `unbounded_without_expiry_i_know_this_is_bad(reason)` escape hatch.
- Extend capacity assertions and discovery formatting so every
  `Full` says what filled.
- Fold HTTP body metrics into the capacity-report shape.
- Update one specimen to show `unknown -> measured -> fixed`.

## Status

- [x] Status note added before coding.
- [x] `CapacityWeight` added.
- [x] HTTP body bytes exposed as the weighted surface.
- [x] Request/response body reports share one shard-local scope.
- [x] Expiring `UnboundedForNow(reason)` added with one-hour default.
- [x] No-expiry escape hatch added and rejected by test/prod policy.
- [x] Assertions and discovery lines include weight/shared fields.
- [x] No DST proof added: the touched weighted surface is live
  HTTP metrics, not simulator state.
- [x] Docs and `specimen_http_body_streaming` updated.
- [x] Relevant tests, fmt, and clippy complete.
- [x] Hostile-review notes complete.
- [ ] PR opened.

## Hostile Review Notes

- Fixed during review: shared-scope high-water initially only moved
  on explicit `try_charge_*` calls, while existing HTTP connection
  code uses `charge_*`; moved shared high-water updates into the
  common charge path so real HTTP traffic reports the shared scope.
- Fixed after hostile review: live HTTP connection traffic now uses
  weighted admission (`try_charge_request` / `try_charge_response`)
  on request and response body charge points. Request-side weighted
  Full maps to the existing 413 shape before service dispatch;
  response-side weighted Full closes/truncates because the response
  head may already be on the wire. Both paths are covered by
  integration tests that assert the user-visible outcome and the
  weighted Full counters.
- Fixed after hostile review: weighted assertion helpers now fail
  loudly when pointed at count-only surfaces instead of treating
  missing weight fields as zero.
- Fixed after hostile review: weighted discovery hints now use
  `high_weight / max_weight` instead of always saying the weighted
  cap fits.
- Fixed after hostile review: body metrics docs no longer make even
  a lower-bound heap footprint claim; they describe body-byte weight
  only.
- Fixed while here: unbounded modes now reject empty reasons so
  "loud and named" is enforced by policy validation.
- No DST proof added: the weighted surface touched here is live
  HTTP metrics, not simulator-owned state. Adding fake DST state for
  live-only metrics would make the proof less honest.
