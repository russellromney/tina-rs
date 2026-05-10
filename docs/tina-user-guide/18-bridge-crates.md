# Bridge Crates

Native Tina is one path. Bridges are the adoption path — they let
Tokio-shaped ecosystem packages live next to a Tina core without
either side lying about pressure.

The rule:

> Tokio may speak ecosystem. Tina owns state. Bridge shows pressure.
> Bridge may adapt. Bridge may not lie.

If you can use a native Tina crate, do. Native HTTPS/1.1 lives in
`tina-http`'s `HttpsListener` and `HttpClient` — explicit DER cert
config, typed startup, matchable TLS errors. For repeated outbound
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
next request). Send `KeepaliveConnectionMsg::Stop` to each
address in `handles.connections` on shutdown so transports close
promptly. Reach for a bridge when you need HTTP/2, ALPN, system
trust roots, redirects/cookies, an existing Axum app, or a
third-party SDK that only ships a Tokio client.

## What ships today

| Crate | Direction | Used when |
| --- | --- | --- |
| `tina-tokio-bridge` | Tokio caller → Tina isolate | A Tokio handler needs a bounded request/reply path into a Tina service. |
| `tina-tower-bridge` | `tower::Service` over a Tina bridge | An Axum/Hyper/Tower stack wants to call a Tina service through normal Tower middleware. |
| `tina-reqwest-bridge` | Tina caller → outbound HTTP via `reqwest` | A Tina service needs outbound HTTP/2, redirects, cookies, system trust roots, or other mature web-client behaviour. Native HTTPS/1.1 from `tina-http::HttpClient` covers single-request DER-rooted calls; reqwest covers everything else. |
| `tina-sqlite-bridge` | Tina caller → SQLite via `rusqlite` | A Tina service needs an in-process SQL database. SQLite is sync C; the bridge owns one connection on a blocking std thread. Autocommit only; no pool, no transactions in first form. |
| `tina-sqlx-bridge` | Tina caller → Postgres via `sqlx::PgPool` | A Tina service needs to reach a real Postgres without blocking shard threads. Two-runtime cost: the bridge spawns SQLx work on Tokio. Postgres-first. Ships `Execute`, `FetchOne`, bounded `FetchMany`, atomic-script `Transaction`, and opt-in DB-side cancel. Generic `sqlx::Database`, ORM, migrations, and user-struct row mapping stay non-goals. |

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
