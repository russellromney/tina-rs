# Phase 051: Ecosystem Bridge Adapters

## Goal

Make Tokio-shaped ecosystem packages fit around Tina easily.

Native Tina is one path. Bridge path is the adoption path.

051 answers:

> Can a normal Rust shop keep using Tokio ecosystem tools at the edge while
> Tina owns the safer bounded service core?

Near-grug:

> Tokio may speak ecosystem. Tina owns state. Bridge shows pressure.

Bridge crates are adoption crates. They are not the native Tina path.

Nearer-grug:

> Bridge may adapt. Bridge may not lie.

## Baseline

Already exists:

- `tina-tokio-bridge`;
- bounded ingress;
- timeout/cancel policy;
- `tina-rpc-tokio` is mostly done as part of the RPC usability work;
- Axum counter comparison;
- WebSocket room comparison;
- Eiffel findings about bridge lifecycle and two-runtime confusion.

Expected before or during this phase:

- 047 bridge lifecycle cleanup;
- tracing context basics from 049 if ready;
- stable pressure vocabulary: `Full`, `Closed`, `Timeout`.

## Coordination

051 can start now.

Coordinate with:

- 047 for bridge lifecycle and shutdown shape;
- 049 for tracing context;
- 048 for native HTTP docs contrast.

Adapter dependencies should be optional features. Native Tina crates must not
pull Axum, Hyper, Reqwest, SQLx, or Tokio-postgres by default.
AWS SDK dependencies must stay in the AWS bridge crate only.

## Non-Goals

- No native HTTP server. That is 048.
- No claim that bridge is pure Tina.
- No hidden unbounded queue between Tokio and Tina.
- No adapter that loses cancellation/deadline semantics.
- No broad support for every crate in Rust.
- No rewrite of Axum, Hyper, Tower, Reqwest, SQLx, or Tonic.
- No rewrite of the AWS SDK, SigV4 signing, credential loading, or AWS service
  protocols.

## Rules

- Bridge boundary must say who owns what.
- Tokio may own sockets/tasks for ecosystem package integration.
- Tina owns bounded service state behind the bridge.
- `Full`, `Closed`, and `Timeout` must map to caller-visible outcomes.
- Shutdown must be one-call boring where possible.
- Cancellation and deadline mapping must be tested, not guessed.
- Tracing context may cross the bridge, but must not become hidden global state.
- Adapter dependencies must not leak into native Tina runtime crates.
- Domain bridge crates should stay small: adapter glue, not a new framework.
- A bridge crate may be disposable when a native Tina crate becomes good enough.
- Every bridge crate must document preserved Tina guarantees and weakened Tina
  guarantees.

## Crate Shape

Keep one generic bridge crate:

- `tina-tokio-bridge` — generic Tokio/Tina host, bounded call, timeout,
  shutdown/drain, health, metrics, and `Full`/`Closed`/`Timeout` mapping.

Add small domain bridge crates only where they remove real ceremony:

- `tina-tower-bridge` — expose Tina services as `tower::Service`.
- `tina-aws-bridge` — bounded AWS SDK worker/pool for S3/DynamoDB/SQS first
  forms.
- `tina-reqwest-bridge` — bounded outbound HTTP worker/pool around `reqwest`.
- `tina-sqlx-bridge` — bounded DB worker/pool around SQLx.
- `tina-rpc-tokio` — async Tokio facade over native Tina RPC.
- later: `tina-smol-bridge` — same generic bridge idea for `smol`/`async-io`
  apps, once Tokio adoption path is boring.

Do not create `tina-axum-bridge` first. If `tina-tower-bridge` is good, Axum can
be an example/helper. Create an Axum-specific crate only if repeated real use
shows Tower is too raw.

## Order

Already / landing:

- bridge lifecycle cleanup in `tina-tokio-bridge`;
- `tina-rpc-tokio` async facade over native RPC.

First remaining bridge crates:

- `tina-tower-bridge`;
- `tina-reqwest-bridge`;
- `tina-sqlx-bridge` first form;
- `tina-aws-bridge`.

Then:

- Axum examples/helpers on top of Tower;
- WebSocket bridge adapter if Eiffel keeps showing the need;
- `tina-smol-bridge` after the Tokio bridge path is boring.

