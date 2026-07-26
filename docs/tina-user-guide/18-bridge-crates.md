# Bridge Crates

Native Tina is one path. Bridges are the adoption path — they let
Tokio-shaped ecosystem packages live next to a Tina core without
either side lying about pressure.

The rule:

> Tokio may speak ecosystem. Tina owns state. Bridge shows pressure.
> Bridge may adapt. Bridge may not lie.

If you can use a native Tina crate, do. Native WebSocket server
upgrade now lives in `tina-http`: HTTP/1.1 `GET` upgrade validation,
Tina-owned TCP/TLS rails after handoff, bounded frame/message/queue
limits, visible ping/pong and close messages, client masking
validation, and unmasked server frames. It is not HTTP/2 WebSocket,
permessage-deflate, a browser session framework, or a broad client
crate. For the bounded room/fanout copy path, see
[Native WebSocket Server](20-native-websocket-server.md). Native HTTP/2 has a
server h2c path in `tina-http::Http2Listener` and a client path in
`tina-http::Http2ClientConnection`: bounded stream tables, explicit
connection/stream flow-control windows, typed protocol errors, streaming
request/response bodies, and protocol facts. The client can target h2c or
h2/TLS with explicit ALPN through `Http2Target`. Native gRPC layers unary,
server-streaming, client-streaming, and bidirectional `prost` messages on those
HTTP/2 pieces through `GrpcRouter` and `GrpcClient`: typed request pulls,
typed `GrpcStatus` trailers, message caps, no compression, and deadline
mapping. The specimens prove tonic `0.12` h2c interop for the core modes. It
is not tonic feature parity, not grpcurl reflection, not pooled production
gRPC clients, and not HTTP/2 mTLS.
Native WebSocket client sessions live in `tina-http` too:
`WebSocketClientConnection` owns a TCP/TLS rail, performs the HTTP/1.1
upgrade, masks outbound client frames, auto-answers ping with pong, exposes
typed send/receive/report calls, and emits WebSocket close facts. It is a
native session, not a reconnecting client manager and not a bridge. Native
HTTPS/1.1 lives in `tina-http`'s
`HttpsListener` and `HttpClient` — explicit DER cert config, typed
startup, matchable TLS errors. For repeated outbound
requests against the same origin,
`tina_http::InstallKeepalivePool::install_keepalive_pool` installs the
pool on a `LocalSystem` and hands you an owned `InstalledKeepalivePool`
handle (`pool.pool()` / `pool.connections()`): one TCP
(or TLS) connection per pool slot serves many requests, with
`acquire` / `release` / `retire` / `close` and a pressure report.
Each connection isolate is bound to one origin at construction —
scheme + `SocketAddr` + (HTTPS) SNI + the configured DER trust
roots themselves — so cross-origin reuse cannot happen at the
connection-isolate level. The recommended consumer pattern is
always release `Reuse`; the connection self-heals on
`must_retire = true` (drops the bad transport, reconnects on the
next request). On shutdown, the consuming
`pool.close_and_drain(timeout)` closes lease admission, waits for
leased connections to return, then stops each connection isolate;
on deadline it returns `KeepaliveCloseAndDrain::TimedOut { pool,
pending }` and keeps the handle so you can retry later. There is no
public force-close on this facade. (The raw-runtime free functions
`build_keepalive_pool` / `shutdown_keepalive_pool` still exist for
`ThreadedRuntime` consumers; they are not the facade form.)
Reach for a bridge when you need the
broader ecosystem behavior Tina has not chosen to own natively:
system trust roots, redirects/cookies, proxies, existing Axum/Tower
apps, or a third-party SDK that only ships a Tokio client.

## What ships today

| Crate | Direction | Used when |
| --- | --- | --- |
| `tina-tokio-bridge` | Tokio caller → Tina isolate | A Tokio handler needs a bounded request/reply path into a Tina service. |
| `tina-tower-bridge` | `tower::Service` over a Tina bridge | An Axum/Hyper/Tower stack wants to call a Tina service through normal Tower middleware. |
| `tina-rpc-tokio` | Tokio caller → `tina-rpc` framed client | A Tokio task wants an `await`able shape over a registered `tina_rpc::Client` (correlator demux, bounded admission, opt-in retry). Wraps the existing client; does not own the wire. |
| `tina-reqwest-bridge` | Tina caller → outbound HTTP via `reqwest` | A Tina service needs redirects, cookies, system trust roots, proxy/middleware behavior, or other mature web-client behaviour. Native `tina-http` covers HTTP/1.1, HTTPS/1.1, HTTP/2 h2c/h2-TLS client basics, keepalive pools, and protocol facts; reqwest covers the broad web-client ecosystem. |
| `tina-sqlite-bridge` | Tina caller → SQLite via `rusqlite` | A Tina service needs an in-process SQL database. SQLite is sync C; the bridge owns one connection on a blocking std thread. Autocommit only; no pool, no transactions in first form. |
| `tina-sqlx-bridge` | Tina caller → Postgres via `sqlx::PgPool` | A Tina service needs to reach a real Postgres without blocking shard threads. Two-runtime cost: the bridge spawns SQLx work on Tokio. Postgres-first. Ships `Execute`, `FetchOne`, bounded `FetchMany`, and atomic-script `Transaction`. Generic `sqlx::Database`, ORM, migrations, and user-struct row mapping stay non-goals. |
| `tina-aws-bridge` | Tina caller → AWS SDK S3/SQS/DynamoDB/SNS/Secrets Manager | A Tina service needs AWS SDK behavior without letting AWS/Hyper/Tokio pressure become invisible. Ships S3 (`PutObject`, bounded `GetObject`, `HeadObject`, `DeleteObject`), SQS (`SendMessage`, `ReceiveMessage`, `DeleteMessage`), DynamoDB (`GetItem`, `PutItem`, `UpdateItem`, `DeleteItem`, `Query` with typed capacity facts), SNS (`Publish`), and Secrets Manager (`GetSecretValue`). The SDK still owns SigV4, credentials, HTTP, TLS, endpoints, and service protocols. |

