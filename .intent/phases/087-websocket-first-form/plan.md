# 087 WebSocket First Form

## Status

- Ready to implement.
- One PR.
- Can run beside 088/089 because this owns `tina-http` WebSocket surface and
  specimens, not bridges or replay tooling.

## Grug Truth

WebSocket is a long-lived HTTP upgrade.

After upgrade, there are two directions.

Reads can flood.

Writes can flood.

Slow peers must be visible.

Ping/pong is liveness, not magic.

Close is a handshake, then resource close.

No hidden unbounded room queue.

## Goal

Add native WebSocket first form on Tina-owned TCP/TLS/HTTP rails.

Use a sync codec crate for frame parsing/formatting if useful, probably
`tungstenite` core. Tina owns the connection/session isolates, mailboxes,
capacity, timers, cancellation, trace, and shutdown truth.

First form should support:

- server-side HTTP/1.1 upgrade;
- one session isolate per upgraded connection;
- text and binary frames;
- ping -> pong;
- close frame -> close reply/close resource;
- bounded inbound frame size;
- bounded outbound send queue;
- slow-reader / slow-writer pressure;
- a tiny room/broadcast specimen;
- one tiny test/client helper only if needed to prove the server path.

## Non-Goals

- no HTTP/2 WebSocket;
- no permessage-deflate;
- no broad web framework;
- no browser auth/session framework;
- no reconnect framework;
- no full WebSocket client crate in this slice;
- no hidden Tokio;
- no unbounded broadcast queue;
- no unbounded fragmented-message reassembly;
- no pretending live WebSocket sessions are fully sim-replayable unless the
  simulator rail has all scripted facts.

## Rock 0: Pick The API Home

Decide early.

Likely home:

- `tina-http/src/websocket.rs`;
- re-export from `tina-http/src/lib.rs`;
- tests in `tina-http/tests/websocket_*.rs`;
- specimen under `examples/specimen_websocket_room`.

Keep WebSocket types in `tina-http`, not `tina-runtime`.

Also decide the stream handoff path before coding:

- reuse the existing HTTP listener/connection ownership model;
- do not fork a second HTTP server stack;
- if the current connection isolate cannot hand off an upgraded stream cleanly,
  add the smallest explicit handoff message/report and test it.

## Rock 1: Upgrade Shape

Add a server upgrade path from `HttpRequest`.

The copied path should feel like:

```rust
match websocket_upgrade(&request, limits) {
    Ok(upgrade) => accept_websocket(upgrade, Session::new(...)),
    Err(err) => reply(HttpResponse::bad_request(...)),
}
```

Pin exact first-form vocabulary:

- `WebSocketUpgradeRequest`;
- `WebSocketAccept`;
- `WebSocketError`;
- `WebSocketLimits`;
- `WebSocketSessionMsg`;
- `WebSocketSessionOutcome` if calls need replies.

The upgrade must validate:

- method is `GET`;
- `Upgrade: websocket`;
- `Connection: Upgrade`;
- `Sec-WebSocket-Key`;
- supported version;
- no unsupported extension silently accepted.

Unsupported extension means reject or accept without it only when the response
explicitly omits it. Do not say permessage-deflate works.

## Rock 2: Session Isolate

One session owns one upgraded stream.

It should:

- arm one read at a time;
- decode frames incrementally;
- reject oversized frames visibly;
- handle fragmentation honestly;
- expose inbound messages to the app through bounded sends/calls;
- accept outbound text/binary/close commands through a bounded mailbox;
- track ping/pong;
- close the stream on protocol error or close handshake timeout.

Do not split read/write into unbounded helper threads.

If reader/writer split is needed, use two isolates with explicit bounded
mailboxes and a lifecycle report. Keep first form boring.

Fragmentation first form:

- either reject continuation frames with a typed protocol error;
- or support them with `max_message_bytes` and no unbounded reassembly.

Pick one in Rock 0. Test it.

## Rock 3: Backpressure

Name the budgets.

Required caps:

- max frame bytes;
- max message bytes if fragmented messages are supported;
- inbound app mailbox capacity;
- outbound frame queue capacity;
- broadcast fanout max targets in specimen;
- ping/pong timeout;
- close handshake timeout.

If new trace event kinds are added, append stable hash tags. Do not renumber old
tags.

Every overflow returns or traces a typed fact:

- frame too large;
- outbound queue full;
- app mailbox full;
- peer closed;
- protocol error;
- timeout.

## Rock 4: Room Specimen

Add `examples/specimen_websocket_room`.

Shape:

- room isolate owns member list;
- each connection/session isolate owns one peer;
- join/leave are visible messages;
- broadcast to N peers is bounded;
- slow peer causes visible `Full` / dropped peer / backpressure report;
- shutdown closes sessions and room.

This is a specimen, not a web framework.

## Rock 5: Tests

Required tests:

- valid upgrade computes expected accept response;
- bad upgrade headers reject with typed error;
- text echo works;
- binary echo works;
- ping produces pong;
- peer close produces close outcome and closes stream;
- oversized frame rejects and closes;
- outbound queue full is visible;
- slow reader or non-reading peer does not create unbounded memory;
- room broadcast reports a slow/full peer distinctly;
- shutdown closes session resource;
- parser/framing has at least one deterministic simulator or unit proof if the
  live rail is too physics-heavy.
- close handshake timeout produces typed close truth.

## Docs

Update:

- `docs/tina-user-guide/18-bridge-crates.md` if WebSocket reduces a bridge need;
- native HTTP docs or README with copied server shape;
- specimen README with Tokio comparison and Tina pressure truth.

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina-http websocket --tests`
- `cargo clippy -p tina-http --tests -- -D warnings`
- specimen smoke test
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` if docs/rustdoc
  changed

## Done Means

- native WebSocket upgrade/session path exists;
- room specimen runs;
- fragmented-message behavior is pinned and tested;
- slow peer pressure is visible;
- close/cancel/shutdown truth is tested;
- docs say what is unsupported.