Hyper-specific bridge waits unless Tower proves insufficient.

## Rocks

1. **Bridge Lifecycle Cleanup**

   Consume the 047 bridge work or finish it here if not done.

   Requirements:

   - close/drain/shutdown on bridge host/handle;
   - no `Arc::try_unwrap` dance in examples;
   - pending calls settle visibly;
   - terminal trace/report remains available;
   - docs show the normal lifecycle.

2. **`tina-rpc-tokio` Landing Check**

   Treat the mostly-done RPC Tokio facade as the first domain bridge proof.

   Requirements:

   - native Tina RPC remains the source of truth;
   - async call shape for Tokio users:

     ```rust
     rpc.call("billing", "charge", &amount)
         .deadline(Duration::from_millis(250))
         .await
     ```

   - one request maps to one bounded Tina call/reply path;
   - dropped future cancellation rule documented;
   - late reply discarded visibly;
   - no silent retry;
   - typed serialization errors distinct from `Full`/`Closed`/`Timeout`;
   - crate docs name preserved and weakened Tina guarantees.

   This rock may be a review/finish rock if the crate lands through 058.

3. **`tina-tower-bridge`**

   Build a small `tower::Service` adapter.

   Requirements:

   - readiness reflects bounded Tina ingress where possible;
   - call maps to Tina request/reply;
   - `Full` maps to an explicit busy/error by default;
   - `Poll::Pending` is allowed only if readiness is proven not to hide
     pressure behind an unbounded wait;
   - timeout/deadline behavior is explicit;
   - cancellation before admission vs. after admission is tested;
   - request id/trace context is carried as optional metadata, not a global.

   Target caller shape:

   ```rust
   let mut svc = TinaTowerService::new(bridge_handle, policy);
   let response = svc.call(request).await?;
   ```

   Target bridge shape:

   ```text
   Tower caller / Axum / Hyper
     -> tower::Service::poll_ready checks Tina admission policy
     -> tower::Service::call performs bounded Tina bridge call
     -> Tina service isolate handles request
     -> response/error maps back to Tower future
   ```

   Pressure rule:

   ```text
   Tina Full -> Tower busy/error, not hidden pending forever
   Tina Closed -> Tower closed/error
   Tina Timeout -> Tower timeout/error
   ```

   `poll_ready` must not become a secret queue. If it returns `Pending`, the
   implementation must explain what is being waited on and why it is bounded.

4. **`tina-reqwest-bridge`**

   Bridge path for teams that need mature outbound HTTP client features now.

   First form is concrete, not clever:

   ```text
   crate: tina-reqwest-bridge
   exports:
     ReqwestWorker
     ReqwestConfig
     ReqwestMsg
     ReqwestRequest
     ReqwestResponse
     ReqwestError
     ReqwestMetrics
   ```

   Requirements:

   - Tokio/reqwest worker owns reqwest client;
   - config either takes an existing `reqwest::Client` or builds one from
     `ReqwestConfig`;
   - config names `mailbox_capacity`, `max_in_flight`,
     `request_body_limit`, `response_body_limit`, `default_timeout`,
     redirect policy, and retry policy;
   - Tina submits bounded outbound request work;
   - full/closed/timeout outcomes visible;
   - response body limit explicit;
   - redirect policy explicit;
   - retry policy absent by default or explicitly configured;
   - cancellation and shutdown tested;
   - docs say this is bridge path, not native Tina HTTP client.

   Target Tina call shape:

   ```rust
   call(
       http,
       ReqwestMsg::Send {
           method,
           url,
           headers,
           body,
       },
       Duration::from_secs(2),
   )
   .reply(AppMsg::HttpReturned)
   ```

   Target bridge shape:

   ```text
   Tina service
     -> bounded call to ReqwestWorker/ReqwestPool
     -> Tokio/reqwest performs outbound HTTP request
     -> capped response returns to Tina continuation
   ```

   First form should be full-response, not streaming:

   ```text
   operations:
     Send { request }

   request:
     method
     url
     headers
     body Vec<u8>
     timeout override optional

   response:
     status
     headers
     body Vec<u8>, capped by response_body_limit

   errors:
     Full
     Closed
     Timeout
     RequestTooLarge
     ResponseTooLarge
     InvalidRequest
     Reqwest

   no streaming
   no hidden redirect/retry unless configured
   no unbounded waiters
   ```

   Dropped caller rule:

   - before admission: no work starts;
   - after admission but before reqwest accepts: best-effort cancel;
   - after reqwest accepts: timeout/cancel may only stop waiting. Late result is
     discarded and counted in metrics.

   Required proof:

   - fake local HTTP server test for success;
   - response cap test;
   - full/timeout/closed tests;
   - dropped-caller/late-result behavior test;
   - shutdown/drain report test;
   - one small Tina service example that calls the bridge through
     `call(...).reply(...)`.

   Native 048 HTTP client remains the Tina-owned path. This bridge is for
   mature ecosystem behavior now.

