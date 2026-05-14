# 094 WebSocket Usable Server

## Status

- Ready to implement after 087 lands.
- One PR.
- Owns `tina-http` WebSocket API/docs/tests and
  `examples/specimen_websocket_room`.
- Do not run beside broad `tina-http` protocol rewrites, HTTP/2 WebSocket,
  gRPC, or permessage-deflate work. Can run beside replay/docs-only work if
  files do not overlap.

## Grug Truth

First form is not a claim.

Users need a copied shape.

Rooms need session handles.

Slow peers must be an app decision, not hidden memory.

Close truth matters later, when debugging.

Docs that say "bounded" need tests that hurt.

Claims about TCP/TLS need both rails in CI.

Examples that do not compile are decoration.

## Goal

Move native WebSocket from "first form exists" to:

```text
Tina supports native bounded server-side WebSockets for HTTP/1.1.
```

This is still not "full WebSocket ecosystem replacement".

The claim this phase may make:

- server-side HTTP/1.1 WebSocket over Tina-owned TCP/TLS/HTTP rails;
- clear public docs with copyable upgrade/session examples;
- bounded memory everywhere: frame, message, read buffer, outbound queue count,
  outbound bytes, active write bytes, room fanout;
- masking, ping/pong, close handshake, protocol errors, and pressure tested;
- real multi-client room/broadcast specimen with visible slow-peer policy;
- stable enough public names for app code:
  `WebSocketLimits`, `websocket_upgrade`, `WebSocketAccept`,
  `WebSocketSessionMsg`, `WebSocketSessionOutcome`, send outcomes, and session
  handles.

## Non-Goals

- no HTTP/2 WebSocket;
- no permessage-deflate;
- no browser auth/session framework;
- no reconnect framework;
- no broad WebSocket client crate;
- no hidden Tokio;
- no unbounded broadcast queue;
- no automatic app retry;
- no full Autobahn compliance suite in this phase;
- no simulator claim for live WebSocket bytes unless scripted facts exist.

Fragmentation may stay rejected if docs and tests say so clearly. If bounded
fragmentation is added, it must be complete enough to handle control frames
between fragments and enforce `max_message_bytes`.

## Rock 0: Read First And Freeze The Claim

Read:

- `.intent/SYSTEM.md`;
- `.intent/phases/087-websocket-first-form/plan.md`;
- `.intent/phases/087-websocket-first-form/review.md`;
- `tina-http/src/websocket.rs`;
- `tina-http/src/connection.rs`;
- `tina-http/tests/websocket_live.rs`;
- `examples/specimen_websocket_room`;
- native HTTP docs and `docs/tina-user-guide/18-bridge-crates.md`.

At start of implementation, edit this status with:

- whether fragmentation remains rejected or becomes bounded reassembly;
- exact session handle API names;
- exact send outcome API names;
- chosen slow-peer policy in the room specimen.

Do not change the public claim until the tests and docs match it.

Implementation order:

1. session handle/send outcome API;
2. live WebSocket tests for handle send, pressure, and lifecycle;
3. room specimen using that public API;
4. TLS rail proof;
5. docs/examples;
6. final naming pass.

Cut line:

- If bounded fragmentation threatens the PR, keep fragmentation rejected.
- If browser/manual smoke threatens CI, keep it manual/docs-only.
- If a helper starts looking like a framework, stop and keep the explicit
  state-machine shape.

## Rock 1: Public Session Handle

Add a bounded session-send surface so apps can store a peer handle and send to
that peer later.

Candidate vocabulary:

- `WebSocketSessionHandle`;
- `WebSocketSend`;
- `WebSocketSendOutcome`;
- `WebSocketSendError`;
- `WebSocketSessionReport`.

The copied app path should feel like:

```rust
match msg {
    WebSocketSessionMsg::Open { session } => room.join(session),
    WebSocketSessionMsg::Text { session_id, text } => room.broadcast(session_id, text),
    WebSocketSessionMsg::Closed { session_id, reason } => room.leave(session_id),
    _ => {}
}
```

The copied path must compile using public API only. Do not require callers to
name `HttpConnection`, private frame types, or test-only helpers.

Hard rules:

- a handle names one session incarnation, not a logical user;
- sends to stale/closed sessions fail visibly;
- handle sends are bounded by outbound frame count and bytes;
- active write bytes count against the same outbound byte budget;
- app sees `QueueFull`, `BytesFull`, `Closing`, `Closed`, or `Timeout`
  distinctly;
- no `mpsc::unbounded`, no hidden thread, no hidden Tokio task;
- one upgraded stream still has one owner;
- handles must be cheap to clone/store but must not keep a closed session alive;
- send outcomes must include enough identity for a room to remove or mark the
  failed member.

If the existing "app replies with outbound commands" shape remains, document it
as the simple echo path. Room/fanout must use explicit handles or another
equally clear bounded send API.

## Rock 2: Stable Upgrade And Session Docs

Add native WebSocket docs where users will actually look.

Update at least:

