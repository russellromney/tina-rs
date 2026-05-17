# Phase 107: Observability And Capacity Product

## Status

- IDD implementation phase.
- Builds on capacity reports, pressure reports, and tracing work.

## Grug Truth

Bounded systems need gauges.

If users cannot see what filled, what dropped, and what got slow, they will set
stupid-high caps and hope.

## Goal

Turn pressure/capacity facts into a product-quality runtime surface:

- service pressure summary
- shard-local capacity scopes
- bounded event/log sink
- queue/full/high-water reports
- CI-friendly capacity assertions for live runs and simulator runs with matching
  report surfaces

## Non-Goals

- No Prometheus server in core.
- No tracing backend.
- No memory magic.
- No automatic capacity tuning.
- No global cross-shard budget.

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

Each assertion helper must have a live path. If the same surface exists in the
simulator, the assertion must work there too. If it does not, the report says
`Unavailable`.

### Rock 5: Docs And Specimen Sweep

Update the production skeleton specimen plus two pressure-heavy specimens to
print or assert capacity summaries. Leave tiny examples alone.

## Required Proof

- Shared scope fill/release/refill.
- Owner stop releases scope charges.
- Bounded event sink drops visibly under load.
- Runtime summary includes at least one pool, bridge, listener, and body surface.
- CI-style assertion failure has copyable message.
- DST replay preserves relevant pressure facts.
- No report path allocates unbounded storage.

## Done Means

A user can run a load test, get a compact capacity/pressure summary, and decide
which cap to change without guessing.