Each crate is small, opt-in, and bounded. Native Tina crates
(`tina-http`, etc.) do not depend on any bridge; bridges do not leak
into the native runtime.

## Two error layers

Every bridge has two distinct failure layers, and the bridge is not
allowed to collapse them silently:

- **Bridge delivery**: did the IsolateCall reach the worker isolate?
  Outcomes are `CallOutcome::Full` / `Closed` / `Timeout`.
- **Worker outcome**: the worker accepted the call and produced a typed
  result. Domain-specific errors live here (HTTP body too large, bad
  URL, transport failure, etc.).

The default reply shape preserves both layers:

```rust
AppMsg::HttpReturned(outcome: CallOutcome<Result<MyResponse, MyError>>)
```

Some crates ship an opt-in `flatten_outcome(...)` helper for app-edge
code that does not need to distinguish the two layers. The flat error
type still names which layer failed; it never collapses them into one
variant. Use the layered shape unless your call site is shorter and
clearer with the flat one.

## Canonical shapes

### `tina-tokio-bridge` — Tokio → Tina

Tokio code holds a `BridgeHandle`, which is the Tokio-side proxy for a
registered Tina isolate. Calling it is one `await`:

```rust
use tina_runtime::LocalSystem;
use tina_tokio_bridge::{BridgeHost, BridgeRequest};

let app = LocalSystem::single_shard(shard, factory)
    .config(local_system_config)
    .try_build()?;
let mut host = BridgeHost::from_app(app);
let bridge = host.register_bridge::<MyService, Req, Reply, Infallible>(
    MyService::default(),
    mailbox_capacity,
    Duration::from_secs(2),
)?;

// Tokio side:
let response = bridge.call(req).await?; // -> Result<Reply, BridgeError>
```

Lifecycle:

```rust
host.drain_and_shutdown(Duration::from_secs(2))?;
```

`BridgeError::{Full, Closed, Timeout}` is caller-visible. `Display` and
`std::error::Error` are implemented so log lines and `BoxError` work.

### `tina-tower-bridge` — Tower over a Tina bridge

Wrap a bridge handle as a `tower::Service`. Drop the wrapped service
into Axum's `State<S>`. Tower middleware (`Timeout`, `ConcurrencyLimit`,
bounded `Buffer`) composes the normal way.

```rust
use tina_tower_bridge::{Service, TinaService, TinaTowerService};

type MyService = TinaService<MyReq, MyReply>;

let svc: MyService = TinaTowerService::new(bridge);
let app = Router::new().route("/x", post(handler)).with_state(svc);

async fn handler(State(svc): State<MyService>) -> Result<String, StatusCode> {
    let mut svc = svc;
    match svc.call(req).await {
        Ok(reply) => Ok(...),
        Err(BridgeError::ForeignSystem { .. }
            | BridgeError::UnknownShard(_)
            | BridgeError::Full
            | BridgeError::Closed) => Err(StatusCode::SERVICE_UNAVAILABLE),
        Err(BridgeError::Timeout) => Err(StatusCode::GATEWAY_TIMEOUT),
    }
}
```

`poll_ready` only signals open vs closed; admission backpressure shows
up on the call future as `Err(BridgeError::Full)`. Never `Pending`.

For per-connection fan-out (e.g. WebSocket reader/writer split), clone
the service; `Service::call`'s `&mut self` is per-clone:

```rust
let mut sub_svc = svc.clone();
sub_svc.call(SubscribeMsg).await?;

let mut publish_svc = svc.clone();
publish_svc.call(PublishMsg).await?;
```

### `tina-reqwest-bridge` — Tina → outbound HTTP

A bounded outbound HTTP worker. Tina services call it through the
normal `call(...).then(...)` path:

```rust
use tina_reqwest_bridge::{ReqwestAddress, ReqwestCallOutcome, ReqwestRequest, send_request};

struct App {
    http: ReqwestAddress,
}

enum AppMsg {
    Start,
    HttpReturned(ReqwestCallOutcome),
}

impl Isolate for App {
    fn handle(&mut self, msg: AppMsg, ctx: &mut Context<'_, _>) -> Effect<Self> {
        match msg {
            AppMsg::Start => send_request(
                self.http,
                ReqwestRequest::get("https://example.com/"),
                Duration::from_secs(2),
            )
            .then(AppMsg::HttpReturned),

            AppMsg::HttpReturned(outcome) => match outcome {
                CallOutcome::Replied(Ok(response)) => { /* success */ }
                CallOutcome::Replied(Err(e)) => { /* worker-level failure */ }
                CallOutcome::Full | Closed | Timeout => { /* bridge-level failure */ }
            },
        }
    }
}
```

Setup uses the `install_local` helper:

```rust
let bridge = ReqwestWorker::<SingleShard>::install_local(&app, ReqwestConfig::default())?;
let app = App { http: bridge.address };
```

### Bridge convention table

What each bridge actually ships. Word matches code; do not invent
a missing column.

