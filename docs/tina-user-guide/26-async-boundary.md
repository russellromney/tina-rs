# The Async Boundary

Tina is not a general async runtime. It is a bounded, shared-nothing, simulatable
message/effect model. So a recurring question from people coming from Tokio is:
*"where does my async dependency go?"*

This page answers that with three buckets. Every common ecosystem need lands in
exactly one.

> **Native** — use a Tina-owned rail. No Tokio, no hidden executor.
> **Bridge** — the external async crate is valuable; bound it at the bridge edge.
> **Unsupported** — Tina cannot preserve bounded/DST truth here yet. Say so.

The deciding question is always the same: *can this preserve Tina's bounded,
typed, replayable truth?* Native preserves it directly. A bridge preserves
admission and worker-terminal truth at its edge while being honest that the
outside system is not Tina-owned. Unsupported means neither is true yet.

There is **no generic `Future`/`Stream` bridge** and **no hidden Tokio under a
native Tina service**. A bridge is an explicit, bounded worker, not a way to
sprinkle `async fn` through your isolates.

## Native paths

Use a Tina-owned rail. These run on the live substrate and replay in `tina-sim`.

- **Timers / delays** (`tokio::time::sleep`, `interval`) → native `sleep(..)`
  effect and the timer helpers. Replayable virtual time.
- **TCP / TLS servers and clients** (`tokio::net`, `tokio-rustls`) → native
  runtime TCP/TLS rails (`tcp_*`, `tls_*`).
- **HTTP/1, HTTP/2, gRPC, WebSocket** (`hyper`, `axum`, `tonic`,
  `tokio-tungstenite`) → the `tina-http` battery for the claimed modes:
  HTTP/1.1, HTTP/2, gRPC unary/streaming, WebSocket server.
- **Local files** (`tokio::fs`) → native local file rails and the bounded storage
  lane.
- **DNS / UDP / process / signals** → native runtime rails (lane-backed or
  poll-backed; see the capability report).
- **Local IPC and codecs** (`tokio_util::codec`) → native Unix-domain socket
  rails plus `tina-codec` / a custom `SyncCodec`. Tina owns the socket; the
  codec is sync state.
- **Channels and backpressure** (`tokio::sync::mpsc`) → native bounded
  mailboxes, cross-shard sends, and the admission policies. Backpressure is a
  typed `Full`, not an unbounded queue.
- **Tracing** (`tracing`) → native `tina-tracing`.

If a native rail exists, async interop is **not** the first answer.

## Bridge paths

The external async crate carries real value (a mature SDK, a wire protocol Tina
has not reimplemented). Wrap it in a bounded bridge: bounded admission, observed
worker-terminal truth, Tina-owned deadlines, honest "external work may continue"
warnings. The SDK's own threads/queues are not Tina-owned unless the bridge
proves and reports them.

- **Full `reqwest` HTTP client** (redirects, cookies, connection reuse you do not
  want to reimplement) → `tina-reqwest-bridge`. (Simple outbound HTTP can be
  native via `tina-http`.)
- **Postgres** (`sqlx`, `tokio-postgres`) → `tina-sqlx-bridge`. No native pg
  client ships; the bridge bounds the pool and classifies outcomes.
- **SQLite** (`rusqlite`, `sqlx`) → `tina-sqlite-bridge`. A serial blocking
  bridge; its capacity is small (`1`), not absent.
- **AWS SDK** (`aws-sdk-s3`, …) → `tina-aws-bridge`. Bounds admission into the
  smithy runtime; names the weakened boundary honestly.
- **`tower` middleware / `axum` apps** → `tina-tower-bridge` / `tina-tokio-bridge`
  to adopt Tina inside an existing Tokio app, or to reuse a `tower::Service`.
- **A blocking library with no async at all** (a C FFI client, a CPU library) →
  a bounded-worker bridge built on the `tina_runtime::bridge` vocabulary, like
  `examples/extensions/tina-extension-fake-bridge`.

A bridge is the right answer when reimplementing the protocol natively would be
a large project and the external crate already does it well.

## Unsupported paths

Tina cannot yet preserve bounded/DST truth here. The honest move is to say so —
not to paper over it with a hidden executor.

- **Generic `async fn` / futures combinators / `tokio::select!`** → unsupported
  as a *style*. Tina's model is synchronous handlers + effects + messages. Use a
  `CallGroup` for first-success races and continuation messages for sequencing;
  do not import a futures executor to get `select!`.
- **Redis / Kafka / NATS and other unbridged wire clients** (`redis`, `rdkafka`)
  → no native rail and no shipped bridge today. Either write a bounded bridge
  (and own that contract) or wait for one. Reaching for the raw async client
  inside an isolate is unsupported.
- **Arbitrary `tokio::spawn` background tasks inside an isolate** → unsupported.
  Background work is a child isolate, a runtime rail, or a bridge worker — never
  an untracked spawned task, which would be the first unbounded, unreplayable
  hole.
- **`Stream`-of-streams / generic reactive pipelines** → unsupported as a generic
  bridge. Model the pipeline as bounded isolates with explicit mailboxes.

"Unsupported" is a real, useful answer. It keeps the rest of the service bounded
and replayable instead of letting one async dependency quietly remove those
guarantees.

## Decision shortcut

1. Is there a Tina-owned rail or battery? → **native**.
2. Is there value in a mature external async crate Tina has not reimplemented? →
   **bridge** (bounded, honest about external work).
3. Neither? → **unsupported** — write a bridge and own its contract, or wait.

When in doubt, prefer native, then bridge, then an explicit unsupported note.
Never a hidden executor.
