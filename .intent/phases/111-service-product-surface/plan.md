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

## Goal

Make the copied production-shaped Tina service easy to assemble and easy to
inspect.

Ship a small service product surface:

- one service report builder
- one pressure/capacity assembly path
- one topology/lifecycle assembly path
- one shutdown summary shape
- one replay-fact handoff shape where facts are already available
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
- `ServicePressure`
- `ServicePressureBuilder`
- `ServiceSurface`
- `ServiceFact`

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

Required inputs:

- `CapacitySurfaceReport`
- `CapacitySummary`
- `ServicePressureReport`
- `SharedCapacityScope` snapshot/report
- `BoundedEventSink` snapshot/report
- pool pressure reports
- bridge pressure/metrics reports where already exposed
- HTTP body metrics where already exposed
- unavailable surfaces with typed reason text

Required output:

- summary line
- discovery lines
- `CapacitySummary`
- assert helper result

Rules:

- surface names are validated once on insertion
- duplicate surface names reject loudly
- unavailable surfaces are explicit, not omitted
- no unbounded collection beyond number of inserted surfaces
- no sampling hidden threads
- no implied "healthy" when a surface is missing

Tests:

- duplicate name rejects
- bad name rejects
- unavailable surface appears in summary and discovery
- full surface appears in `assert_no_full`
- builder output matches direct existing `ServicePressureReport` behavior
- no surface disappears when one adapter returns unavailable

### Rock 2: `ServiceReportBuilder`

Ship a service report builder that threads together:

- service name
- lifecycle/readiness/health
- topology components
- pressure summary
- shutdown choreography/report if present
- replay/support facts if present

Required output:

- `ServiceReport`
- `summary_line()`
- `discovery_lines()`
- `health()`
- `readiness()`
- `capacity_summary()`

Rules:

- every component has a name
- every component has a lifecycle state or explicit `Unavailable`
- topology is service-local, not runtime-global
- builder is plain data and clone/debug friendly where practical
- missing optional data is explicit in the report

Tests:

- report names every inserted component
- readiness degrades when pressure surface is full
- shutdown report is preserved after service stops
- topology lines include lifecycle and pressure name
- a report with missing bridge metrics says unavailable

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

### Rock 6: Docs

Update:

- `docs/tina-user-guide/10-service-patterns.md`
- `docs/tina-user-guide/13-lifecycle-and-shutdown.md`
- `docs/tina-user-guide/14-service-client-worked-example.md` or the current
  worked-example page
- `docs/tina-user-guide/06-boundedness-and-overload.md`
- `examples/systems/README.md`
- `examples/FINDINGS.md`
- `CHANGELOG.md`

Docs must show one copied service report path.

Docs must say:

- service report is service-local
- missing surfaces must be declared unavailable
- pressure is not health by itself, but readiness may use pressure
- replay support is explicit

## Required Proof

Run at least:

```text
cargo fmt --all --check
cargo test -p tina-runtime service_pressure -- --nocapture
cargo test -p tina-runtime lifecycle -- --nocapture
cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml
cargo test --manifest-path examples/systems/system_soak_http_db/Cargo.toml
cargo test --manifest-path examples/systems/system_api_gateway_limits/Cargo.toml
cargo clippy -p tina-runtime --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

If targeted test names differ, run the nearest exact package/specimen tests and
record the commands in the phase status.

## Hostile Review Checklist

Before merge, prove:

- no missing surface is silently omitted
- duplicate names are loud
- unavailable is visible
- shutdown order is visible
- service report is service-local
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