| Crate | Install path | Caller-supplied path | Close | Bounded drain | Late-result reporting | Tracing target prefix |
|---|---|---|---|---|---|---|
| `tina-tokio-bridge` | `BridgeHost::register_bridge` returns a `BridgeHandle` | n/a (host owns the Tina runtime; callers own Tokio) | `BridgeHandle::close()` flags closed; subsequent `call`/`call_with_*` returns `BridgeError::Closed`. Health surface is `BridgeHandle::health()` (`Accepting` / `Closed`). | `BridgeHost::drain_and_shutdown(d)` waits up to `d` for all handle clones to drop | `BridgeMetricsSnapshot::dropped_responses` — handler tried to respond after the Tokio caller went away (caller-terminal) | `tina_tokio.bridge.call`, `tina_tokio.bridge` |
| `tina-tower-bridge` | `TinaTowerService::new(handle)` wraps a `BridgeHandle` | inherits whatever the underlying handle has | `TinaTowerService::close()` forwards to the handle | inherits the handle's host-level drain | inherits the handle's `dropped_responses` | `tina_tower.bridge.call`, `tina_tower.bridge` (plus the inner tokio targets) |
| `tina-rpc-tokio` | `BridgeClient::new(runtime, client_addr, max_in_flight, client_max_in_flight)` registers a reply-shim isolate beside an existing `tina_rpc::Client` | n/a (the wrapped `Client` is always caller-owned) | no `close()` — drop the `BridgeClient` (and the underlying client) when done | n/a in first form | no per-bridge late-result counter; late wire replies are dropped at the shim by correlator | `tina_rpc.bridge.call` span (parent of inner events) |
| `tina-reqwest-bridge` | `ReqwestWorker::install_local(&system, cfg)`; `ReqwestWorker::install(&runtime, cfg)` is the lower-level runtime form | `ReqwestWorker::with_supplied_client(cfg, client, handle)` then explicit registration | `ReqwestCloser::close()` flags closed; new sends reply `ReqwestError::Closed`. In-flight tasks run to natural completion or per-attempt timeout (`tokio::time::timeout`, then `AbortHandle::abort`). | no bounded drain helper — drop the runtime to force-cancel | no `late_results` counter: the per-attempt timeout aborts the spawned Tokio task. A reply that arrives after the Tina caller's `IsolateCall` deadline shows as `CallReplyRejected` in the runtime trace. | `tina_reqwest.bridge.call`, `tina_reqwest.bridge` |
| `tina-sqlite-bridge` | `SqliteWorker::install_local(&system, cfg)`; `SqliteWorker::install(&runtime, cfg)` is the lower-level runtime form | n/a (one in-process connection; no pool/handle to supply) | `SqliteCloser::close()` flags closed; the worker thread always finishes its current SQLite call. Drop the bridge isolate to retire the thread at its next `recv`. | n/a — the worker thread is uncancellable C code | `SqliteMetrics::late_results` — worker-terminal landed after the bridge surfaced `SqliteError::Timeout` (also visible in the trace as `CallReplyRejected`) | `tina_sqlite.bridge.call`, `tina_sqlite.bridge` |
| `tina-sqlx-bridge` | `PgWorker::install_local(&system, cfg)` builds a `PgPool` and a small Tokio runtime; `PgWorker::install(&runtime, cfg)` is the lower-level runtime form | `PgWorker::install_local_with_pool(&system, cfg, pool, handle)` or `PgWorker::install_with_pool(&runtime, cfg, pool, handle)` (SQLx settings on the supplied pool stay caller-owned; the supplied Tokio runtime is never shut down by the bridge) | `PgCloser::close()` flags closed; does **not** close the SQLx pool. Owned pool drops with the bridge; supplied pool stays caller-owned. | no bounded drain helper; SQLx queries keep running until natural completion | `PgMetrics::late_results` — spawned SQLx task completed after the bridge surfaced `PgError::Timeout`. Does **not** count Postgres-side execution that continues past the future drop, nor the caller-observed `CallOutcome::Timeout` path (that lives in the trace as `CallReplyRejected`). | `tina_sqlx.bridge.call`, `tina_sqlx.bridge` |
| `tina-aws-bridge` (S3) | `install_s3_local(&system, cfg)` builds an SDK client and a small Tokio runtime behind `LocalSystem`; `install_s3(&runtime, cfg)` is the lower-level runtime form | `S3Worker::with_supplied_client(cfg, client, handle)` then explicit registration (caller-owned client owns SigV4/credentials/HTTP/TLS/SDK retry; `sdk_max_attempts` reports `0`/unknown). Caller's Tokio runtime is never shut down by the bridge. | `S3Closer::close()` flags closed; new admissions reply `S3Error::Closed` | `S3Closer::close_and_drain(timeout)` waits up to `timeout` for already-admitted SDK work to leave the in-flight set; reports `in_flight_remaining` + per-operation `in_flight_kinds` on deadline. Spawned SDK futures are **not aborted**: a bridge timeout means Tina stopped waiting, not that AWS/Hyper cancelled bytes. | `S3Metrics::late_results` — SDK future terminal after the bridge already surfaced `S3Error::Timeout`. Until the SDK future finishes, it keeps occupying `max_in_flight` capacity. | `tina_aws.bridge.call`, `tina_aws.bridge` |
| `tina-aws-bridge` (SQS) | `install_sqs_local(&system, cfg)`; `install_sqs(&runtime, cfg)` is the lower-level runtime form | `SqsWorker::with_supplied_client(cfg, client, handle)` (same ownership split as S3) | `SqsCloser::close()` flags closed | `SqsCloser::close_and_drain(timeout)` mirrors the S3 shape | `SqsMetrics::late_results` mirrors S3 — SDK terminal after bridge timeout. SQS state (sent, visibility extended, deleted) is **not** rolled back when the bridge stops waiting. | `tina_aws.bridge.call`, `tina_aws.bridge` |
| `tina-aws-bridge` (DynamoDB) | `install_dynamodb_local(&system, cfg)`; `install_dynamodb(&runtime, cfg)` is the lower-level runtime form | `DynamoWorker::with_supplied_client(cfg, client, handle)` (same ownership split as S3) | `DynamoCloser::close()` flags closed | `DynamoCloser::close_and_drain(timeout)` mirrors the S3 shape | `DynamoMetrics::late_results` mirrors S3 — SDK terminal after bridge timeout. DynamoDB mutations (`PutItem`, `UpdateItem`, `DeleteItem`) are **not** rolled back when the bridge stops waiting. | `tina_aws.bridge.call`, `tina_aws.bridge` |
| `tina-aws-bridge` (SNS) | `install_sns_local(&system, cfg)`; `install_sns(&runtime, cfg)` is the lower-level runtime form | `SnsWorker::with_supplied_client(cfg, client, handle)` (same ownership split as S3) | `SnsCloser::close()` flags closed | `SnsCloser::close_and_drain(timeout)` mirrors the S3 shape | `SnsMetrics::late_results` mirrors S3 — SDK terminal after bridge timeout. A `Publish` already accepted by SNS is not undone when the bridge stops waiting. | `tina_aws.bridge.call`, `tina_aws.bridge` |
| `tina-aws-bridge` (Secrets) | `install_secrets_local(&system, cfg)`; `install_secrets(&runtime, cfg)` is the lower-level runtime form | `SecretsWorker::with_supplied_client(cfg, client, handle)` (same ownership split as S3) | `SecretsCloser::close()` flags closed | `SecretsCloser::close_and_drain(timeout)` mirrors the S3 shape | `SecretsMetrics::late_results` mirrors S3 — SDK terminal after bridge timeout. `GetSecretValue` is read-only so no rollback applies; the bridge cap may still surface as `SecretsError::SecretTooLarge`. | `tina_aws.bridge.call`, `tina_aws.bridge` |

