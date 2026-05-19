# Phase 107: Observability And Capacity Product

## Status

- IDD implementation phase.
- Builds on capacity reports, pressure reports, and tracing work.

## Grug Truth

Bounded systems need gauges.

If users cannot see what filled, what dropped, and what got slow, they will set
stupid-high caps and hope.

## Goal

Turn pressure/capacity facts into a product-quality **live** runtime surface:

- service pressure summary
- shard-local capacity scopes
- bounded event/log sink
- queue/full/high-water reports
- CI-friendly capacity assertions for live runs

The phase ships the live product. Trace-derived pressure facts
(`PressureSummary::from_events`) continue to replay through DST as
before. The new *out-of-trace* live surfaces — `SharedCapacityScope`
and `BoundedEventSink` — intentionally report `Unavailable` in sim
for this phase. A simulator adapter that carries those facts into
replay is explicit follow-up (see [`examples/FINDINGS.md`](../../../examples/FINDINGS.md)
entry #30) and not a 107 blocker.

## Non-Goals

- No Prometheus server in core.
- No tracing backend.
- No memory magic.
- No automatic capacity tuning.
- No global cross-shard budget.
- No simulator adapter for `SharedCapacityScope` / `BoundedEventSink`
  in this phase. Sim runs surface them as `Unavailable { reason }`
  via `ServicePressureReport`. The adapter ships in a follow-up.

## Rocks

### Rock 1: Runtime Pressure Summary

Add a copied report shape:

- mailbox pressure
- pending replies/calls
- pool waiters/leases
- bridge in-flight/full/late
- body bytes
- protocol sessions
- high-water and full counts

Missing surfaces are explicit `Unavailable`, not omitted silently.

### Rock 2: Shard-Local Capacity Scopes

Build shared capacity scopes with user-defined weight:

- default weight is count = 1
- users can charge bytes/messages/work units
- admission returns typed `Full`
- reports show current/high-water/full/released
- owner stop releases held charges

Wrong weights can still make bad systems; reports must make the lie visible.

### Rock 3: Bounded Event Sink

Add a bounded event/log sink for runtime/service facts:

- cap
- drop policy
- dropped count
- high-water
- drain snapshot

Never add an unbounded "observability queue."

### Rock 4: Capacity Assertions

Make test/CI assertions easy:

- surface exists
- current <= cap
- high-water <= expected
- full count == expected
- no drops
- utilization line for discovery

Each assertion helper must have a live path. Simulator parity for
the new out-of-trace primitives (`SharedCapacityScope`,
`BoundedEventSink`) is *not* in this phase: sim runs report those
surfaces as `Unavailable { reason }` through `ServicePressureReport`
so the contract stays honest. Trace-derived pressure
(`PressureSummary::from_events`) is unchanged and still replays.

### Rock 5: Docs And Specimen Sweep

Update the production skeleton specimen plus two pressure-heavy specimens to
print or assert capacity summaries. Leave tiny examples alone.

## User Proof

Update:

- `mini_saas_api`: prints one compact startup/topology/capacity summary and
  asserts no unexpected full/drop in smoke.
- `system_api_gateway_limits`: proves shared weighted capacity.
- `system_soak_http_db`: emits discovery lines usable in CI.

Every report line must be grep-friendly and copyable into a test assertion.

## Required Proof

- Shared scope fill/release/refill.
- Owner stop releases scope charges.
- Bounded event sink drops visibly under load.
- Runtime summary includes at least one pool, bridge, listener, and body surface.
- CI-style assertion failure has copyable message.
- DST replay preserves existing trace-derived pressure facts
  (`PressureSummary::from_events` over `RuntimeEvent`). New
  out-of-trace surfaces (`SharedCapacityScope`, `BoundedEventSink`)
  surface as `Unavailable` in sim — the sim adapter is follow-up.
- At least three README examples show exact commands and output shape.
- No report path allocates unbounded storage.

## Follow-Ups (Not In This Phase)

- DST adapter that carries `SharedCapacityScope` /
  `BoundedEventSink` snapshots through replay so sim runs can
  reconstruct `assert_no_full` semantics for the new primitives. See
  [`examples/FINDINGS.md`](../../../examples/FINDINGS.md) entry #30
  for the rough shape (snapshot at admit/release/drop/push/drain, or
  ride alongside `LiveReplayFact`).

## Done Means

A user can run a load test, get a compact capacity/pressure summary, and decide
which cap to change without guessing.