5. **`tina-sqlx-bridge`**

   DB is the production adoption trap. Provide a runnable first adapter shape.

   First form is concrete Postgres only. Do not start generic over every
   `sqlx::Database`; that will turn a bridge into an abstraction contest.

   ```text
   crate: tina-sqlx-bridge
   first feature: postgres
   exports:
     PgDbWorker
     PgDbConfig
     PgDbMsg
     PgDbRequest
     PgDbResponse
     PgDbValue
     PgDbError
     PgDbMetrics
   ```

   The user passes an existing `sqlx::PgPool` or a config that builds one.
   Pool size remains visible as SQLx pool config; Tina-facing ingress remains
   visible as mailbox/worker config. Both matter.

   Requirements:

   - bounded DB worker ingress;
   - query timeout;
   - connection-pool capacity visible;
   - pool busy/full outcomes visible;
   - closed connection outcome visible;
   - row/body size limits named where practical;
   - transaction story either first-form or explicit non-goal;
   - cancellation behavior documented: after SQLx has accepted a query, what can
     and cannot be stopped;
   - example shows Tina service state calling DB through bounded bridge.

   Target Tina call shape:

   ```rust
   call(
       db,
       PgDbMsg::Execute {
           sql,
           params,
       },
       Duration::from_millis(250),
   )
   .reply(AppMsg::DbReturned)
   ```

   Target bridge shape:

   ```text
   Tina service
     -> bounded call to DbWorker/DbPool
     -> Tokio/SQLx pool performs query/execute
     -> result summary or capped rows return to Tina continuation
   ```

   First form should stay narrow:

   ```text
   operations:
     Execute { sql, params }
     FetchOne { sql, params, row_size_limit }
     FetchMany { sql, params, max_rows, row_size_limit }

   params:
     first form uses PgDbValue enum, not user structs:
       Null
       Bool
       I64
       F64
       String
       Bytes

   rows:
     Vec<PgDbRow>
     PgDbRow = Vec<(String, PgDbValue)>
     no derive-to-user-struct in first form

   results:
     rows_affected for Execute
     zero-row / too-many-row outcomes explicit for FetchOne
     row_count and truncated flag for FetchMany

   limits:
     explicit pool capacity
     explicit query timeout
     max rows
     per-row/per-field byte caps where practical

   non-goals:
     transactions
     row streaming
     user struct mapping
     generic SQLx database support
     database cancellation guarantee
   ```

   SQLx cancellation is not magic. If the query has reached SQLx/database, the
   first form treats timeout as "Tina stopped waiting." Late result is discarded
   and counted. Database-side cancellation can be a later explicit operation
   after Postgres cancellation semantics are designed.

   Required proof:

   - compile-gated Postgres tests or a local Postgres/SQLx test fixture if CI
     supports it;
   - pure unit tests for value conversion and row caps;
   - fake worker tests for full/closed/timeout and dropped caller behavior;
   - one runnable example with a Tina service calling `Execute` and
     `FetchOne`;
   - docs naming preserved Tina guarantees and weakened SQLx/DB guarantees.