> **Late-result vocabulary.** "Late" means three different things and
> the bridge can only see two of them. Read the row that fits the
> bridge you are using before writing a runbook.
>
> 1. **Bridge timeout, worker terminal later** — the bridge stopped
>    waiting (`*Error::Timeout`) and the underlying SDK/SQLx task
>    eventually finished anyway. Where present, this lives in
>    `late_results` (sqlite, sqlx, aws S3 + SQS). reqwest aborts the
>    Tokio task on timeout, so it has no equivalent counter.
> 2. **Caller IsolateCall timeout, bridge reply later** — the Tina
>    caller's outer `CallOutcome::Timeout` fired. The bridge does not
>    see this. The runtime drops the eventual reply as
>    `CallReplyRejected` in the trace; no bridge counter increments.
> 3. **Backend keeps working after the bridge stops observing** —
>    Postgres continues executing the query (default sqlx behaviour),
>    AWS/Hyper finishes the in-flight IO. Neither bridge counts these
>    bytes; the budget surfaces as continued `max_in_flight` capacity
>    occupancy until the spawned task finishes.

> **Supplied-client/pool ownership.** When a bridge accepts a
> caller-supplied client, runtime handle, or pool, the bridge does
> **not** re-apply the matching config fields and does **not** shut
> the supplied runtime down on close. Knobs that are ignored on that
> path are named explicitly in each bridge's docs:
>
> - `tina-reqwest-bridge::ReqwestWorker::with_supplied_client` — the
>   supplied `reqwest::Client` owns redirect policy, the reqwest
>   `Client::timeout`, connection reuse, TLS, and proxy. The bridge
>   still enforces its own `default_timeout` (or per-request
>   override) via `tokio::time::timeout(...)` on every attempt.
> - `tina-sqlx-bridge::PgWorker::install_with_pool` — the supplied
>   `PgPool` owns `max_connections`, `acquire_timeout`, TLS, idle
>   timeout. `PgConfig::pool` and `PgConfig::cancel` are silently
>   ignored on this path. The pool must be built inside an active
>   Tokio context (SQLx 0.8 spawns maintenance tasks at construction).
> - `tina-aws-bridge` S3, SQS, DynamoDB, SNS, and Secrets Manager
>   `with_supplied_client` — the supplied SDK client owns
>   credentials, region, endpoint, HTTP connector, TLS, and the SDK
>   retry policy. The bridge reports `sdk_max_attempts = 0` (unknown)
>   in metrics when the SDK retry policy is caller-owned.
>
> When the bridge owns the client (the plain `install` path), it also
> owns the Tokio runtime and drops it with the worker. The
> supplied-client path never shuts the caller's Tokio runtime down.

The production-shaped copy path for SQLite plus native outbound HTTP is
`examples/systems/mini_saas_api`:

```sh
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- smoke
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- pressure
```

It uses `SqliteWorker` as the honest one-lane pool shape and
`install_keepalive_pool` for outbound notifications. The route code keeps
bridge-layer `Full` / `Closed` / `Timeout` distinct from worker or upstream
failures, and `/debug/capacity` reports DB full/closed/timeout counts beside
outbound keepalive waiters, leases, full, closed, and cancellation counts.

