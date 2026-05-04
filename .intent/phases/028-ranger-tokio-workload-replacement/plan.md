# 028 Ranger Service Workload Hardening Plan

## Purpose

Move Tina from primitive-complete toward service-ready.

Ranger is a framework-development phase. It should build the service-shaped
runtime pieces that are still missing or under-pressure after the driver
contract work:

- reusable framed TCP service structure;
- stateful session / registry coordination without shared mutable state;
- timer/backoff and shutdown behavior inside service-shaped flows;
- supervised bounded work execution;
- overload handling that remains visible and bounded under real service
  pressure.

This is not release polish and not a demo phase. The examples and tests are
development pressure: they should reveal missing helpers, awkward APIs, weak
runtime behavior, or incomplete service semantics before Gemini tries to
explain the framework.

## Context

025 made Betelgeuse the honest live substrate.
026 made the Tina-owned TCP/time driver contract backend-neutral.
027 added parallel support evidence, Betelgeuse simulated-I/O polish,
Tokio-vs-Tina constrained comparisons, and adapter research.

The next missing thing is service-shaped pressure on the framework. TCP echo
and small semantic comparisons are useful, but they do not force the same
design questions as a service with framing, state, overload, shutdown,
supervision, and retry behavior.

Ranger should build a small set of service workloads directly in Tina without
Tokio, Tower, Axum, Hyper, arbitrary futures, or async isolate handlers. The
goal is not to market Tina as a Tokio replacement. The goal is to make the
core framework better by exercising the kind of code users would eventually
write on top of it.

## Scope

### 1. Framed TCP Request/Response Service

Build one framed TCP service with reusable service structure:

- line-delimited or length-prefixed requests
- multiple clients
- partial reads
- partial writes
- malformed frames
- peer close
- graceful listener shutdown
- bounded per-connection and service mailboxes

This should push the runtime-owned TCP helpers, connection state shape,
partial-write handling, and shutdown semantics harder than echo did.

### 2. Stateful Session / Registry Service

Build a workload where local state and routing matter:

- one isolate owns per-session state
- one isolate owns shared registry/routing state
- no `Arc<Mutex<...>>` in user code
- bounded mailboxes expose overload
- stale or stopped session addresses fail visibly

This should pressure address ownership, stale-address handling, user-facing
registry ergonomics, and the "state stays local" programming model.

### 3. Timer / Backoff Workload

Build timer and retry behavior into service-shaped flow:

- timeout
- retry/backoff
- cancellation or requester stop
- shutdown with pending timer or I/O work
- deterministic simulator replay for the same logic where applicable

This should not be a standalone toy if it can naturally live inside the TCP or
worker service.

### 4. Supervised Worker Pool

Build a task-dispatcher-shaped service that can become a reusable pattern:

- bounded work queue
- worker failure
- supervisor restart
- stale-address rejection after restart
- restart budget exhaustion
- continued service for healthy workers

This should pressure supervision ergonomics as service code, not only as trace
unit tests.

### 5. Overload Lab

Pin overload behavior across the above workloads:

- live ingress `Full`
- target mailbox `Full`
- cross-shard queue `Full` where applicable
- slow consumer pressure
- shutdown under pending work
- no hidden unbounded fallback queue

Use operation/allocation evidence only where it is narrow and stable. Do not
turn this into benchmark theater.

## Build Shape

Ranger should prefer two complete service-shaped implementations over many
small demos:

1. A framed TCP request/response service.
2. A supervised stateful worker/registry service.

Timer/backoff, overload, malformed input, shutdown, restart, stale addresses,
and bounded pressure should be folded into those services where possible. Add
separate focused tests only when a behavior cannot be made clear inside the
service workloads.

## User Surface Requirements

All service code should use the preferred public surface:

- `tina::prelude::*`
- `#[tina::isolate(...)]` or `#[tina_runtime::isolate(...)]`
- helpers such as `send`, `reply`, `spawn`, `batch`, `stop`, and runtime-owned
  call helpers
- `BetelgeuseRuntime` / `BetelgeuseMultiShardRuntime` or the post-026
  driver-backed public equivalent
- `tina-sim` for deterministic proof when the workload fits simulation

If service code needs internal test shims to be understandable, that is an API
friction finding. Record it in `review.md`. Add only tiny helpers that reduce
repeated boilerplate without creating a second DSL.

## API Friction Log

For each ugly or repetitive service-code shape, record:

- the awkward snippet or pattern;
- why it exists;
- whether it is core semantic cost or removable boilerplate;
- the smallest helper or API change that would improve it;
- whether that helper should land in Ranger or be deferred.

Ranger may add small helpers only when they reinforce one preferred Tina
surface. Do not add parallel micro-DSLs.

## Tokio Context Notes

Tokio remains useful context because many service-shaped expectations come
from Rust users who know Tokio. Do not build side-by-side marketing demos.
Instead, record short notes in `review.md` where relevant:

- what service concern Tokio users would recognize;
- what Tina handles differently;
- what Tina still does not provide;
- whether the missing piece belongs in core, in an adapter phase, or nowhere.

Use hardened Tokio examples only when they clarify a design decision. Bounded
`mpsc`, `try_reserve`, `send_timeout`, structured shutdown, and current-thread
Tokio are fair references. Do not compare only against naive unbounded-channel
examples.

## Refusals

- Do not build `tina-runtime-tokio-bridge`.
- Do not add Tower, Axum, Hyper, or arbitrary futures integration.
- Do not make isolate handlers async.
- Do not expose driver/backend handles to user isolates.
- Do not add unbounded queues for convenience.
- Do not claim production readiness.
- Do not claim broad Tokio replacement.
- Do not start Gemini release docs until Ranger has service-shaped framework
  behavior worth documenting.

## Review Prompts

Ask reviewers to focus on:

- whether these workloads create real service-shaped pressure on Tina's public
  API and runtime behavior
- whether examples use the real preferred public Tina surface
- whether test coverage proves behavior rather than checking logs
- whether overload and shutdown outcomes stay visible
- whether any helper added in Ranger improves one preferred surface instead of
  creating another
- whether the Tokio comparisons are fair to hardened Tokio

## Done Means

- At least two non-trivial service-shaped Tina workloads exist and are written
  against the preferred public surface.
- The framed TCP service has assertion-backed tests for happy path, malformed
  input, partial reads/writes, backpressure, peer close, and graceful shutdown.
- The supervised stateful worker/registry service has assertion-backed tests
  for bounded work, local state, stale addresses, restart, budget exhaustion,
  and continued service for healthy workers.
- Timer/backoff and cancellation behavior is exercised inside service-shaped
  flow, not only in a toy timer test.
- At least one workload runs through live runtime, simulated driver/runtime,
  and `tina-sim`, or `review.md` records the concrete reason one layer does not
  apply.
- No Tina core semantics change unless the phase pauses and records the design
  decision first.
- No Tokio bridge or ecosystem integration appears.
- `review.md` records service behavior, API friction, small helper decisions,
  Tokio-context notes, and remaining non-claims.
- Gemini can document real service-shaped framework capability instead of
  inventing a docs story around smaller proof snippets.
- `make verify` passes.
