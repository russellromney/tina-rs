# Phase 134 Review: Config And Budget Manifest

## What shipped

- `tina_runtime::budget` module with the planned vocabulary:
  `ServiceBudgetManifest`, `BudgetSurface`, `BudgetKind`, `BudgetUnit`,
  `BudgetCap`, `ReplayImpact`, `BudgetSource`, `BudgetManifestReport`,
  `BudgetManifestRow`, `ObservedBudget`, `BudgetConsistencyReport`,
  `BudgetConsistencyRow`, `BudgetReplayExport`, `ReplayBudgetEntry`,
  `BudgetValidationError`.
- `validate()` collects every error (not just the first): duplicate / empty /
  whitespace names, zero caps, bounded/unbounded mode conflicts, policy
  rejection (expired / production unbounded / empty reason), secret-looking
  printable fields, and missing required kinds.
- Adapters (`budget_surfaces(prefix)`): `LocalSystemConfig`,
  `ThreadedRuntimeConfig`, `MultiShardRuntimeConfig` (runtime);
  `HttpServerConfig`, `HttpClientConfig`, `PoolConfig`, `Http2ServerConfig`,
  `Http2ClientLimits`, `WebSocketLimits` (http); `SqliteConfig` (bridge).
- Consistency: `compare_capacity_summary`, `compare_service_pressure`,
  `compare_manifest`; `report(&pressure)` joins configured caps with observed
  facts.
- `replay_export()`: FNV-1a hash over replay-affecting surfaces + schema
  version; display-only surfaces listed but not hashed.
- `mini_saas_api` migrated: one `src/budget.rs` manifest, caps read back from
  it, validate-before-bind, manifest report at shutdown, manifest hash pinned
  into the live-replay fact, `tests/budget.rs` proof suite.
- Docs: boundedness user guide "Start Here" section, noun table rows, mini_saas
  README capacity section, CHANGELOG, FINDINGS finding 36 note.

## Required proof — where it lives

- Duplicate/invalid/zero/unbounded-expiry/secret rejection:
  `budget::tests` in `tina-runtime/src/budget.rs`.
- Rows from `LocalSystemConfig` / HTTP1 / HTTP2 / bridge:
  `budget_adapters::tests`, `tina-http/src/budget.rs::tests`,
  `tina-sqlite-bridge/src/budget.rs::tests`.
- Capacity-summary and service-pressure compare catches missing/extra/mismatch:
  `capacity_summary_compare_catches_missing_extra_and_mismatch`,
  `weighted_dimension_mismatch_is_caught`,
  `service_pressure_compare_treats_unavailable_as_declared`.
- Replay capture includes manifest metadata + change/ignore behaviour:
  `replay_export_hash_changes_on_replay_affecting_change`,
  `replay_export_ignores_display_only_change`,
  `replay_hash_is_stable_across_calls`; service-level in
  `mini_saas_api/tests/budget.rs::replay_export_pins_body_cap_and_ignores_display_only`
  and the smoke test's `budget_hash` assertion.
- mini_saas docs caps == manifest rows; live surfaces all have rows:
  `mini_saas_api/tests/budget.rs`.
- Observed capacity/full from runtime facts, not config:
  `live_surfaces_all_have_manifest_rows_and_observed_facts` asserts the body
  `full_count` comes from the run.

## Hostile self-review

- **Is the manifest a "giant magic builder"?** No. It is plain data plus
  `add`/`extend`/`validate`/`report`/`replay_export`. Adapters are free
  functions on each config; the service composes them. No builder hides
  registration or runtime semantics.
- **Did core runtime semantics change?** No. The module is additive; no
  existing config constructor takes a manifest. mini_saas register sites read
  the same numeric caps, now from one module instead of scattered literals.
- **Secret detector false positives/negatives.** Targeted: PEM blocks, AWS key
  ids, credential URLs, `secret-word=value` assignments. Env var *names* and
  file paths pass. It is intentionally not an entropy scanner (that would flag
  the export hash and legitimate tokens). A determined leak in a free-text
  owner label could still slip a non-matching format through; the detector
  catches the common shapes, not all conceivable ones. Documented as such.
- **Deadlines / retries.** The unit vocabulary is count and weight, not time.
  Deadlines are deliberately *not* surfaced rather than faked as counts. Retry
  budgets that are attempt *counts* are representable (`ConnectAttempt` +
  `Attempts`); retry *durations* are not. Stated in the PR and docs.