- `tina-http` crate docs;
- a native HTTP/WebSocket user-guide page or README section;
- `examples/specimen_websocket_room/README.md`;
- bridge docs only to clarify when a bridge is still useful.

Docs must include:

- copyable upgrade example;
- copyable echo/session example;
- copyable room/broadcast sketch using session handles;
- `no_run` or real doctest coverage for the copied Rust examples where
  practical;
- limits table explaining every budget;
- pressure table mapping send/read/parse failures to typed outcomes;
- close lifecycle;
- ping/pong lifecycle;
- plain note that browser `WebSocket` clients interoperate with the HTTP/1.1
  server path, with a tiny JS snippet if useful;
- unsupported features list.

Docs must not overclaim:

- if fragmentation is rejected, say so;
- if simulator replay does not cover live WebSocket bytes, say so;
- if TLS is inherited from `HttpsListener`, say so;
- if there is no native WebSocket client, say so.

## Rock 3: Bound Everything And Name Every Overflow

Audit and harden every byte/count budget.

Required caps:

- max inbound frame bytes;
- max inbound message bytes;
- read buffer high-water;
- outbound queue frame count;
- outbound queued bytes, including active write;
- app/session mailbox capacity;
- room broadcast fanout target count;
- room per-peer send timeout or call timeout;
- ping/pong timeout;
- close handshake timeout.

Every overflow must surface a typed fact:

- `FrameTooLarge`;
- `MessageTooLarge`;
- `ReadBufferTooLarge`;
- `OutboundQueueFull`;
- `OutboundBytesFull`;
- `AppMailboxFull`;
- `PeerClosed`;
- `Closing`;
- `Closed`;
- `ProtocolError`;
- `PingTimeout`;
- `CloseTimeout`.

Avoid one giant "error soup" if the names become too broad. Use a small public
send error for send admission and a session close/report reason for lifecycle.

## Rock 4: Protocol Hardening

Keep 087's hostile fixes and extend tests around protocol edges.

Required behavior:

- method/header/version/key validation;
- plain TCP WebSocket upgrade and message path;
- TLS WebSocket upgrade and message path through `HttpsListener`;
- unsupported extensions rejected or explicitly omitted;
- RSV bits rejected without negotiated extensions;
- client frames must be masked;
- server frames must be unmasked;
- control frames max 125 bytes;
- control frames must not fragment;
- close payload: empty, valid code/reason, invalid one-byte, invalid code,
  invalid UTF-8;
- text frames validate UTF-8;
- binary frames preserve bytes;
- ping sends pong promptly;
- app-initiated ping can time out visibly;
- close reply and close timeout both close the resource;
- peer FIN closes resource and reports peer close;
- multiple frames in one read are drained without waiting for more bytes;
- large length encodings are checked before allocation.

Fragmentation decision:

- if rejected: continuation frames and `FIN = 0` data frames produce typed
  protocol close and docs say "fragmented data messages unsupported";
- if supported: implement bounded reassembly, interleaved control frames,
  `max_message_bytes`, and tests for split text/binary plus oversize
  reassembly.

## Rock 5: Real Room Specimen

Upgrade `examples/specimen_websocket_room` from smoke shape to a real
multi-client room specimen.

Shape:

- room isolate owns member table;
- each WebSocket session has a stable session id/handle;
- join and leave are visible room messages;
- two or more live clients connect in the smoke test;
- a message from client A reaches client B;
- client B can reply and client A receives it;
- slow/non-reading peer is a real live socket that stops reading, not only a
  unit fake;
- slow/non-reading peer creates visible pressure before memory can grow without
  bound;
- app policy is explicit: drop message, shed peer, or close peer;
- shutdown closes sessions and room cleanly.

The specimen must not use `tokio-tungstenite`, axum WebSocket, or any broad
WebSocket client crate. A tiny local test client helper is allowed for smoke
testing Tina's server path.

The specimen README must explain:

- what Tina owns;
- what the room owns;
- how slow peer policy is chosen;
- what remains out of scope.

The specimen should expose a small `run()` report shaped for tests and a copied
`main`/README path shaped for users. If a browser manual smoke is included, it
must be separate from CI and not become a hidden dependency.

## Rock 6: Tests

Add tests at three levels.

Unit/pure codec tests:

- accept key RFC example;
- frame parse small/126/127 lengths;
- masked decode;
- unmasked client reject;
- RSV reject;
- control-frame rules;
- close payload decode matrix;
- outbound queue count and byte caps.

Live `tina-http` tests:

- valid upgrade computes expected response;
- copied upgrade/session example compiles against public API;
- bad upgrade headers reject with typed error;
- unsupported extension rejects or is explicitly omitted;
- cleartext listener upgrade/text/ping path works;
- TLS listener upgrade/text/ping path works;
- text echo works;
- binary echo works;
- multiple frames in one read drain;
- unmasked client frame rejects;
- fragmented control frame rejects;
- oversized control frame rejects;
- RSV reject;
- bad close payload rejects;
- ping produces pong;
- app ping timeout closes visibly;
- pong satisfies liveness;
- peer close produces close outcome and closes stream;
- app close handshake timeout closes stream;
- peer FIN closes stream;
- oversized frame rejects and closes;
- read buffer high-water rejects;
- outbound bytes cap visible, including active write;
- outbound queue full visible;
- app mailbox full visible if reachable;
- slow/non-reading peer cannot create unbounded memory;
- HTTP/2 WebSocket remains unsupported and does not accidentally claim upgrade.

