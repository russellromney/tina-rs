# Bridge Crates

Native Tina is one path. Bridges are the adoption path — they let
Tokio-shaped ecosystem packages live next to a Tina core without
either side lying about pressure.

The rule:

> Tokio may speak ecosystem. Tina owns state. Bridge shows pressure.
> Bridge may adapt. Bridge may not lie.

If you can use a native Tina crate, do. If you need HTTPS, HTTP/2, an
existing Axum app, or a third-party SDK that only ships a Tokio
client, reach for a bridge crate.

## What ships today

| Crate | Direction | Used when |
| --- | --- | --- |
| `tina-tokio-bridge` | Tokio caller → Tina isolate | A Tokio handler needs a bounded request/reply path into a Tina service. |
| `tina-tower-bridge` | `tower::Service` over a Tina bridge | An Axum/Hyper/Tower stack wants to call a Tina service through normal Tower middleware. |
| `tina-reqwest-bridge` | Tina caller → outbound HTTP via `reqwest` | A Tina service needs outbound HTTPS/HTTP-2 with mature redirect, retry, and connection-reuse. |

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

`flatten_outcome(outcome)` is available when the call site does not
need to distinguish bridge-layer from worker-layer failures.

`outcome.classify()` (via the `ReqwestOutcomeExt` trait) is available
for caller-owned retry loops: it returns
`ReqwestOutcomeClass::{Succeeded, Transient(reason), Fatal(reason)}`
where the typed reason still names which layer failed
(`BridgeTimeout` vs `WorkerTimeout`, `BridgeFull` vs `WorkerFull`,
etc.). The classifier does not retry — caller still owns idempotency,
budget, and backoff.

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
- Look at the Eiffel specimens (`eiffel_axum_counter`,
  `eiffel_ws_room`) for tested call-site shapes.
- The rule is "bridge may not lie." If a bridge looks like it would
  let a request disappear, smooth a typed error into a generic one,
  or grow an unbounded queue, that's a bug — file it as a paper cut
  in `examples/FINDINGS.md`.