`flatten_outcome(outcome)` is available when the call site does not
need to distinguish bridge-layer from worker-layer failures.

`outcome.classify()` (via the `ReqwestOutcomeExt` trait) is available
for caller-owned retry loops: it returns
`ReqwestOutcomeClass::{Succeeded, Transient(reason), Fatal(reason)}`
where the typed reason still names which layer failed
(`BridgeTimeout` vs `WorkerTimeout`, `BridgeFull` vs `WorkerFull`,
etc.). The classifier does not retry — caller still owns idempotency,
budget, and backoff.

### `tina-sqlx-bridge` — Tina → Postgres

Adoption bridge. SQLx owns the pool, the wire, and TLS. The bridge
owns bounded ingress, the per-attempt deadline, late-result truth,
and typed failures.

```rust
use tina_sqlx_bridge::{PgConfig, PgPoolConfig, PgWorker, execute_call};

let cfg = PgConfig::new()
    .with_pool(PgPoolConfig::new(env::var("DATABASE_URL")?))
    .with_max_in_flight(8)
    .with_default_timeout(Duration::from_secs(2));
let bridge = PgWorker::<SingleShard>::install_local(&system, cfg)?;

// In a handler:
execute_call(self.db, "INSERT INTO t (k, v) VALUES ($1, $2)",
    vec![1.into(), "hello".into()], Duration::from_secs(2))
    .then(AppMsg::Inserted);
```

Runtime-checked SQLx (`sqlx::query(...)`). No `query!` macros, no
offline metadata, no DB needed at compile time.

**Operations.**

| Request | Returns | Notes |
| --- | --- | --- |
| `Execute` | `rows_affected` | row-less statement |
| `FetchOne` | `Row` / `NoRows` / `TooManyRows` | streams at most two rows; never buffers a large result set |
| `FetchMany { max_rows }` | `Rows { rows, truncated }` | reads `max_rows + 1` then stops; effective cap clamped by `max_response_rows` |
| `Transaction { steps }` | `Committed { steps }` / `RolledBack { failed_at, error, completed }` | atomic script; no nesting |

Helpers project the response shape away: `execute_call` → `u64`,
`fetch_one_call` → `Option<PgRow>`, `fetch_many_call` → `PgRows`,
`transaction_call` → `PgTransactionOutcome`. Each helper has a
`PgOutcomeExt::classify` impl that sorts outcomes into Succeeded /
Transient / Fatal for caller-owned retry. Classifier does not
retry.

**Errors.**

```text
mailbox full       -> CallError::TargetFull (Tina ingress)
max_in_flight      -> PgError::Full
per-attempt clock  -> PgError::Timeout
pool acquire clock -> PgError::PoolAcquireTimeout
pool closed        -> PgError::PoolClosed
sqlx error         -> PgError::Sqlx(detail)
decode error       -> PgError::Decode(detail)
too many rows      -> PgError::TooManyRows
worker closed      -> PgError::Closed
```

`Full` is not `PoolAcquireTimeout`. Tina admission and SQLx pool
acquire are different bottlenecks.

**Timeouts.** Three different ones:

- `CallOutcome::Timeout` — the *caller's* IsolateCall deadline.
  Bridge does not see it; the runtime drops the late reply,
  visible in the trace.
- `PgError::Timeout` — the *bridge's* per-attempt deadline. Bridge
  detaches the receiver and replies. The SQLx future runs to
  natural completion; when it does, `late_results` bumps and the
  real outcome lands in the worker-terminal counter.
- `PgError::PoolAcquireTimeout` — SQLx's pool deadline.

**Cancellation.** Postgres keeps running the query
past `PgError::Timeout`. The connection stays held until SQLx
returns. Treat `PgError::Timeout` as "Tina stopped waiting," not
"the database stopped."

`PgConfig::with_cancel_on_timeout(pool_size)` is a compatibility
no-op. The old sidecar pool fired `pg_cancel_backend(pid)` on
timeout, but that path could race connection reuse and cancel a
later query, so the bridge does not fire it. `db_cancels_sent`
stays at zero; Tina-side timeout settles the caller while the SQLx
slot remains occupied until physical terminal. Supplied-pool
installs ignore the setting entirely.

**Two install paths.**

- `install_local(&system, cfg)` — the `LocalSystem` facade form;
  `install(&runtime, cfg)` is the lower-level runtime form. Bridge
  builds the pool and a small Tokio runtime from `cfg`.
- `install_local_with_pool(&system, cfg, pool, handle)` —
  facade form of `install_with_pool(&runtime, cfg, pool, handle)`;
  caller supplies both. SQLx settings on the supplied pool are caller-
  owned. The pool must be built inside an active Tokio context
  (SQLx 0.8 spawns maintenance tasks at construction).

**Value types.** Boring core (`bool`, `i64`, `f64`, `String`,
`Vec<u8>`) ships always. Wider types are cargo features:

- `uuid` — `UUID`
- `json` — `JSON` and `JSONB` (both → `serde_json::Value`; bind
  sends `JSONB`)
- `numeric` — `NUMERIC` (`rust_decimal::Decimal`, ~28 digits)
- `time` — `TIMESTAMP`, `TIMESTAMPTZ`, `DATE`

Each feature pulls the matching SQLx feature. A column whose type
isn't enabled returns `PgError::Decode` with a hint at the cargo
feature that fixes it. Nothing is silently coerced.

**NULLs.** `PgValue::Null` sends `INT8 NULL`. Postgres infers the
actual type from context most of the time. When it can't (a
positional NULL into a non-INT8 column without a SQL cast),
`PgValue::TypedNull(PgType::X)` — or shorthand `PgValue::null_*()`
— sends a NULL with the right wire type oid. Decode always lands
in `PgValue::Null` regardless of how the NULL was bound.