Specimen tests:

- two-client join/broadcast;
- leave removes member;
- slow/full peer pressure reported distinctly;
- chosen slow-peer policy executes;
- shutdown closes sessions;
- specimen public `run()` report proves the same user-visible facts as the
  live smoke, not a different unit-only path.

If live socket timing makes a case flaky, build a deterministic test helper or
simulated rail proof. Do not paper over flakiness with sleeps.

## Rock 7: Public API Stability Pass

Before finishing, do a naming pass.

Keep stable:

- `WebSocketLimits`;
- `WebSocketUpgradeRequest`;
- `WebSocketAccept`;
- `websocket_upgrade`;
- `WebSocketSessionMsg`;
- `WebSocketSessionOutcome`.

Add only if needed and commit to them:

- `WebSocketSessionHandle`;
- `WebSocketSend`;
- `WebSocketSendOutcome`;
- `WebSocketSendError`;
- `WebSocketSessionReport`;
- `WebSocketCloseReason`.

Avoid leaking internal codec types unless tests or users need them. If a helper
is test-only, keep it test-only.

Run a small "new user" audit:

- a service can accept an upgrade without reading private modules;
- an app can echo without understanding HTTP internals;
- a room can store/remove session handles without fighting lifetimes;
- failures a user must handle appear in public enums, not logs or traces only.

If one of these requires too much ceremony, add a tiny helper rather than a
framework.

## Rock 8: User-Realistic End-To-End Proof

Add one e2e test or specimen path that exercises the API the way a user would.

Minimum:

1. Start a Tina HTTP listener with the public WebSocket upgrade API.
2. Register a room isolate that stores session handles.
3. Connect at least two raw WebSocket clients over TCP.
4. Broadcast between clients through the room, not by directly calling session
   internals.
5. Stop one client from reading and prove the chosen slow-peer policy through a
   typed report.
6. Close/shutdown and prove sessions do not leak.

Also add a TLS e2e test for the native rail claim:

1. Start `HttpsListener` with test certs.
2. Complete a WebSocket upgrade over TLS.
3. Send text and ping/pong.
4. Close cleanly.

Do not require a real browser in CI. If a browser smoke is useful, make it a
manual example or ignored test with clear instructions. The CI proof can use a
tiny local test client as long as docs show browser interoperability.

## Docs

Add or update docs so a user can build:

- echo server;
- room server;
- slow-peer policy;
- graceful shutdown.

Suggested doc wording for the claim:

```text
`tina-http` supports native bounded server-side WebSockets over HTTP/1.1.
It owns the TCP/TLS stream through upgrade, validates client masking, emits
unmasked server frames, exposes ping/pong and close lifecycle, and surfaces
backpressure as typed outcomes.
```

Docs must also include "Still out of scope":

- HTTP/2 WebSocket;
- permessage-deflate;
- native broad client;
- browser auth/session framework;
- automatic reconnect;
- full protocol compliance suite.

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina-http websocket --tests`
- `cargo test -p tina-http websocket_tls --tests` or equivalent targeted TLS
  WebSocket test name
- `cargo clippy -p tina-http --tests -- -D warnings`
- `cargo test --manifest-path examples/specimen_websocket_room/Cargo.toml`
- specimen smoke test with live multi-client room
- `cargo test -p tina-http http2 --tests` if shared HTTP response/listener
  code changed
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` if docs/rustdoc
  changed

## Done Means

- We can honestly say "Tina supports native bounded server-side WebSockets for
  HTTP/1.1."
- Room/fanout uses explicit bounded session handles or an equivalent clear API.
- Every queue/buffer has a named cap and typed failure.
- Masking, ping/pong, close, pressure, and protocol errors are tested live.
- The room specimen proves multi-client broadcast and slow-peer policy.
- Docs include copyable examples and do not overclaim.

## Hostile Review Notes

- Risk: session handles become a hidden broker.
  Fix: handles must send through bounded session mailboxes and typed outcomes.
- Risk: room specimen cheats with in-memory direct calls instead of live sockets.
  Fix: smoke test must use multiple real TCP WebSocket clients.
- Risk: "bounded bytes" forgets the active write.
  Fix: active write plus queued frames count against outbound bytes.
- Risk: liveness becomes magic.
  Fix: ping/pong and close timers are visible and typed.
- Risk: docs claim more than tests prove.
  Fix: each public claim has a named test or specimen check.
- Risk: fragmentation half-support is worse than rejection.
  Fix: either keep rejection explicit or implement full bounded reassembly rules.