- **Zero-cap rule.** Rejected for every kind: a zero mailbox can't receive, a
  zero pool can't lease, a zero body-byte cap rejects all bodies, a zero lane
  disables a rail, a zero shard-pair deadlocks cross-shard. No kind has a
  legitimate zero in this vocabulary.
- **Replay hash stability.** FNV-1a over length-prefixed fields
  (`name`, `kind`, `unit`, `max`, `mode`), sorted by name, seeded with schema
  version and entry count. Portable and stable across runs (not
  `DefaultHasher`). Owner/shard labels are not hashed — they are display, not
  replay-determining.

## Second review round (independent adversarial pass) and fixes

An independent reviewer read the whole diff. Real findings fixed:

- **Replay-hash delimiter injection (correctness).** The old canonical form
  joined fields with `|` and entries with `\n`, so a user-defined weight unit
  label containing those bytes could shift field boundaries and collide two
  different manifests onto one hash. Fixed: the hash now length-prefixes every
  field (`fnv_field`), so content can never fake a boundary. Added
  `replay_hash_resists_unit_label_delimiter_injection`. Unit labels are also
  validated (no control chars / non-empty) via `InvalidUnitLabel`.
- **`report()` read observed numbers by the manifest's unit, not the live
  dimension.** A manifest/live dimension disagreement silently zeroed the
  observed numbers. Fixed: `ObservedBudget::from_report` now picks count vs
  weight from the live report itself; the dimension disagreement is surfaced by
  the consistency check, not by dropping data. Added
  `report_observed_dimension_follows_live_not_manifest`.
- **Zero-cap diagnostic swallowed on a mode conflict.** A `Bounded { max: 0 }`
  with an unbounded mode reported only `CapModeConflict`. Fixed: the zero check
  now runs regardless of mode shape. Added
  `zero_cap_with_mode_conflict_still_flags_zero`.
- **mini_saas was partly mirrored, not installed, from the manifest.** Fixed:
  `ServiceCaps::from_manifest` now also drives the accept mailbox; the handler
  body check uses the manifest-derived `body_cap`; and the observed body-cap
  fact is read from the actual listener config (not a const), so an off-manifest
  listener would fail the consistency check. (An attempt to also measure the
  outbound pool from the host during shutdown via a second `call_blocking`
  caused thread-resource exhaustion (`EAGAIN`) in teardown and was reverted;
  the pool stays observable live via `/debug/capacity` and is honest
  `Unavailable` in the shutdown join.)
- **Secret detector was shallow and over-claimed.** Broadened to catch `:`
  delimiters, nested roots (`db_password=`, `client_secret=`), and distinctive
  token prefixes (`ghp_`, `xoxb-`, …) at word boundaries; the doc now says
  "best-effort guard for common leaked-credential shapes," not "never carries
  secrets."

Findings deliberately left as documented limits (not bugs): count-flavor units
(`Calls`/`Connections`/…) compare on dimension only, by design; the HTTP
per-connection mailbox is a `tina-http` preset internal that a service can pull
in via `HttpServerConfig::budget_surfaces` but mini_saas does not declare; dual
count+weight surfaces are outside the one-dimension-per-surface model.

## Known limits / deliberately manual

- Time deadlines and retry-budget durations: out of vocabulary, stay manual.
- Per-isolate mailbox live depth: the runtime does not sample it, so those
  manifest rows report `observed=none` at shutdown (declared, not measured) —
  explicit `Unavailable`, never silently dropped.
- `MultiShardRuntimeConfig` is the only config with an exact manifest→config
  builder (`from_manifest`); configs with time fields or non-budget state are
  built by hand, by design.
- Replay export is a self-contained artifact (`BudgetReplayExport`) that a
  service pins into its own DST case (mini_saas pins the hash into its
  live-replay fact). It is not woven into `tina-sim`'s saved-case format; that
  would be a follow-up if more services need it.

## Test / lint status

- `cargo fmt --all --check`: clean.
- `cargo clippy -p tina-runtime -p tina-http -p tina-sqlite-bridge
  --all-targets`: clean.
- `cargo test` for the three crates' budget surfaces + `mini_saas_api` suite:
  pass.
- `mini_saas_api` `tests/soak.rs` is a pre-existing load test that is mildly
  timing-flaky under host load (one error kind occasionally classified
  differently); unchanged by this phase and passes on re-run. The cap values it
  uses are identical to before, now sourced from `crate::budget`.
