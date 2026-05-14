# Native WebSocket Room Specimen

Small Tina-native WebSocket room shape over `tina-http`'s HTTP/1.1
upgrade path.

The HTTP listener owns TCP accept and request parsing. A `GET /room`
upgrade returns `HttpResponse::websocket(...)`, after which the
connection isolate becomes the WebSocket session owner. The room app
receives both the 087 echo-style events (`Open`, `Text`, `Binary`,
`Ping`, `Pong`, `Close`, `Pressure`, `Closed`) and the 094
handle-bearing events (`SessionOpen`, `SessionText`, `SessionBinary`).

The specimen serves:

- `GET /` - a copyable browser `WebSocket` smoke page;
- `GET /room` - the WebSocket upgrade route;
- `POST /rooms/default` - create/reopen the bounded named room;
- `DELETE /rooms/default` - delete the bounded named room and close members;
- `GET /room-report` - JSON counters for joins, leaves, send outcomes,
  admission rejections, shutdown, stale-handle rejection, high-water counts,
  and fill-close-refill;
- `GET /health` and `GET /ready` - small liveness/readiness endpoints.

The browser smoke requests `tina.room.v1`; the gateway selects that
subprotocol when offered and omits it otherwise. Unsupported extension offers
such as `permessage-deflate` remain ignored rather than negotiated.

Run the Rust e2e checks with:

```sh
cargo test --manifest-path examples/specimen_websocket_room/Cargo.toml
```

Run the real-browser smoke, after installing the local Playwright package and
browser, with:

```sh
cd examples/specimen_websocket_room
npm install
npx playwright install chromium
npm run browser:smoke
```

The Playwright smoke starts both a plain `RoomServer` and a TLS
`TlsRoomServer`, then proves Chromium can use the browser page over `ws://` and
`wss://`. The TLS path uses local test trust only; production deployments should
use ordinary trusted certificates.

The specimen has one bounded named room, `default`. Delete closes current
members, rejects new upgrades, and leaves stored stale handles unable to hit a
later recreated session. Create reopens that named room. Idle expiry uses a
Tina-owned timer and deletes the room when it has no live members.

The room stores `WebSocketSessionHandle` values in a fixed-capacity member
table. Broadcast, shutdown, and per-session reports use only public API:

```rust
handle.text_effect::<Room>("room:hello", timeout)
handle.close_effect::<Room>(Some(WebSocketCloseCode(1001)), "server shutdown", timeout)
handle.report_effect::<Room>(timeout)
```

That message routes back through the one connection isolate that owns the
upgraded stream. The handle does not own a writer, spawn a task, or keep a
closed session alive. `report_effect` returns a `#[non_exhaustive]` bounded
snapshot for one session: id/generation, selected subprotocol, close state,
queued frames/bytes, active write bytes, last pressure, and last close
code/reason byte count.

The specimen is copyable application shape, not the final crate boundary. If
the room registry, admission policy, fanout policy, slow-peer policy, and report
shape become reused across apps, they should move into a small
`tina-websocket-room`-style helper crate while `tina-http` keeps only the core
upgrade/session/handle API.

This specimen keeps fanout deliberately tiny. Its tests prove the public
multi-client WebSocket path through raw frames, real `tungstenite` clients over
`ws://`, a real `tungstenite` client over `wss://` backed by rustls, browser
page serving over `ws://` and `wss://`, Origin/auth/subprotocol admission
rejection, bidirectional broadcast, slow-peer byte pressure, room report,
health/readiness, shutdown close/reject behavior, capacity+1 rejection,
repeated reconnect/refill without live-member leaks, CI-short load/churn,
many-client shutdown, room create/delete, idle room expiry, ordinary HTTP
routes beside an active WebSocket, and fill-close-refill capacity shape.
Room send outcomes distinguish
`OutboundQueueFull`, `OutboundBytesFull`, `Closed`, `Closing`, `Stale`, and
`Protocol`; the specimen policy removes a member on full/closed pressure.

Still out of scope here: HTTP/2 WebSocket, permessage-deflate compression,
automatic reconnect, Autobahn classification, live trace to simulator replay,
and a broad native WebSocket client crate. Browser extension offers are ignored
unless Tina explicitly negotiates an extension in a later slice.
