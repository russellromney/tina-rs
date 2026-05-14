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
[Native WebSocket Server](20-native-websocket-server.md). Native HTTP/2 now has a
server-first h2c path in `tina-http::Http2Listener`: cleartext
prior-knowledge transport, bounded stream table, explicit
connection/stream flow-control windows, ordinary `HttpRequest` /
`HttpResponse` service dispatch, streamed response DATA from Tina chunk
sources, and gRPC request-body pull sources. Native gRPC now layers unary plus
first server-streaming/client-streaming `prost` messages on that h2c path
through `tina_http::GrpcRouter`: typed `GrpcStatus` trailers, message caps, no
compression, and service timeout mapped to `DeadlineExceeded`. It is not tonic
parity, not true bidirectional streaming, not HTTPS/2 ALPN, and not a broad
client. Native HTTPS/1.1 lives in `tina-http`'s
`HttpsListener` and `HttpClient` — explicit DER cert config, typed
startup, matchable TLS errors. For repeated outbound
requests against the same origin, `tina_http::build_keepalive_pool`
hands you a `KeepalivePoolHandles { pool, connections }`: one TCP
(or TLS) connection per pool slot serves many requests, with
`acquire` / `release` / `retire` / `close` and a pressure report.
Each connection isolate is bound to one origin at construction —
scheme + `SocketAddr` + (HTTPS) SNI + the configured DER trust
roots themselves — so cross-origin reuse cannot happen at the
connection-isolate level. The recommended consumer pattern is
always release `Reuse`; the connection self-heals on
`must_retire = true` (drops the bad transport, reconnects on the
next request). On shutdown, call `shutdown_keepalive_pool(...)`
or close the pool and call `KeepaliveConnectionMsg::Stop` on each
address in `handles.connections`, checking for
`KeepaliveOutcome::Stopped`; closing the pool alone only closes
lease admission. With `CloseMode::Drain`, the helper waits for
leased connections to return before stopping connection isolates;
if that deadline fires, the report names the remaining leased count
and leaves connections running. Reach for a bridge when you need
outbound HTTP/2 client behavior, HTTPS/2 ALPN, system trust roots,
redirects/cookies, an existing Axum app, or a third-party SDK that only
ships a Tokio client.

## What ships today

| Crate | Direction | Used when |
| --- | --- | --- |
| `tina-tokio-bridge` | Tokio caller → Tina isolate | A Tokio handler needs a bounded request/reply path into a Tina service. |
| `tina-tower-bridge` | `tower::Service` over a Tina bridge | An Axum/Hyper/Tower stack wants to call a Tina service through normal Tower middleware. |
| `tina-reqwest-bridge` | Tina caller → outbound HTTP via `reqwest` | A Tina service needs outbound HTTP/2, redirects, cookies, system trust roots, or other mature web-client behaviour. Native HTTPS/1.1 from `tina-http::HttpClient` covers single-request DER-rooted calls; native HTTP/2 is server-side h2c first form only; reqwest covers everything else. |
| `tina-sqlite-bridge` | Tina caller → SQLite via `rusqlite` | A Tina service needs an in-process SQL database. SQLite is sync C; the bridge owns one connection on a blocking std thread. Autocommit only; no pool, no transactions in first form. |
| `tina-sqlx-bridge` | Tina caller → Postgres via `sqlx::PgPool` | A Tina service needs to reach a real Postgres without blocking shard threads. Two-runtime cost: the bridge spawns SQLx work on Tokio. Postgres-first. Ships `Execute`, `FetchOne`, bounded `FetchMany`, atomic-script `Transaction`, and opt-in DB-side cancel. Generic `sqlx::Database`, ORM, migrations, and user-struct row mapping stay non-goals. |
| `tina-aws-bridge` | Tina caller → AWS SDK S3/SQS | A Tina service needs AWS SDK behavior without letting AWS/Hyper/Tokio pressure become invisible. Ships S3 (`PutObject`, bounded `GetObject`, `HeadObject`, `DeleteObject`) and SQS (`SendMessage`, `ReceiveMessage`, `DeleteMessage`). The SDK still owns SigV4, credentials, HTTP, TLS, endpoints, and service protocols. |

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
use tina_tokio_bridge::{BridgeHost, BridgeRequest};