Non-goals: generic `sqlx::Database`, ORM, migrations, struct mapping, and a
transaction *handle* (vs. atomic script). The bridge stays a small typed worker
boundary, not a general SQLx facade.

### `tina-aws-bridge` — Tina → AWS SDK S3/SQS

Adoption bridge. The AWS Rust SDK owns AWS protocol behavior; Tina
owns bounded admission, body/message caps, per-operation timeout truth,
typed outcomes, and metrics.

```rust
use tina_aws_bridge::{
    S3Config, S3Credentials, S3Request, S3PutObject, install_s3_local, send_s3,
};

let cfg = S3Config::new()
    .with_region("us-east-1")
    .with_credentials(S3Credentials::new("access-key-id", "secret-access-key"))
    .with_max_in_flight(8)
    .with_default_timeout(Duration::from_secs(2));
let bridge = install_s3_local(&system, cfg)?;

// In a handler:
send_s3(
    self.aws,
    S3Request::PutObject(S3PutObject {
        bucket: "bucket".into(),
        key: "key".into(),
        body: b"hello".to_vec(),
        content_type: Some("text/plain".into()),
    }),
    Duration::from_secs(2),
)
.then(AppMsg::S3PutDone);
```

The copied raw call shape is:

```rust
call(
    aws,
    S3Msg::Send(S3Request::PutObject(S3PutObject {
        bucket,
        key,
        body,
        content_type: None,
    })),
    Duration::from_secs(2),
)
.then(AppMsg::S3PutDone)
```

**Operations.**

| Request | Returns | Notes |
| --- | --- | --- |
| `PutObject` | `S3PutObjectOk` | Full buffered request body; capped by `request_body_limit`. |
| `GetObject { max_bytes }` | `S3Object` | Reads SDK stream chunk by chunk; fails with `ResponseTooLarge` once the request/config cap would be crossed. |
| `HeadObject` | `S3ObjectHead` | Object metadata without buffering a body. |
| `DeleteObject` | `S3DeletedObject` | Object delete only; no batch/delete framework. |

**Pressure and retry truth.**

```text
mailbox full        -> CallError::TargetFull / CallOutcome::Full
max_in_flight       -> S3Error::Full
request body cap    -> S3Error::RequestTooLarge
response body cap   -> S3Error::ResponseTooLarge
per-operation clock -> S3Error::Timeout
SDK failure         -> S3Error::Sdk(detail)
worker closed       -> S3Error::Closed
```

`S3Config` disables AWS SDK retries by default. If you opt into
`SdkRetryPolicy::Standard { max_attempts }`, one admitted bridge
operation may perform multiple SDK HTTP attempts internally. The
bridge exposes the configured `sdk_max_attempts` in metrics; it does
not claim to observe every internal retry attempt. If you wrap a
caller-supplied `aws_sdk_s3::Client`, SDK retry policy is entirely
caller-owned and `sdk_max_attempts` is reported as `0` (unknown).

`install` builds an S3 client from explicit bridge config. First form
supports static credentials only (`S3Credentials`). Use
`with_supplied_client` when you need the AWS default provider chain,
assume-role, SSO, custom HTTP connector, custom TLS, proxy policy, or
any other SDK-owned client setup.

No unbounded request/result buffer is part of the bridge: callers are
bounded by the Tina mailbox, SDK work is bounded by `max_in_flight`,
and the bridge does not queue waiters behind saturated in-flight
slots.

**Cancellation.** `S3Closer::close()` stops Tina-side admission.
`S3Closer::close_and_drain(timeout)` closes admission and waits a
bounded time for already accepted SDK work to leave the bridge's
in-flight set. If the deadline fires, `S3DrainReport` names the
remaining operation kinds and count. On bridge timeout, the caller can
stop waiting; the spawned SDK future is left alive because task abort
does not prove AWS/Hyper cancelled bytes already accepted for IO. That
late SDK future continues to occupy `max_in_flight` capacity until it
reports terminal truth; then worker-terminal metrics are tallied and
`late_results` increments.

Owned install builds a private Tokio runtime for SDK work and drops it
with the worker. The supplied-client path uses the caller's Tokio
runtime handle and never shuts it down.

SQS follows the same lifecycle vocabulary with SQS-shaped types:

```rust
use tina_aws_bridge::{
    SqsConfig, SqsCredentials, SqsRequest, SqsSendMessage, install_sqs_local, send_sqs,
};

let cfg = SqsConfig::new()
    .with_region("us-east-1")
    .with_credentials(SqsCredentials::new("access-key-id", "secret-access-key"))
    .with_message_body_limit(64 * 1024)
    .with_max_receive_messages(10);
let bridge = install_sqs_local(&system, cfg)?;

send_sqs(
    self.sqs,
    SqsRequest::SendMessage(SqsSendMessage {
        queue_url,
        body: "hello".into(),
        message_group_id: None,
        message_deduplication_id: None,
    }),
    Duration::from_secs(2),
)
.then(AppMsg::SqsSendDone);
```

SQS receive names visibility timeout explicitly and never auto-deletes:

```rust
SqsRequest::ReceiveMessage(SqsReceiveMessage {
    queue_url,
    max_messages: 5,
    visibility_timeout_seconds: 30,
    wait_time_seconds: 0,
})
```

