# 035 Jelle Zijlstra Runtime-Owned I/O Breadth Plan

## Purpose

Make the next runtime-owned I/O expansion explicit so it does not keep
appearing from fog.

Piet de Jong owns the local production core with the current I/O claim: time,
TCP, bridge ingress, shutdown, health, hardening, and measured cost. Jelle
Zijlstra owns the question after that: which additional I/O families Tina must
support to feel like a real local service framework rather than a TCP/time
isolate system.

This is not persistence. Durable state is Wim Kok.

## Starting Baseline

At Jelle start, expected baseline from Piet:

- canonical local app runner exists or is explicitly deferred;
- Tower/Axum bridge is production-shaped enough for local service ingress;
- runtime-owned time and TCP are tested across live runtime and simulator;
- cancellation, shutdown, overload, and trace semantics are stable;
- performance/allocation envelope names current hot costs;
- local-service support table says broader I/O is deferred to this phase.

## I/O Families To Decide

This phase must decide and either implement or explicitly defer:

| Family | Why it matters | Main semantic risk |
|---|---|---|
| DNS | Real clients rarely connect to raw IPs forever. | Resolver caching, platform config, search domains, timeouts. |
| TLS | Real network services need secure connections. | Handshake cancellation, cert roots, ALPN, dependency choice, backpressure during handshake. |
| UDP | Metrics, discovery, game/network protocols. | Packet boundaries, loss, receive buffers, multicast temptation. |
| File | Config, logs, local state, snapshots. | Blocking vs async backend, platform behavior, hidden thread pools. |
| Process | Worker subprocesses and controlled shell-outs. | Child lifecycle, pipes, exit status, cancellation, zombie avoidance. |
| Signal | Graceful app shutdown and ops integration. | Platform differences, process-global handlers, deterministic simulation. |

Expected default:

- implement only families needed for Tina's near-term local service claim;
- pin deferrals with reasons when a family is not needed yet;
- keep the simulator/DST story first-class for every family that lands;
- never hide blocking work in an unbounded pool and call it Tina-safe.

## Implementation Shape

For each accepted I/O family:

- add a runtime-owned call type and user helper;
- add live driver support;
- add simulator/DST support or explicitly state why deterministic simulation is
  impossible and what narrower test oracle replaces it;
- define cancellation, shutdown, timeout, and overload semantics;
- define trace events and replay artifacts;
- add bridge interaction tests if the family can be used behind Tower/Axum;
- add performance/allocation notes for the hot path.

## Refusals

- no arbitrary async futures inside isolates;
- no hidden unbounded queues;
- no silent blocking thread pool;
- no "TLS because production" without choosing dependency and cancellation
  semantics;
- no DNS helper that bypasses Tina timeout/cancellation;
- no process API without child cleanup proof;
- no signal API that breaks deterministic tests by surprise.

## Proof

Each implemented I/O family needs:

- live runtime integration tests;
- simulator/DST tests where meaningful;
- cancellation and shutdown tests;
- timeout tests where operation can wait;
- trace/replay proof or explicit non-claim;
- bridge/service-level e2e test if it is part of normal app use.

## Done Means

- A support table states `supported`, `deferred`, or `rejected for now` for DNS,
  TLS, UDP, file, process, and signal.
- Every supported family has user-facing helpers and direct tests.
- Every supported family has live-runtime semantics and simulator/DST semantics
  or an explicit narrower oracle.
- Persistence is still not smuggled into this phase.
- The roadmap no longer has unnamed "broader I/O someday" hiding in open
  questions.
