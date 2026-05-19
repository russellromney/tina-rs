# Phase 111: Service Product Surface

## Status

- IDD implementation phase.
- One PR.
- Can run beside Phase 110 if ownership stays split:
  - Phase 110 owns pending/workflow helpers.
  - Phase 111 owns service reports, service skeleton shape, docs, and selected
    system rewrites.
- Can run beside protocol/bridge phases if it does not change runtime trace
  event vocabulary or bridge internals.

## Grug Truth

A real service is more than one handler.

It has:

- routes
- bridges
- pools
- pending work
- capacity
- health
- shutdown
- replay facts

Today the pieces exist.

The copied path is still scattered.

This phase makes one boring service surface.

No framework fog.

No hidden global registry.

No fake "everything is healthy" report.

Prefer compile-time rails for report shape. Missing runtime data is an honest
runtime state, but building a half-shaped `ServiceReport` by struct literal is a
coding mistake.

## Goal

Make the copied production-shaped Tina service easy to assemble and easy to
inspect.

Ship a small service product surface in `tina-runtime`:

- `tina_runtime::service_report::ServiceReport`
- `tina_runtime::service_report::ServiceReportBuilder`
- `tina_runtime::service_pressure::ServicePressureBuilder`
- one topology/lifecycle assembly path using existing lifecycle types
- one shutdown summary shape using existing shutdown types
- one replay-fact handoff shape
- refreshed system specimens that use the shape

The user should be able to ask:

```text
what is running?
what is full?
what is unhealthy?
what is draining?
what facts can I replay?
```

and get one service-shaped answer.

## Names

Use user-facing names.

Preferred names:

- `ServiceReport`
- `ServiceReportBuilder`
- `ServicePressureReport`
- `ServicePressureBuilder`
- `ServicePressureSurface`
- `ServiceReplayStatus`

Avoid names like:

- `Registry` unless it is explicit service-local state
- `Aggregator` if it hides source surfaces
- `Manager`
- `Framework`

The service owns the builder. The runtime does not invent a global service
registry.

## Non-Goals

- no web framework
- no Axum clone
- no global service registry
- no automatic retry
- no hidden queue
- no hidden shutdown ordering
- no protocol trace event changes
- no bridge implementation refactor
- no generic plugin system
- no Prometheus server
- no dashboard

## Build

### Rock 1: `ServicePressureBuilder`

Ship a builder that assembles existing pressure/capacity surfaces.

Home:

```rust
tina_runtime::service_pressure::ServicePressureBuilder
```

Required inputs:

- `CapacitySurfaceReport`
- `CapacitySummary`
- `ServicePressureReport`
- `SharedCapacityScope::surface_report(...)`
- `BoundedEventSink::surface_report(...)`
- pool pressure reports
- bridge pressure reports that already return `CapacitySurfaceReport`
- HTTP body metrics converted by new helpers on `BodyPressureReport`:
  `capacity_surfaces(prefix, mode)` and `service_surfaces(prefix, kind, mode)`
- unavailable surfaces with typed reason text

Required output:

- summary line
- discovery lines
- `CapacitySummary`
- assert helper result

Required API:

```rust
ServicePressureBuilder::new(service_name)
builder.surface(kind, CapacitySurfaceReport)
builder.unavailable(name, kind, reason)
builder.merge(ServicePressureReport)
builder.finish() -> Result<ServicePressureReport, ServiceReportBuildError>
```

Required build errors:

```rust
ServiceReportBuildError::BadServiceName
ServiceReportBuildError::BadSurfaceName { name }
ServiceReportBuildError::DuplicateSurface { name }
```

Rules:

- surface names are validated once on insertion
- duplicate surface names reject loudly
- unavailable surfaces are explicit, not omitted
- no unbounded collection beyond number of inserted surfaces
- no sampling hidden threads
- no implied "healthy" when a surface is missing
- new builder-owned types have private fields when public fields would bypass
  validation. Users construct through builder methods, not struct literals.

Tests:

- duplicate name rejects
- bad name rejects
- unavailable surface appears in summary and discovery
- full surface appears in `assert_no_full`
- builder output matches direct existing `ServicePressureReport` behavior
- no surface disappears when one adapter returns unavailable
- compile-fail proves a new builder-owned surface wrapper cannot be constructed
  with raw fields that bypass name validation.

### Rock 2: `ServiceReportBuilder`

Ship a service report builder that threads together:

Home:

```rust
tina_runtime::service_report::{ServiceReport, ServiceReportBuilder}
```

- service name
- lifecycle/readiness/health
- topology components
- pressure summary
- shutdown choreography/report
- replay/support status

Required output:

- `ServiceReport`
- `summary_line()`
- `discovery_lines()`
- `health()`
- `readiness()`
- `capacity_summary()`

Required API:

```rust
ServiceReportBuilder::new(service_name)
builder.lifecycle(Lifecycle)
builder.readiness(Readiness)
builder.health(Health)
builder.topology(ServiceTopology)
builder.pressure(ServicePressureReport)
builder.shutdown(ServiceShutdownReport)
builder.replay(ServiceReplayStatus)
builder.finish() -> Result<ServiceReport, ServiceReportBuildError>
```

Additional `ServiceReportBuilder` errors:

```rust
ServiceReportBuildError::MissingLifecycle
ServiceReportBuildError::MissingReadiness
ServiceReportBuildError::MissingHealth
ServiceReportBuildError::MissingPressure
ServiceReportBuildError::MissingTopology
```

Rules:

- every component has a name
- every component has a lifecycle state or explicit `Unavailable`
- topology is service-local, not runtime-global
- builder and report implement `Debug`
- `ServiceReport` implements `Clone`
- missing optional data is explicit in the report
- `ServiceReport` fields are private. Users can inspect through methods, but
  cannot build a report that skipped lifecycle/readiness/health/pressure.
- `ServiceReplayStatus` is a typed enum, not free text.

Tests:

- report names every inserted component
- migrated system degrades readiness when its own pressure policy sees a live
  full surface; the builder only preserves that verdict
- readiness stays ready when a surface has pressure history but no current
  pressure, so historical full counts do not poison the live readiness answer
- shutdown report is preserved after service stops
- topology lines include lifecycle and pressure name
- a report with missing bridge metrics says unavailable
- missing required report pieces reject with the exact typed error above
- compile-fail proves `ServiceReport { ... }` construction outside the module
  is impossible.
- compile-fail proves replay status cannot be supplied as a plain string.

### Rock 3: Shutdown Summary

Use existing `ShutdownChoreography` and `ResourceCloseReport`.

Add the adapters needed so a service can fold shutdown into `ServiceReport`:

- `ServiceReportBuilder::shutdown_choreography(...)`
- `ServiceReportBuilder::resource_close_report(...)`
- `ServiceReport::shutdown_summary_line()`

Rules:

- shutdown order stays visible
- ordering violations stay visible
- late work remains visible
- no helper closes resources automatically unless named as a shutdown action

Tests:

- clean shutdown summary
- shutdown with in-flight request
- shutdown with one unavailable resource report
- post-shutdown new work is rejected or closed visibly in the refreshed system

### Rock 4: Replay Fact Handoff

Do not build protocol replay here.

Add a small handoff shape so a service report can say:

- replay facts available
- replay facts unavailable
- unsupported facts observed

Add a plain-data field on `ServiceReport`:

```rust
ServiceReplayStatus
```

with variants:

- `Available { case_name, projected_events }`
- `Unsupported { facts }`
- `NotCaptured { reason }`

Rules:

- no fake replay claim
- unsupported facts are named
- config/topology used by replay is visible

Tests:

- report with replay facts prints them
- report with unsupported facts says unsupported
- missing replay facts does not fail service health by itself

### Rock 5: System Specimen Refresh

Migrate these systems:

- `examples/systems/mini_saas_api`
- `examples/systems/system_soak_http_db`
- `examples/systems/system_api_gateway_limits`

- `examples/systems/system_metrics_shipper`

Required changes:

- service report built through the new builder
- pressure surfaces inserted through the new builder
- health/readiness visible through report
- shutdown summary visible where the system has shutdown
- README shows copied output

Do not rewrite business logic for churn.

Tests:

- each migrated system has a smoke test
- each migrated system asserts at least one report line
- one migrated system forces `Full` and proves the report names it
- one migrated system proves unavailable bridge/surface is explicit
- one migrated system proves shutdown report survives stop
- one migrated system writes the copied report lines into its README and the
  test asserts the live output still contains those key fields

### Rock 6: Docs

Update:

- `docs/tina-user-guide/10-service-patterns.md`
- `docs/tina-user-guide/14-lifecycle-and-shutdown.md`
- `docs/tina-user-guide/15-service-client-worked-example.md`
- `docs/tina-user-guide/06-boundedness-and-overload.md`
- `docs/tina-user-guide/README.md`
- `examples/systems/README.md`
- `examples/FINDINGS.md`
- `CHANGELOG.md`

Docs must show:

- one service report builder example
- one pressure builder example
- one shutdown report example
- one unavailable surface example
- one copied command that runs a migrated system smoke test

Docs must say:

- service report is service-local
- missing surfaces must be declared unavailable
- pressure is not health by itself, but readiness may use pressure
- replay support is explicit
- copied code uses builders, not direct struct literals

## Required Proof

Run at least:

```text
cargo fmt --all --check
cargo test -p tina-runtime service_report -- --nocapture
cargo test -p tina-runtime service_pressure -- --nocapture
cargo test -p tina-runtime lifecycle -- --nocapture
cargo test -p tina-runtime --test compile_fail -- service_report
cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml
cargo test --manifest-path examples/systems/system_soak_http_db/Cargo.toml
cargo test --manifest-path examples/systems/system_api_gateway_limits/Cargo.toml
cargo test --manifest-path examples/systems/system_metrics_shipper/Cargo.toml
cargo clippy -p tina-runtime --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

## Hostile Review Checklist

Before merge, prove:

- no missing surface is silently omitted
- duplicate names are loud
- unavailable is visible
- shutdown order is visible
- service report is service-local
- readiness degradation is an explicit service choice, not automatic magic
- invariant-bearing report fields are private or otherwise compile-guarded
- no hidden global registry exists
- no unbounded report storage exists beyond inserted surface count
- systems are shorter or safer, not just rewritten
- at least one cheap-model copied command is in README

## Done Means

A new Tina service can copy one shape for:

- health
- readiness
- topology
- capacity
- shutdown
- replay fact availability

and the report tells the truth when pieces are full, missing, unavailable, or
draining.
