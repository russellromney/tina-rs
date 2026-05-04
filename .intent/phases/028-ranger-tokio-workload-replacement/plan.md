# 028 Ranger Tokio-Workload Replacement Plan

## Purpose

Prove that Tina can replace concrete Tokio-shaped workloads when the user does
not need Tokio ecosystem integration.

The question Ranger answers is:

> Can a user see Tina, run it, and replace an actual small Tokio service with
> Tina while keeping bounded queues, synchronous handlers, runtime-owned
> time/TCP, shutdown, supervision, overload visibility, and simulation proof?

This is not release polish. Gemini should document replacement evidence that
already exists. Ranger creates that evidence.

## Context

025 made Betelgeuse the honest live substrate.
026 made the Tina-owned TCP/time driver contract backend-neutral.
027 added parallel support evidence, Betelgeuse simulated-I/O polish,
Tokio-vs-Tina constrained comparisons, and adapter research.

The next missing thing is user belief. TCP echo and small semantic comparisons
are useful, but they are not enough for a user to say, "I can write my next
small Tokio service in Tina instead."

Ranger should build a replacement ladder: small services that look like things
Rust users write with Tokio today, but implemented directly in Tina without
Tokio, Tower, Axum, Hyper, arbitrary futures, or async isolate handlers.

## Scope

### 1. Framed TCP Request/Response Service

Build one framed TCP service with real protocol pressure:

- line-delimited or length-prefixed requests
- multiple clients
- partial reads
- partial writes
- malformed frames
- peer close
- graceful listener shutdown
- bounded per-connection and service mailboxes

This is the direct replacement for "start with `tokio::net::TcpListener`,
spawn one task per connection, parse frames, write responses."

### 2. Stateful Session / Registry Service

Build a workload where local state matters:

- one isolate owns per-session state
- one isolate owns shared registry/routing state
- no `Arc<Mutex<...>>` in user code
- bounded mailboxes expose overload
- stale or stopped session addresses fail visibly

This proves Tina's core value: state stays local and the runtime owns
communication.

### 3. Timer / Backoff Workload

Build a retrying client or worker:

- timeout
- retry/backoff
- cancellation or requester stop
- shutdown with pending timer or I/O work
- deterministic simulator replay for the same logic where applicable

### 4. Supervised Worker Pool

Build a task-dispatcher-shaped service:

- bounded work queue
- worker failure
- supervisor restart
- stale-address rejection after restart
- restart budget exhaustion
- continued service for healthy workers

This should be recognizable as a small background job runner, not only a unit
test for restart events.

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

## User Surface Requirements

All examples should use the preferred public surface:

- `tina::prelude::*`
- `#[tina::isolate(...)]` or `#[tina_runtime::isolate(...)]`
- helpers such as `send`, `reply`, `spawn`, `batch`, `stop`, and runtime-owned
  call helpers
- `BetelgeuseRuntime` / `BetelgeuseMultiShardRuntime` or the post-026
  driver-backed public equivalent
- `tina-sim` for deterministic proof when the workload fits simulation

If an example needs internal test shims to be understandable, that is an API
friction finding. Record it in `review.md`. Add only tiny helpers that reduce
repeated boilerplate without creating a second DSL.

## Tokio Comparison Requirements

For each workload, record a short comparison in `review.md`:

- what a typical Tokio version would use
- what Tina uses instead
- what Tina strengthens
- what Tina weakens or does not provide
- whether this is a true replacement candidate or needs Apollo because it
  depends on Tokio ecosystem APIs

Use hardened Tokio examples where useful. Bounded `mpsc`, `try_reserve`,
`send_timeout`, structured shutdown, and current-thread Tokio all count. Do not
compare only against naive unbounded-channel examples.

## Refusals

- Do not build `tina-runtime-tokio-bridge`.
- Do not add Tower, Axum, Hyper, or arbitrary futures integration.
- Do not make isolate handlers async.
- Do not expose driver/backend handles to user isolates.
- Do not add unbounded queues for convenience.
- Do not claim full production readiness.
- Do not claim broad Tokio replacement. Claim only no-ecosystem-dependency
  workload replacement where the evidence exists.
- Do not start Gemini release docs until Ranger gives Gemini concrete
  replacement workloads to document.

## Review Prompts

Ask reviewers to focus on:

- whether these workloads are recognizable replacements for small Tokio
  services
- whether examples use the real preferred public Tina surface
- whether test coverage proves behavior rather than checking logs
- whether overload and shutdown outcomes stay visible
- whether any helper added in Ranger improves one preferred surface instead of
  creating another
- whether the Tokio comparisons are fair to hardened Tokio

## Done Means

- At least two non-trivial Tina services are runnable by a new user and look
  like things they would otherwise write with Tokio.
- Each service has black-box assertion-backed tests for happy path, malformed
  input where relevant, backpressure, timeout/cancellation, shutdown, and
  failure/restart where relevant.
- At least one workload runs through live runtime, simulated driver/runtime,
  and `tina-sim`, or `review.md` records why one layer does not apply.
- No Tina core semantics change unless the phase pauses and records the design
  decision first.
- No Tokio bridge or ecosystem integration appears.
- `review.md` records the replacement evidence, fair Tokio comparisons, API
  friction, and remaining non-claims.
- Gemini can document a real try-it-and-replace-it path instead of inventing a
  docs story around smaller proof snippets.
- `make verify` passes.