`SqsResponse::ReceivedMessages` may contain an empty vector; that is a
successful empty receive, not an error. Each returned `SqsMessage`
carries the `receipt_handle` the caller must pass to
`SqsRequest::DeleteMessage` if the message should be deleted. The
bridge does not retry sends or deletes and does not infer idempotency
from FIFO deduplication fields; retry budget, backoff, and idempotency
keys remain caller-owned.

`SqsCloser::close()` stops new Tina-side admission.
`SqsCloser::close_and_drain(timeout)` reports any accepted SQS work
still in flight by operation kind, just like S3. If a bridge timeout
fires after SDK acceptance, Tina stops waiting but SQS has not rolled
back the send, receive visibility change, or delete. The SDK future is
left to finish so late-result metrics and in-flight capacity remain
honest. The supplied-client path uses the caller's Tokio runtime handle
and never shuts it down.

## What bridges preserve and weaken

**Preserved by every bridge crate:**

- bounded ingress (mailbox or `max_in_flight`);
- typed visible failures (`Full` / `Closed` / `Timeout` named at every
  layer);
- synchronous Tina handlers (the bridge does not turn handlers async);
- no hidden unbounded queue between Tokio and Tina.

**Weakened (by the nature of the boundary):**

- deterministic replay under `tina-sim` — bridge-side IO is not
  observed by the simulator;
- Tower readiness backpressure — Tina ingress cannot back-press a
  Tower `poll_ready` without an unbounded wait, so admission shows
  up on the call future, not on `Pending`.

Each bridge crate's lib docs name these explicitly; the per-crate
list is the source of truth.

## When in doubt

- Read the bridge crate's lib-level docs. They name the contract,
  the cancellation rule, and the metrics.
