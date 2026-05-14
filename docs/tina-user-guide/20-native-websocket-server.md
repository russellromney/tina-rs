# Native WebSocket Server

`tina-http` supports native bounded server-side WebSockets for HTTP/1.1.

That means:

- `HttpListener` and `HttpsListener` can validate a `GET` upgrade;
- after `101 Switching Protocols`, one connection isolate owns the stream;
- the server frame codec, ping/pong, close handshake, and pressure outcomes
  stay inside Tina;
- session handles route sends back through the owning connection isolate.
- room code can drive graceful app shutdown by sending
  `WebSocketSessionMsg::Shutdown` to its WebSocket app isolate, which closes
  stored handles through the same bounded owner path.
- upgrade code can inspect offered subprotocols/extensions and select one
  subprotocol for the `101` response.

It does not mean HTTP/2 WebSocket, permessage-deflate compression, automatic
reconnect, or a broad native WebSocket client crate.
Browser extension offers such as `permessage-deflate` are ignored unless Tina
explicitly negotiates an extension in the future.

## Copy Path

The production-shaped specimen is:

```sh
cargo test --manifest-path examples/specimen_websocket_room/Cargo.toml
```

Copy from `examples/specimen_websocket_room/src/lib.rs` when you need a
small multi-client room with bounded member storage, Origin/auth/subprotocol
admission, send outcomes, slow-peer policy, stale-handle proof, app-level
shutdown, health/readiness endpoints, report endpoint, browser `ws://` and
`wss://` smoke page, and fill-close-refill capacity proof.

## Upgrade

The HTTP service still returns an ordinary `HttpResponse`. On the WebSocket
route, validate the upgrade and hand the accepted session to the room app:

```rust
use http::Method;
use tina_http::{
    HttpRequest, HttpResponse, WebSocketLimits, WebSocketSessionMsg,
    WebSocketSessionOutcome, websocket_upgrade,
};
use tina::Address;

fn response_for(
    request: &HttpRequest,
    room: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
    limits: WebSocketLimits,
) -> HttpResponse {
    if request.method == Method::GET && request.path == "/room" {
        return match websocket_upgrade(request, limits) {
            Ok(upgrade) => HttpResponse::websocket(upgrade.accept(room, limits)),
            Err(_) => HttpResponse::bad_request(),
        };
    }

    HttpResponse::not_found()
}
```

If a route requires a subprotocol, inspect the offered protocols before
accepting:

```rust
let upgrade = websocket_upgrade(request, limits)?;
if upgrade
    .offered_subprotocols()
    .iter()
    .any(|protocol| protocol == "tina.room.v1")
{
    let accept = upgrade.accept_subprotocol(room, limits, "tina.room.v1")?;
    return Ok(HttpResponse::websocket(accept));
}
```

Unsupported extension offers are visible through `extension_offers()` but are
not negotiated by default.

The room specimen keeps admission local and boring: an optional Origin
allowlist, optional bearer-token-or-cookie check, and optional required
subprotocol. Rejections are ordinary HTTP responses before the stream upgrades:
`403` for Origin, `401` for auth, `400` for unsupported required
subprotocol, `503` for shutdown/full service state.

## Echo Path

The 087 echo shape still works. A one-session app can ignore handles and reply
with `WebSocketSessionOutcome`:

```rust
use tina_http::{WebSocketSessionMsg, WebSocketSessionOutcome};

fn echo(msg: WebSocketSessionMsg) -> WebSocketSessionOutcome {
    match msg {
        WebSocketSessionMsg::Text(text) => WebSocketSessionOutcome::Text(text),
        WebSocketSessionMsg::Binary(bytes) => WebSocketSessionOutcome::Binary(bytes),
        WebSocketSessionMsg::Ping(bytes) => WebSocketSessionOutcome::Pong(bytes),
        WebSocketSessionMsg::Close(code, reason) => {
            WebSocketSessionOutcome::Close(code, reason)
        }
        _ => WebSocketSessionOutcome::None,
    }
}
```

Use this for simple request-ish sessions. Use handles for rooms and fanout.

## Room Sends