6. **`tina-aws-bridge`**

   The founding wound bridge: AWS SDK calls under Tokio/Hyper can overload in
   ways that show up as thread contention, latency cliffs, and mystery queues.
   This bridge contains that behind Tina budgets.

   Requirements:

   - AWS SDK stays underneath; Tina does **not** rebuild SigV4, credentials, or
     service protocols here;
   - bridge owns or is given one Tokio runtime/handle for AWS SDK work;
   - one bounded Tina-facing AWS worker/pool address;
   - explicit `max_in_flight`;
   - explicit `max_waiters` or bounded mailbox ingress;
   - explicit per-operation timeout;
   - SDK retry policy disabled by default or surfaced as capped config;
   - operation enum starts narrow:
     - S3 `PutObject`, `GetObject`, `DeleteObject` first;
     - DynamoDB `GetItem`/`PutItem` first if needed;
     - SQS `SendMessage`/`ReceiveMessage` first if needed;
   - request/body size limits named where practical;
   - response body cap for S3 `GetObject`;
   - dropped caller behavior documented:
     - no magical AWS cancel guarantee after SDK accepts work;
     - late result is discarded or counted visibly;
   - shutdown/drain cancels what can be cancelled and reports what remains;
   - metrics are first-class:
     - accepted;
     - full;
     - closed;
     - timeout;
     - sdk_error;
     - retry_count;
     - in_flight;
     - queue_depth;
     - latency.

   Target Tina call shape:

   ```rust
   call(
       aws,
       AwsMsg::S3PutObject { bucket, key, body },
       Duration::from_secs(2),
   )
   .reply(AppMsg::S3PutDone)
   ```

   Target bridge shape:

   ```text
   Tina service
     -> bounded call to AwsWorker/AwsPool
     -> Tokio/AWS SDK performs AWS request
     -> result returns to Tina continuation
   ```

   First proof should include a fake/local AWS endpoint if possible
   (LocalStack, MinIO for S3, or a small mock Smithy/HTTP server) so CI does not
   require real AWS credentials. A real AWS/Fly load probe can come later.

7. **Axum Helper/Example**

   Make the existing good path more copyable without creating a crate too early.

   Requirements:

   - example handler with `bridge.call(req).await`;
   - mapping table: Tina outcomes to HTTP status;
   - docs for cloning handles and shutdown;
   - no Axum-specific crate unless Tower helper is painful in practice.

   Target shape:

   ```rust
   async fn handler(
       State(tina): State<TinaAxumState>,
       Json(req): Json<AppRequest>,
   ) -> Result<Json<AppReply>, StatusCode> {
       tina.call(req).await.map(Json).map_err(map_tina_error)
   }
   ```

   This should be example/helper glue over `tina-tower-bridge` or
   `tina-tokio-bridge`, not a new crate first.

8. **WebSocket Bridge Adapter**

   Improve the current WebSocket comparison into a reusable pattern if needed.

   Requirements:

   - inbound/outbound halves have bounded pressure;
   - slow reader behavior explicit;
   - close handshake behavior documented;
   - Tina room/session core remains isolate-owned.

   Target bridge shape:

   ```text
   Tokio WebSocket edge
     -> bounded inbound messages to Tina session/room isolates
     -> bounded outbound queue per peer
     -> slow reader gets visible full/close policy
   ```

   First form should avoid pretending bidirectional streams are easy:

   ```text
   inbound capacity
   outbound capacity per peer
   max message size
   slow-reader policy
   close policy
   shutdown policy
   ```

9. **Tracing And Context Across Bridge**

   Requirements:

   - request id / trace context can enter Tina call metadata;
   - Tina outcome fields can be emitted through `tracing`;
   - context propagation is optional;
   - simulator/replay story is documented.

10. **`tina-smol-bridge` Sketch**

   Later, maybe. Keep Tina runtime-neutral in posture.

   Requirements:

   - `smol`/`async-io` apps can call Tina services through bounded bridge calls;
   - no Tokio dependency;
   - same pressure vocabulary as `tina-tokio-bridge`;
   - sketch only unless a real app/example needs it.

## IDD Slices

Ship as small PRs. Do not combine unrelated bridge crates just because they are
all bridges.

1. **051A — Bridge Lifecycle And Contract Docs**

   Scope:

   - confirm `tina-tokio-bridge` lifecycle cleanup from 047;
   - docs page for generic bridge contract;
   - table of preserved vs weakened Tina guarantees;
   - shutdown/drain tests if not already landed.

   Done when:

   - examples no longer need `Arc::try_unwrap` shutdown dances;
   - pending bridge calls settle visibly;
   - terminal report/trace remains available.