- Look at the per-crate example (`tina-reqwest-bridge`'s `fetch_one`,
  `tina-tower-bridge`'s `axum_counter`).
- Look at the bridge specimens (`specimen_axum_counter`,
  `specimen_ws_room`, `specimen_sqlite_counter`,
  `specimen_postgres_counter`) for tested call-site shapes.
- The rule is "bridge may not lie." If a bridge looks like it would let a
  request disappear, smooth a typed error into a generic one, or grow an
  unbounded queue, treat that as an API bug.

## Bridge author kit

This section is for someone adding the next SDK bridge. It pins the
shared vocabulary every bridge implements and the test checklist a
review will look for.

If you want the user-shaped *copied path* — eight numbered steps that
map to `BridgeInstall`, `BridgeCloser`, `close_and_drain`, the metrics
handle, the pressure report, and the classifier — start from
[30-bridge-author-kit.md](30-bridge-author-kit.md). This section names
the deeper vocabulary the copied path uses.

### Lifecycle

```
                     install
                        │
                        ▼
                ┌──────────────┐
                │   Ready /    │
                │   Failed     │ ───── install failed: caller gets
                └──────┬───────┘       Err(InstallError); no handle.
                       │
              admit?   │
        ┌────yes──┐    │   ┌──no──────┐
        ▼         │    │   ▼          │
   in-flight    rejected           Full / Closed
   SDK call    (typed err)        (caller sees
        │                          Retryable / Unavailable)
        ▼
   worker terminal     ◀── close()
        │                       └─── closes admission,
        ▼                            in-flight SDK keeps running
   reply to caller
   (or late_result if              ┌── close_and_drain(timeout)
    caller already gave up)        └── waits for in-flight to drain
                                       up to deadline; reports kinds
                                       still in flight
```

Every bridge says the same boring things:

- `install` — build worker, register on a Tina runtime, return a
  typed install handle.
- `ready` / `failed` — `install_*` returns `Result<InstalledXxxBridge,
  InstallError>`. Failure is typed, not a panic.
- `admit` / `full` — admission is bounded by `max_in_flight`. When the
  cap is reached, the bridge replies `Full` immediately. It does not
  queue.
- `close admission` — `XxxCloser::close()` sets the closed flag.
  Already-admitted SDK work continues; new admissions are rejected
  with `Closed`.
- `drain` — `XxxCloser::close_and_drain(timeout)` waits for in-flight
  count to reach zero, then reports a typed `XxxDrainReport`.
- `shutdown` — drop the runtime/handle that owns the bridge. The
  bridge's Tokio runtime (if it owns one) is shut down in background;
  callers' Tokio runtimes are never shut down by the bridge.
- `late result` — when a caller deadline fires but the SDK call
  continues, the worker still observes terminal completion. The
  bridge increments a `late_results` counter and emits the result to
  a sink that no one is listening on. The slot is released only on
  worker terminal.
- `metrics` — every bridge exposes a `XxxMetricsHandle` with
  `snapshot()` for the typed counters.
- `pressure` — every bridge exposes `XxxMetricsHandle::pressure_report()`
  with the shared shape (capacity, current, high water, full,
  timeout, closed, late, plus per-bridge extensions). The handle
  stores the installed capacity itself: a caller cannot lie about it
  by passing a fresh config to the metrics handle.

### Close vs drain vs shutdown

| What you call             | What happens                                     | When to use                      |
| ------------------------- | ------------------------------------------------ | -------------------------------- |
| `closer.close()`          | Admission flips closed. In-flight keeps running. | Stop taking new work; cheap.     |
| `closer.close_and_drain`  | Closes, then waits up to `timeout` for drain.    | Graceful shutdown with deadline. |
| drop the install          | Tokio runtime (if owned) shuts down background.  | Test teardown, app exit.         |

The closer is cloneable and `Send`. The drain report names the kinds
of operations still in flight so you can decide whether to give them
more time or move on.

### External work cancellation honesty

The bridge cannot stop the outside system. When a Tina caller's
`IsolateCall` deadline fires or the bridge's per-operation deadline
fires:

- Tina stops waiting.
- The SDK future is **not** aborted. Aborting a Tokio task does not
  prove that bytes already accepted by the HTTP client were cancelled.
- The SDK eventually finishes. Worker-terminal metrics are tallied,
  `late_results` increments, and the operation leaves the bridge's
  in-flight set.

If the call mutated remote state (`PutObject`, `Publish`, `UPDATE`),
the mutation may have already happened. The bridge cannot prove
otherwise. The caller's idempotency story decides what to do.

### Worker-terminal vs caller-observed

The bridge has two truths:

- **Worker terminal** — the SDK round-trip finished, success or
  classified failure. Counts roll into worker-terminal metrics.
- **Caller observed** — the Tina reply slot received the outcome.

These coincide when the caller is still listening. They diverge when
the caller has given up (deadline, cancellation, bridge timeout).
Then `worker_terminal_count` reflects what the bridge measured but
`late_result_count` is the count the caller did not see.

Bridges must not claim the caller observed an outcome they cannot
prove was observed. When in doubt, attach a
`BridgeCallerWarning::ExternalWorkMayContinue` to the reply or surface
the late result through metrics.

### Late-result truth

When a deadline fires while SDK work is in flight:

- The reply slot the caller is holding gets `CallOutcome::Timeout`
  (or `Replied(Err(BridgeTimeout))` if it was the bridge's deadline).
- The SDK future is dropped onto the bridge's runtime. When it
  finishes, the bridge:
  - increments `late_results`,
  - decrements `in_flight_current`,
  - records the typed terminal classification in a sink (or just
    logs it).

If the bridge cannot observe late terminal completion (rare; mostly
fire-and-forget shapes), it must say so in docs and report
`late_result_count = 0`. Silently rolling late events into "success"
is wrong.

### Pressure report shape

Use the shared `BridgePressure` type from `tina_runtime::bridge` when
exposing pressure across bridge boundaries (dashboards,
`ServicePressureReport`). Each bridge crate ships a
`From<XxxPressureReport> for BridgePressure` impl:

```rust
use tina_runtime::bridge::BridgePressure;

let pg: BridgePressure = pg_metrics.pressure_report().into();
report.add_measured("bridge", pg.capacity_surface(CapacityMode::Fixed));
```

`BridgePressure`'s fields are private. The only ways to construct
one are `BridgePressure::measured(...)`, `BridgePressure::unavailable(
name, reason)`, and the per-bridge `From` impls. This is on purpose:
a forged literal would let a buggy adapter lie about installed
capacity or rename a dashboard surface by typo.

### Supplied-client ownership rule

Each bridge ships two install paths:

- `install_xxx(runtime, cfg)` — bridge owns its Tokio runtime *and*
  SDK client. Bridge applies credentials, region, endpoint, retry,
  HTTP/TLS.
- `XxxWorker::with_supplied_client(cfg, client, runtime_handle)` —
  caller supplies an SDK client *and* a Tokio runtime handle. Bridge
  uses them as given and never touches credentials, region, endpoint,
  or retry on this path. Bridge does **not** shut down the supplied
  runtime.

When wrapping a supplied client, the bridge reports
`sdk_max_attempts = 0` because the SDK retry policy is caller-owned.
Tina-side caps (`mailbox_capacity`, `max_in_flight`,
`per_request_timeout`, request/response size caps) remain bridge-owned
on both paths — they are not negotiable.

### Classifier rule

Every bridge ships a `XxxOutcomeExt::classify(...)` (or a
`bridge_class()` projection on the per-bridge richer enum) that returns
[`BridgeOutcomeClass`](../../tina-runtime/src/bridge.rs):

- `Succeeded` — worker reached terminal `Ok`.
- `Retryable(BridgeRetryable)` — caller may retry under their own
  idempotency rules.
- `Unavailable(BridgeUnavailable)` — bridge / pool / resource is
  closed. Retrying on the same handle reproduces. Caller needs a new
  handle.
- `Fatal(BridgeFatal)` — request will not succeed without changing
  inputs, permissions, schema, or code.

Two anti-fog rules the classifier must uphold:

1. `Closed` and `PoolClosed` go in `Unavailable`, **not** `Retryable`.
2. The generic SDK-error wrapper (`Sdk(_)`) carries no retryable
   metadata at this layer; classify it as `Fatal(SdkUnknown)`, not
   `Retryable(SdkRetryable)`. Reserve `SdkRetryable` for cases where
   typed SDK metadata explicitly says throttled / retryable.

### Hermetic test checklist

Every bridge should have:

- Happy-path test that the worker accepts the typed request, the SDK
  is called, the typed response comes back.
- `Full` / `Closed` test that the closer flips admission and new
  callers see the right typed outcome.
- Caller-timeout test that produces a `late_result` and asserts the
  count exactly once (no double-tally).
- Drain test that closes mid-flight and confirms the drain report
  names the in-flight kinds.
- Classifier coverage of every typed error variant against the shared
  `BridgeOutcomeClass`.
- Pressure-report test that asserts the installed capacity (cannot be
  faked by passing a fresh config to the metrics handle).
- Late-result count visible when the bridge can observe late
  completions; explicit `0` documented when it cannot.

Use `BridgePressure::unavailable(name, reason)` when a bridge surface
genuinely cannot be measured. Do not silently omit; the discovery
line should always show what was measured and what was not.