Rooms store `WebSocketSessionHandle` values from `SessionOpen`. Each handle
carries the session id plus generation/incarnation and the target address
generation. A stale handle must fail visibly and must not send to a later
session.

```rust
use std::time::Duration;
use tina::Isolate;
use tina_http::{WebSocketSessionHandle, WebSocketSessionMsg};
use tina_runtime::RuntimeCall;

fn send_room_text<I>(
    handle: WebSocketSessionHandle,
    text: String,
    timeout: Duration,
) -> tina::Effect<I>
where
    I: Isolate<Message = WebSocketSessionMsg, Call = RuntimeCall<WebSocketSessionMsg>>,
{
    handle.text_effect::<I>(text, timeout)
}
```

The call target is the same connection isolate that owns the upgraded stream.
There is no second writer and no hidden queue. Runtime delivery pressure is
preserved as `Full`, `Closed`, or `Timeout`, then mapped into
`WebSocketSendError`.

## Budgets

| Limit | Meaning |
| --- | --- |
| `max_frame_bytes` | largest accepted frame payload |
| `max_message_bytes` | largest complete data message, including bounded fragmented reassembly |
| `read_buffer_high_water` | max resident read buffer before closing the peer |
| `inbound_app_mailbox_capacity` | documented app mailbox budget; actual cap is isolate registration |
| `outbound_frame_queue_capacity` | max parked outbound frames while a write is in flight |
| `max_queued_outbound_bytes` | queued plus active outbound frame bytes budget |
| `broadcast_fanout_max_targets` | documented room fanout cap for copied room shapes |
| `ping_pong_timeout` | unanswered ping deadline |
| `close_handshake_timeout` | close handshake deadline |

Session handles also expose a bounded owner-routed report call. Use it when a
room needs connection-owned state such as close state, selected subprotocol,
queued outbound frames/bytes, active write bytes, or the last pressure reason:

```rust
handle.report_effect::<Room>(Duration::from_secs(1))
```

Keep report collection bounded. The specimen exposes aggregate room/server
snapshots rather than appending one event per message.

## Pressure

| Where | Visible outcome |
| --- | --- |
| Runtime call to session owner is full | `WebSocketSendError::OutboundQueueFull` |
| Runtime call to session owner is closed/rejected | `WebSocketSendError::Closed` |
| Runtime call times out | `WebSocketSendError::Timeout` |
| Handle names a previous session generation | `WebSocketSendError::Stale`, or `Closed` if the old owner already stopped |
| Session is closing | `WebSocketSendError::Closing` |
| Outbound frame queue is full | `WebSocketSendError::OutboundQueueFull` |
| Outbound byte budget is full | `WebSocketSendError::OutboundBytesFull` |
| Peer sends malformed continuation state | `WebSocketError::ProtocolError` |
| Peer exceeds read/frame/message budget | `WebSocketSessionMsg::Pressure(...)` then close |

The room specimen uses a plain policy: record `Full` / `Closed` / `Timeout`,
remove that member from the bounded table, and keep serving the rest of the
room.

## User-Facing Proofs

The room specimen tests the path users are likely to copy:

- two real `tungstenite` clients over `ws://` join the same room and exchange
  broadcasts;
- a real `tungstenite` client over `wss://` upgrades through `HttpsListener`
  and rustls;
- `GET /` serves a tiny browser `WebSocket` page that points at `/room`, and
  the Playwright smoke runs that page in Chromium over both `ws://` and
  `wss://` with local test trust;
- `GET /room-report` exposes counters for joins, leaves, send outcomes,
  admission rejections, shutdown, stale-handle rejection, high-water counts,
  and live member count;
- `GET /health` and `GET /ready` expose simple liveness/readiness;
- shutdown closes existing clients and rejects new room upgrades.

## What This Is Not

This is a usable bounded server surface, not the final WebSocket product
story. Before Tina should call WebSockets "fully ready" from a user
perspective, it still needs at least a standards/compliance pass, automated
Autobahn classification, simulator/live-replay facts for the byte path, and a
product decision on Tina-native client support versus documented external
clients.
