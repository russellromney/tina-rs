# Phase 051: Ecosystem Bridge Adapters

## Goal

Make Tokio-shaped ecosystem packages fit around Tina easily.

Native Tina is one path. Bridge path is the adoption path.

051 answers:

> Can a normal Rust shop keep using Tokio ecosystem tools at the edge while
> Tina owns the safer bounded service core?

Near-grug:

> Tokio may speak ecosystem. Tina owns state. Bridge shows pressure.

## Baseline

Already exists:

- `tina-tokio-bridge`;
- bounded ingress;
- timeout/cancel policy;
- Axum counter comparison;
- WebSocket room comparison;
- Eiffel findings about bridge lifecycle and two-runtime confusion.

Expected before or during this phase:

- 047 bridge lifecycle cleanup;
- tracing context basics from 049 if ready;
- stable pressure vocabulary: `Full`, `Closed`, `Timeout`.

## Non-Goals

- No native HTTP server. That is 048.
- No claim that bridge is pure Tina.
- No hidden unbounded queue between Tokio and Tina.
- No adapter that loses cancellation/deadline semantics.
- No broad support for every crate in Rust.
- No rewrite of Axum, Hyper, Tower, Reqwest, SQLx, or Tonic.

## Rules

- Bridge boundary must say who owns what.
- Tokio may own sockets/tasks for ecosystem package integration.
- Tina owns bounded service state behind the bridge.
- `Full`, `Closed`, and `Timeout` must map to caller-visible outcomes.
- Shutdown must be one-call boring where possible.
- Cancellation and deadline mapping must be tested, not guessed.
- Tracing context may cross the bridge, but must not become hidden global state.

## Rocks

1. **Bridge Lifecycle Cleanup**

   Consume the 047 bridge work or finish it here if not done.

   Requirements:

   - close/drain/shutdown on bridge host/handle;
   - no `Arc::try_unwrap` dance in examples;
   - pending calls settle visibly;
   - terminal trace/report remains available;
   - docs show the normal lifecycle.

2. **Tower Service Adapter**

   Build a small `tower::Service` adapter.

   Requirements:

   - readiness reflects bounded Tina ingress where possible;
   - call maps to Tina request/reply;
   - `Full` maps to `Poll::Pending`, error, or typed busy by documented policy;
   - timeout/deadline behavior is explicit;
   - cancellation before admission vs. after admission is tested.

3. **Axum Helper**

   Make the existing good path more copyable.

   Requirements:

   - helper/state wrapper for `Router::with_state`;
   - example handler with `bridge.call(req).await`;
   - mapping table: Tina outcomes to HTTP status;
   - docs for cloning handles and shutdown.

4. **Hyper Service Bridge**

   Support lower-level Hyper users without Axum.

   Requirements:

   - request head/body limits explicit;
   - response maps typed Tina reply;
   - service full maps to 503 or caller policy;
   - no hidden body buffering.

5. **Reqwest / Outbound HTTP Bridge Worker**

   Bridge path for teams that need mature HTTP client features now.

   Requirements:

   - Tokio/reqwest worker owns reqwest client;
   - Tina submits bounded outbound request work;
   - full/closed/timeout outcomes visible;
   - response body limit explicit;
   - cancellation and shutdown tested;
   - docs say this is bridge path, not native Tina HTTP client.

6. **SQLx / Tokio-Postgres Bridge Sketch**

   DB is the production adoption trap. Provide a first adapter shape.

   Requirements:

   - bounded DB worker ingress;
   - query timeout;
   - connection-pool capacity visible;
   - full/closed/busy outcomes visible;
   - result row/body size limits named;
   - transaction story either first-form or explicit non-goal.

   This may start as a sketch/example if full adapter is too large.

7. **WebSocket Bridge Adapter**

   Improve the current WebSocket comparison into a reusable pattern.

   Requirements:

   - inbound/outbound halves have bounded pressure;
   - slow reader behavior explicit;
   - close handshake behavior documented;
   - Tina room/session core remains isolate-owned.

8. **Tracing And Context Across Bridge**

   Requirements:

   - request id / trace context can enter Tina call metadata;
   - Tina outcome fields can be emitted through `tracing`;
   - context propagation is optional;
   - simulator/replay story is documented.

## Required Proof

- Bridge examples compile and run.
- Axum and Tower examples show `Full`, `Closed`, and `Timeout` mapping.
- Reqwest bridge worker proves bounded outbound call.
- SQLx/tokio-postgres bridge is either runnable first form or documented sketch
  with clear missing work.
- Shutdown tests prove bridge closes cleanly.
- Docs clearly separate native Tina path from bridge path.

## Done Means

- Tina can enter real Tokio-shaped apps without shame.
- Users can keep ecosystem packages at the boundary while Tina owns bounded
  state and pressure.
- Bridge path is honest: useful, not pure, not hidden.
