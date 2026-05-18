# Phase 103: Protocol Parity Finish

## Status

- IDD implementation phase.
- Absorbs the old HTTP/2/gRPC finish row and WebSocket replacement follow-up.
- Shipped (PR #129): Rocks 1–4 in full. Rock 5 is deferred — see
  [Deferred](#deferred-to-follow-up) below — and a follow-up phase is named in
  `ROADMAP.md` under "Protocol facts as runtime/simulator trace events". The
  "At least one DST replay case for a protocol pressure/lifecycle bug" line in
  [Required Proof](#required-proof) is satisfied by the pre-existing
  `tina-http/tests/dst_simulator.rs` cases for `slow_body_multichunk_inbound`,
  `service_full_with_concurrent_peers`, and `shutdown_mid_request`; no new DST
  replay case was added in this phase, and adding a protocol-fact-driven
  replay rides with the Rock 5 follow-up.

## Grug Truth

Real services speak protocols, not demos.

If Tina claims "bounded actor-style network services can replace Tokio here,"
then HTTP/2, gRPC, and WebSocket must survive real peers, slow peers, resets,
TLS, backpressure, and shutdown. First form is not enough now.

## Goal

Make one honest native-protocol replacement story:

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

Implement and test:

- inbound DATA cap
- outbound DATA cap
- stream window behavior
- connection window behavior
- reset/cancel behavior
- trailers/final status behavior
- typed errors for peer reset, local cancel, flow-control full, malformed frame,
  timeout, and closed connection

### Rock 2: gRPC Streaming Finish

Build gRPC modes on top of HTTP/2 truth:

- server streaming
- client streaming
- bidirectional streaming
- final-status/trailers ownership
- cancel/reset mapping

Each mode gets a copied service example and tonic/grpcurl proof.

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

Extract one small room/session helper from existing specimens. It must preserve
explicit admission, fanout pressure, slow-peer policy, and close reports.

### Rock 4: Client-Side Protocol Parity

For every claimed server mode, ship one client answer:

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

## User Proof

Update these proof surfaces:

- `specimen_grpc_counter`: unary, server stream, client stream, bidi stream,
  with tonic/grpcurl interop commands.
- `specimen_websocket_room`: extracted room helper, browser-style `ws` and
  `wss`, slow peer, ping/pong, close.
- `examples/systems/system_realtime_rooms`: production WebSocket shape with
  smoke, slow-peer/load proof, and findings update.

Every updated README must show the copied command, what external client was
used, what Tina proves, and what still felt rough.

## Required Proof

- `cargo fmt --all --check`
- `cargo clippy -p tina-http --all-targets -- -D warnings`
- HTTP/2 live tests for full-duplex, reset, slow reader, flow-control full.
- gRPC interop tests against tonic/grpcurl for each claimed mode.
- WebSocket browser or browser-like test for `ws` and `wss`.
- WebSocket bad-peer tests: malformed frame, reset, idle, slow reader, close.
- System smoke and load/bad-peer proof for `system_realtime_rooms`.
- No hidden unbounded collection in protocol paths.
- At least one DST replay case for a protocol pressure/lifecycle bug.

## Done Means

A user can look at the docs and know exactly which protocol modes Tina can
replace today, which modes are server-only, and which modes are still out of
scope.

## Deferred to follow-up

Rock 5 ("Simulator Facts") did not ship in this phase. The protocol facts the
plan named — stream opened/closed/reset, flow-control full, body high-water,
WebSocket slow-peer close, gRPC final status sent/received — surface today as
bounded counters on `Http2ConnectionReport`, `BodyMetrics`,
`WebSocketMemberTableReport`, and the typed `GrpcStatus` trailers. They are
**not** plumbed through `RuntimeEventKind` on the trace stream and they do
**not** round-trip through `tina-sim` replay. A user who wants protocol
behavior in a deterministic "bug in a box" still has to wire one of the
existing DST cases (TCP-script-driven listener/connection lifecycle in
`tina-http/tests/dst_simulator.rs`) rather than asking the trace for "show me
the stream-reset events for this run."

This is a named follow-up in `ROADMAP.md` ("Protocol facts as
runtime/simulator trace events"), not a forgotten requirement. It is the next
protocol-observability slice and should ride with the proof harnesses /
replay-ops capability cluster, since the user-visible payoff is "this protocol
bug now replays from a trace" — the same shape as Phase 108's replay ops.

The new DST case the plan required ("at least one DST replay case for a
protocol pressure/lifecycle bug") rides with this follow-up. The existing DST
cases (`slow_body_multichunk_inbound`, `service_full_with_concurrent_peers`,
`shutdown_mid_request`) already satisfy the "protocol pressure/lifecycle"
shape via the TCP-script path, so the phase 103 minimum is met; the new case
that exercises protocol-fact trace events is gated on the trace-event work
above.