2. **051B — `tina-rpc-tokio` Landing Review**

   Scope:

   - finish or review the mostly-done RPC Tokio facade;
   - prove dropped future behavior;
   - prove `Full`/`Closed`/`Timeout` mapping;
   - docs say this is async edge facade over native Tina RPC.

   Done when:

   - Tokio caller can `await` a native Tina RPC call;
   - no hidden queue or retry exists;
   - cancellation/late reply behavior is tested.

3. **051C — `tina-tower-bridge`**

   Scope:

   - one small crate;
   - `tower::Service` wrapper over a Tina bridge handle;
   - readiness/backpressure contract;
   - minimal Axum example may use this crate, but no Axum crate.

   Done when:

   - Tower service maps Tina `Full`, `Closed`, and `Timeout` explicitly;
   - readiness does not hide unbounded waiting;
   - cancellation before/after admission is tested.

4. **051D — `tina-reqwest-bridge`**

   Scope:

   - one small crate;
   - bounded outbound HTTP request worker/pool around `reqwest`;
   - response body cap;
   - redirect/retry policy explicit.

   Done when:

   - Tina service can call outbound HTTP through bounded bridge;
   - overload is visible;
   - shutdown cancels or drains honestly;
   - docs contrast native Tina HTTP client from bridge reqwest worker.

5. **051E — `tina-sqlx-bridge` First Form**

   Scope:

   - one small crate or runnable example if crate is too much for first PR;
   - bounded query worker/pool around SQLx;
   - timeout and pool capacity visible;
   - transaction non-goal or first-form explicit.

   Done when:

   - Tina service can submit a bounded DB query;
   - pool busy/full is not hidden;
   - SQLx cancellation limits are documented.

6. **051F — `tina-aws-bridge`**

   Scope:

   - one small crate;
   - bounded AWS SDK worker/pool;
   - S3 first form preferred because it tests body size and network pressure;
   - DynamoDB/SQS can be added if the first slice is still small;
   - explicit SDK retry policy;
   - metrics/pressure report.

   Done when:

   - Tina service can submit bounded AWS operation work;
   - `max_in_flight` and bounded ingress are tested;
   - timeout and late-result behavior are tested;
   - SDK retry count is visible or retries are disabled;
   - CI uses fake/local endpoint, not real AWS credentials.

7. **051G — Optional Bridge Follow-Ups**

   Scope:

   - Axum helper/example on top of Tower;
   - WebSocket adapter only if repeated use needs it;
   - `tina-smol-bridge` sketch only.

   Done when:

   - follow-ups either land as small examples/docs or move to a later phase.

## DAG

```text
051A lifecycle/docs
  ├─> 051B tina-rpc-tokio review
  ├─> 051C tower bridge
  ├─> 051D reqwest bridge
  ├─> 051E sqlx bridge
  └─> 051F aws bridge

051C tower bridge
  └─> 051G Axum helper/example

051D reqwest bridge
  └─> later native-vs-bridge outbound HTTP docs

051E sqlx bridge
  └─> informs 055 native DB

051F aws bridge
  └─> later AWS/Fly overload probe
```

051B/051C/051D/051E/051F can run mostly in parallel after 051A names the generic
bridge contract. They should not share implementation files except workspace
metadata and shared docs.

## Required Proof

- Bridge examples compile and run.
- Adapter crates/features do not affect default native Tina builds.
- Generic bridge docs list preserved and weakened guarantees.
- Tower example shows `Full`, `Closed`, and `Timeout` mapping.
- AWS bridge worker proves bounded AWS operation admission and timeout without
  real AWS credentials in CI.
- Reqwest bridge worker proves bounded outbound HTTP call.
- SQLx bridge proves bounded DB query call or lands with a clearly marked
  runnable first slice plus explicit non-goals.
- RPC Tokio facade proves async call maps to native Tina RPC without hiding
  pressure.
- Shutdown tests prove bridge closes cleanly.
- Docs clearly separate native Tina path from bridge path.

## Done Means

- Tina can enter real Tokio-shaped apps without shame.
- Users can keep ecosystem packages at the boundary while Tina owns bounded
  state and pressure.
- Bridge path is honest: useful, not pure, not hidden.