let mut host = BridgeHost::new(shard, factory, runtime_config);
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
        Err(BridgeError::Full | BridgeError::Closed) => Err(StatusCode::SERVICE_UNAVAILABLE),
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
normal `call(...).reply(...)` path:

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
            .reply(AppMsg::HttpReturned),

            AppMsg::HttpReturned(outcome) => match outcome {
                CallOutcome::Replied(Ok(response)) => { /* success */ }
                CallOutcome::Replied(Err(e)) => { /* worker-level failure */ }
                CallOutcome::Full | Closed | Timeout => { /* bridge-level failure */ }
            },
        }
    }
}
```

Setup uses the `install` helper:

```rust
let bridge = ReqwestWorker::<SingleShard>::install(&runtime, ReqwestConfig::default())?;
let app = App { http: bridge.address };
```

For direct bridge crates, this is the convention:

```text
install = validate config, register worker, return address + closer + metrics
close   = stop admitting new work
drain   = wait bounded time for accepted work
metrics = bridge view of worker-terminal outcomes
trace   = runtime truth for dropped callers and late replies
```

Do not invent a new bridge setup dialect unless the old words lie.
SQLite first form follows this shape with one blocking connection.
The Postgres SQLx bridge follows it too, with two install paths
(`install` builds a pool from config; `install_with_pool` wraps a
caller-supplied `sqlx::PgPool` whose SQLx settings stay caller-owned).

The production-shaped copy path for SQLite plus native outbound HTTP is
`examples/systems/mini_saas_api`:

```sh
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- smoke
```

It uses `SqliteWorker` as the honest one-lane pool shape and
`build_keepalive_pool` for outbound notifications. The route code keeps
bridge-layer `Full` / `Closed` / `Timeout` distinct from worker or upstream
failures.

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
let bridge = PgWorker::<SingleShard>::install(&runtime, cfg)?;

// In a handler:
execute_call(self.db, "INSERT INTO t (k, v) VALUES ($1, $2)",
    vec![1.into(), "hello".into()], Duration::from_secs(2))
    .reply(AppMsg::Inserted);
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

**Cancellation.** Default off: Postgres keeps running the query
past `PgError::Timeout`. The connection stays held until SQLx
returns. Treat `PgError::Timeout` as "Tina stopped waiting," not
"the database stopped."

Opt-in via `PgConfig::with_cancel_on_timeout(pool_size)`. The
bridge builds a sidecar pool from the same URL and fires
`pg_cancel_backend(pid)` on timeout. Cost: one extra round trip
per request to capture the backend PID. `db_cancels_sent` counts
attempts. Best-effort — Postgres may not honor it, and a small
race exists between cancel firing and the target connection
returning to the pool. Only the `install` path honors this;
`install_with_pool` silently ignores it.

**Two install paths.**

- `install(&runtime, cfg)` — bridge builds the pool and a small
  Tokio runtime from `cfg`.
- `install_with_pool(&runtime, cfg, pool, handle)` — caller
  supplies both. SQLx settings on the supplied pool are caller-
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

Non-goals: generic `sqlx::Database`, ORM, migrations, struct
mapping, a transaction *handle* (vs. atomic script). See the
phase plan for the why.

### `tina-aws-bridge` — Tina → AWS SDK S3/SQS

Adoption bridge. The AWS Rust SDK owns AWS protocol behavior; Tina
owns bounded admission, body/message caps, per-operation timeout truth,
typed outcomes, and metrics.

```rust
use tina_aws_bridge::{
    S3Config, S3Credentials, S3Request, S3PutObject, install_s3, send_s3,
};

let cfg = S3Config::new()
    .with_region("us-east-1")
    .with_credentials(S3Credentials::new("access-key-id", "secret-access-key"))
    .with_max_in_flight(8)
    .with_default_timeout(Duration::from_secs(2));
let bridge = install_s3(&runtime, cfg)?;

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
.reply(AppMsg::S3PutDone);
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
.reply(AppMsg::S3PutDone)
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
    SqsConfig, SqsCredentials, SqsRequest, SqsSendMessage, install_sqs, send_sqs,
};

let cfg = SqsConfig::new()
    .with_region("us-east-1")
    .with_credentials(SqsCredentials::new("access-key-id", "secret-access-key"))
    .with_message_body_limit(64 * 1024)
    .with_max_receive_messages(10);
let bridge = install_sqs(&runtime, cfg)?;

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
.reply(AppMsg::SqsSendDone);
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
- The rule is "bridge may not lie." If a bridge looks like it would
  let a request disappear, smooth a typed error into a generic one,
  or grow an unbounded queue, that's a bug — file it as a paper cut
  in `examples/FINDINGS.md`.
