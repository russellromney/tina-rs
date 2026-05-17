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

- [x] Shared scope fill/release/refill:
      `system_api_gateway_limits::shared_scope_fills_and_releases_across_routes`,
      `shared_scope::tests::admission_fills_and_release_refills`.
- [x] Owner stop releases scope charges:
      `system_api_gateway_limits::owner_stop_releases_charges_when_isolate_is_torn_down_mid_flight`
      holds leases across `runtime.shutdown()` (hold > timeout) and
      asserts `scope_current_at_drain == 0` after the post-shutdown
      snapshot.
- [x] Bounded event sink drops visibly under load:
      `system_soak_http_db::event_sink_drops_visibly_under_load` runs
      the soak with a small cap and asserts `slow_events_dropped > 0`
      and that the discovery line's `dropped=N` matches.
- [x] Runtime summary includes at least one pool, bridge, listener,
      body surface: `mini_saas_api` startup summary covers body
      (`http.request_body`), controller mailbox, two pool surfaces
      (`db.pool`, `outbound.pool`), one listener
      (`http.main_listener.mailbox`), plus two bridge surfaces marked
      `Unavailable` with reasons. Smoke asserts every required name
      shows up in `startup_discovery_lines`.
- [x] CI-style assertion failure has copyable message:
      `capacity::tests::format_assertion_failure_starts_with_fail`,
      `capacity::tests::format_assertion_failure_covers_every_variant`,
      `system_soak_http_db` smoke greps `^FAIL surface=`.
- [x] At least three README examples show exact commands and output
      shape: `mini_saas_api`, `system_api_gateway_limits`,
      `system_soak_http_db`.
- [x] No report path allocates unbounded storage: every new sink and
      summary is bounded by cap or by surface count.
- [x] `full_count` is honest under contention:
      `shared_scope::tests::full_count_is_honest_under_admit_release_contention`
      runs 8×5000 concurrent admit/release rounds and asserts the
      counter matches observed `Err` returns. (The pre-fix code
      could over-count from a stale-read on the Full branch.)

### Partial / not-yet

- [~] DST replay preserves *new* pressure facts. The existing trace
      pressure (mailbox-full / send-rejected / call-reply-rejected)
      remains preserved across replay because `RuntimeEvent` is
      untouched; `PressureSummary::from_events` still walks the
      trace. The new types (`SharedCapacityScope`,
      `BoundedEventSink`) live outside the trace and are *not* yet
      replayed in sim. See follow-up #30 below.
- [~] Simulator parity for capacity assertions. Plan Rock 4 allows
      a sim surface to be marked `Unavailable`. Today no `tina-sim`
      path imports the new types. The `ServicePressureReport` shape
      lets a future sim adapter emit `Unavailable` lines until the
      live↔sim story is built. See follow-up #30 below.

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
27–30:

- Lease handoff into a `PendingReplies` slot.
- Runtime-side `SharedScopeRegistry`.
- Effect combinator for multi-stage request rails.
- DST/sim adapter that snapshots `SharedCapacityScope` /
  `BoundedEventSink` facts into the replay trace, so the new
  observability primitives carry through to simulator runs.
