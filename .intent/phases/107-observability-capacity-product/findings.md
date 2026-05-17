# Phase 107 — Observability And Capacity Product: Findings

## What shipped

- `tina_runtime::SharedCapacityScope` — shard-local weighted scope
  with `try_admit(weight) -> Result<SharedLease, SharedScopeFull>`.
  Drop releases. Includes `snapshot()`, `surface_report(mode)`,
  `decorate(report)`, `discovery_line()`. (Rock 2)
- `tina_runtime::BoundedEventSink<T>` — cap + drop policy
  (`DropOldest` / `DropNewest`) + `accepted` / `dropped` /
  `dropped_oldest` / `dropped_newest` / `high_water` counters and
  `drain_snapshot()` / `surface_report(mode)` /
  `discovery_line()`. Never unbounded. (Rock 3)
- `tina_runtime::ServicePressureReport` — copyable, grep-friendly
  aggregation of every bounded surface a service exposes, with
  explicit `Unavailable { reason }` for surfaces the service does
  not measure. `summary_line()` is one line; `discovery_report()` is
  one line per surface; `capacity_summary()` returns a
  `CapacitySummary` of measured surfaces only. (Rock 1)
- `CapacitySummary::assert_no_full()` + `format_assertion_failure`
  — aggregate "no Full anywhere" assertion that reports every
  offender at once in copyable `FAIL surface=…` shape. (Rock 4)
- `util_bp=N` field on `format_discovery_line` output — high-water
  utilization in basis points (0..=10000), so CI can grep
  utilization without parsing weights. (Rock 4)
- New specimens:
  - `examples/systems/system_api_gateway_limits` — two routes share
    one weighted scope, proving shared cap + owner-stop release.
  - `examples/systems/system_soak_http_db` — fast in-process soak
    that emits every required discovery line shape: `scope name=…`,
    `events sink=…`, `capacity surface=…`, `state=unavailable`,
    `service=…`, `FAIL surface=…`.
- `mini_saas_api` now emits a `startup_summary_line` plus a
  `startup_discovery_lines` topology + per-surface vector. The
  startup line names every surface the service declares, including
  the two it cannot measure from the startup scope
  (`db.bridge_in_flight`, `outbound.bridge_in_flight`).

## Required-proof checklist

- [x] Shared scope fill/release/refill: `system_api_gateway_limits`,
      `shared_scope::tests::admission_fills_and_release_refills`.
- [x] Owner stop releases scope charges:
      `shared_scope::tests::owner_stop_releases_held_charges`,
      `system_api_gateway_limits::pure_upload_burst_fills_only_upload_lane_then_drains`.
- [x] Bounded event sink drops visibly under load:
      `event_sink::tests::drop_oldest_evicts_front` /
      `drop_newest_keeps_first`, `system_soak_http_db` smoke.
- [x] Runtime summary includes at least one pool, bridge, listener,
      body surface: `mini_saas_api` startup summary covers body
      (32 bytes), controller mailbox, db pool, outbound pool, with
      bridge surfaces marked Unavailable.
- [x] CI-style assertion failure has copyable message:
      `capacity::tests::format_assertion_failure_starts_with_fail`,
      `system_soak_http_db` smoke greps `^FAIL surface=`.
- [x] DST replay preserves relevant pressure facts: pressure trace
      events feed `PressureSummary::from_events`; mini_saas_api
      `terminal_line` still includes `trace_pressure=…` after
      changes.
- [x] At least three README examples show exact commands and output
      shape: `mini_saas_api`, `system_api_gateway_limits`,
      `system_soak_http_db`.
- [x] No report path allocates unbounded storage: every new sink and
      summary is bounded by cap or by surface count.

## Non-goals preserved

- No Prometheus server.
- No tracing backend.
- No global cross-shard budget.
- No automatic capacity tuning.
- No memory magic.

## Hostile review

| Risk | Reading |
|---|---|
| "Discovery lines are still strings: a typo silently disappears." | Names are validated at `CapacitySummary::push` (empty, whitespace, control-char rejected). Discovery line quoting is asserted in tests. Risk reduced to "user picks a misleading name", which is mode-orthogonal. |
| "`util_bp=` could break existing parsers." | Existing parsers (mini_saas_api smoke, capacity grep) split on whitespace and look up keys; `util_bp=` is an additional key that everyone ignores by default. Smoke tests pass without changes. |
| "Shared scope leak on isolate panic." | Each lease is owned by `self.held_leases`/`HashMap<qid, SharedLease>` inside the isolate. If the isolate panics, the runtime tears down its state; `SharedLease::drop` releases the charge. The scope `Arc<Inner>` outlives the isolate. |
| "Event sink under heavy contention." | A single `Mutex<VecDeque<T>>` per sink. Contention is per shard at most. Drop accounting is atomic. Higher contention would justify per-shard sinks; for runtime/service facts it is enough. |
| "`Unavailable` reasons can drift from reality." | Each `Unavailable` is named in `summary_line` so on-call can spot a surface that *should* be observed but is not. There is no silent omission. |
| "Three new public types blow up the API surface." | The types replace ad-hoc per-bridge counters (see `BodyMetrics` baking its own shared scope). Subsequent rocks should migrate `BodyMetrics` to use `SharedCapacityScope` once the shape stabilizes; today's body code is unchanged. |
| "Assert helpers raise the bar for new isolates." | The helpers are opt-in. `CapacitySummary::push` is the only existing entry that gained validation, and it already validated names. `assert_no_full` is additive. |

## Suggested follow-ups

See [`examples/FINDINGS.md`](../../../examples/FINDINGS.md) entries
27–29:

- Lease handoff into a `PendingReplies` slot.
- Runtime-side `SharedScopeRegistry`.
- Effect combinator for multi-stage request rails.
