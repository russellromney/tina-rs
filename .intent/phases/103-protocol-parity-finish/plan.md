# Phase 103: Protocol Parity Finish

## Status

- IDD implementation phase.
- Not started.
- This absorbs the old HTTP/2/gRPC finish row and the WebSocket replacement follow-up.

## Grug Truth

Real services speak protocols, not demos.

If Tina claims "bounded actor-style network services can replace Tokio here,"
then HTTP/2, gRPC, and WebSocket must survive real peers, slow peers, resets,
TLS, backpressure, and shutdown. First form is not enough now.

## Goal

Make Tina's native protocol surface good enough for one serious replacement
story:

- HTTP/2 server and client paths have bounded flow-control truth.
- gRPC has unary plus streaming modes with final-status ownership.
- WebSocket has browser `ws`/`wss`, backpressure, ping/pong, close, fanout, and
  slow-peer behavior that can be explained without hand waving.
- All claimed modes have interop tests against real clients.

## Non-Goals

- No broad web framework.
- No automatic retry.
- No hidden queues to look like Tokio.
- No compression unless a real test needs it.
- No "supports gRPC" claim for modes not proved by tonic/grpcurl or equivalent.

## Rocks

### Rock 1: HTTP/2 Flow-Control Truth

Pin the HTTP/2 state machine:

- inbound DATA cap
- outbound DATA cap
- stream window behavior
- connection window behavior
- reset/cancel behavior
- trailers/final status behavior

Expose typed errors for peer reset, local cancel, flow-control full, malformed
frame, timeout, and closed connection.

### Rock 2: gRPC Streaming Finish

Build the remaining gRPC modes on top of the HTTP/2 truth:

- server streaming
- client streaming, with a hostile review of any existing first form before
  calling it done
- bidirectional streaming
- final-status/trailers ownership
- cancel/reset mapping

Each mode must have a copied service example and an interop proof.

### Rock 3: WebSocket Production Replacement

Finish the WebSocket server replacement surface:

- browser `ws`
- browser `wss`
- subprotocol selection
- ping/pong
- close handshake
- bounded send queue
- slow-reader eviction
- broadcast/fanout pressure
- per-session state
- graceful server shutdown

Keep the room/session helper small if repeated specimens prove the shape. Do
not build a framework before evidence.

### Rock 4: Client-Side Protocol Parity

If a server mode is claimed, the client story must be named:

- native Tina client exists, or
- external client interop is the supported proof path, or
- the mode is server-only and docs say so.

No ambiguous "works with HTTP/2" copy.

### Rock 5: Simulator Facts

Add simulator/runtime events for the protocol facts users debug:

- stream opened/closed/reset
- flow-control full
- body high-water
- websocket slow-peer close
- grpc final status sent/received

Stable trace hashes must not churn for unrelated effects.

## Required Proof

- `cargo fmt --all --check`
- `cargo clippy -p tina-http --all-targets -- -D warnings`
- HTTP/2 live tests for full-duplex, reset, slow reader, flow-control full.
- gRPC interop tests against tonic/grpcurl for each claimed mode.
- WebSocket browser or browser-like test for `ws` and `wss`.
- WebSocket bad-peer tests: malformed frame, reset, idle, slow reader, close.
- No hidden unbounded collection in protocol paths.
- At least one DST replay case for a protocol pressure/lifecycle bug.

## Done Means

A user can look at the docs and know exactly which protocol modes Tina can
replace today, which modes are server-only, and which modes are still out of
scope.
