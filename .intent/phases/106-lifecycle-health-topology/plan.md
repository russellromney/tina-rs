# Phase 106: Lifecycle, Health, And Topology

## Status

- IDD implementation phase.

## Grug Truth

Services need a boring life.

Start. Become ready. Stop admitting. Drain. Close resources. Report what
happened. Stop. If every app hand-rolls this, every app gets one edge wrong.

## Goal

Make service lifecycle a copied Tina pattern:

- health/readiness/liveness
- topology report
- graceful shutdown choreography
- resource close/drain reports

## Non-Goals

- No Kubernetes framework.
- No service mesh.
- No hidden background supervisor.
- No "best effort" shutdown with no report.
- No single global registry for every app.

## Rocks

### Rock 1: Health Report

Add service health vocabulary:

- `Starting`
- `Ready`
- `Draining`
- `Degraded`
- `NotReady`
- `Stopped`

Include reasons and recent pressure facts. Health is data, not just a boolean.

### Rock 2: Topology Report

Add local runtime/service topology report:

- isolates
- bridges
- pools
- listeners
- important addresses
- shard
- capacity surfaces
- lifecycle state

No trace spelunking to know what the app started.

### Rock 3: Shutdown Choreography

Build a shutdown state helper with explicit steps:

1. stop ingress
2. cancel or close sessions
3. drain in-flight work
4. flush batchers
5. close pools/bridges/resources
6. emit final report
7. stop owner

Each step has timeout and outcome. No hidden scheduler.

### Rock 4: Resource Close/Drain Reports

Unify report vocabulary for:

- listener close
- connection close
- body stream drain
- pool drain/force close
- bridge close
- child stop/join

The resource-specific details stay typed, but the lifecycle words should match.

### Rock 5: Production Skeleton Refresh

Update one system specimen into the copied skeleton:

- health endpoint
- readiness endpoint
- shutdown signal
- pool/bridge close
- final report
- capacity summary
- topology report

This is the "start here" shape for real apps.

## Required Proof

- Service starts NotReady, becomes Ready, enters Draining, then Stopped.
- New requests rejected after ingress stop.
- Existing request drains or times out visibly.
- Pool/bridge/listener close reports are included.
- Topology lists real started surfaces.
- Shutdown with stuck child returns a timeout report.
- Signal-driven shutdown works in live runtime.
- DST case for shutdown ordering.

## Done Means

A user can copy one service skeleton and get health, readiness, graceful
shutdown, topology, and final lifecycle truth without inventing a private
framework.
