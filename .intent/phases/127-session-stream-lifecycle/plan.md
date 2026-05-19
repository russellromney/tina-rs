# Phase 127: Native Session And Protocol Completion

## Status

- Future implementation plan for the second post-122 core wave.
- Combines the old session/stream lifecycle and native protocol completion
  plans.
- Runs after Phase 116. Should reuse Phase 119 resource lifecycle words.

## Purpose

Make long-lived native protocol sessions production-shaped.

User story:

```text
my WebSocket, HTTP/2, gRPC, and TCP sessions all open, drain, close, cancel,
report, and run native clients without per-protocol weirdness or Tokio fallback
```

## Includes

- shared lifecycle vocabulary: open, active, idle, draining, closed, failed
- close/cancel/drain/report behavior across TCP sessions, HTTP/2 streams, gRPC
  streams, and WebSocket sessions
- slow-peer policy
- half-close/reset/idle-timeout truth
- per-session pressure report
- graceful server shutdown draining sessions with deadline
- request-scoped cancel feeding session close/cancel reports
- native broad WebSocket client
- HTTP/2 TLS ALPN
- mTLS if rustls support fits the existing TLS shape
- gRPC client polish: metadata/interceptors, status facts, deadline mapping,
  streaming ergonomics
- bounded gRPC endpoint policy: explicit fixed endpoint list, round-robin or
  first-healthy selection, typed no-healthy-endpoint outcome; no discovery
- pooled HTTP/2/gRPC clients with lifecycle/pressure reports
- client-side protocol facts, not only server-side facts
- real-client/server interop tests

## Does Not Include

- no remoting/clustering
- no global session manager
- no fake cancellation of external work
- no per-protocol lifecycle dialect unless the protocol truly needs it
- no hidden retry or reconnect
- no web framework
- no full Envoy replacement
- no unbounded client pool
- no HTTP/3/QUIC
- no gRPC reflection unless it stays bounded and useful
- no redo of Phase 116 first-form clients. This phase finishes production
  client gaps: lifecycle, security, pooling, interop, and protocol facts.

## Must Not Change

- Existing HTTP/1, HTTP/2, gRPC, and WebSocket success paths keep their current
  wire behavior.
- Existing protocol error mappings stay stable unless this phase explicitly
  adds a more precise typed outcome and updates tests.
- Existing request-scoped cancellation truth remains: Tina stops waiting and
  Tina-owned rails close only where the rail contract supports it.
- Existing Phase-116 client APIs remain source-compatible unless this phase adds
  a compile-time safety improvement with migration tests.
- Authority/SNI/Host truth stays explicit; no convenience default may hide a
  mismatch.

## Implementation Shape

Use common protocol/session nouns:

```text
SessionState
SessionCloseReason
SessionDrainReport
SessionPressureReport
SlowPeerPolicy
IdlePolicy
Http2Client
GrpcClient
WebSocketClient
TlsAlpn
ClientProtocolFact
ClientSessionReport
```

Rules:

- Stop accepting new work before drain.
- Drain waits for accepted in-flight work until a deadline.
- Force close marks still-owned work closed/stale and reports it.
- Slow-peer action is explicit: shed message, close session, or backpressure.
- Half-close and reset are different outcomes.
- Idle timeout closes or reports why close could not run.
- Reports include accepted, completed, cancelled, reset, timed out, full,
  high-water, final-current.
- Authority, SNI, Host, and ALPN are explicit and tested.
- Client pools report connection count, active streams, full, closed, retired,
  late, high-water.
- gRPC status/trailers become typed outcomes and protocol facts.
- WebSocket client owns ping/pong, close handshake, slow-peer policy, and
  bounded outbound queue.
- Client cancellation means Tina stops waiting; Tina-owned rails close when the
  contract supports it.

## User Proof Specimens

- chat/room service: many sessions, one slow peer, active peer continues
- gRPC bidi stream: shutdown drains accepted messages and rejects late sends
- HTTP/2 streaming response reset: body source receives cancel/close truth
- TCP half-close service: peer close does not leak rail ownership
- Tina WebSocket client talks to Tina server and browser-compatible server
- Tina HTTP/2 client talks to Tina server over TLS ALPN
- Tina gRPC client talks to tonic server and Tina server
- pooled HTTP/2/gRPC client reuses one connection across many requests and
  retires bad connections

## Required Proof

- WebSocket slow reader evicted or backpressured with report
- HTTP/2 stream reset while body source is active settles body source visibly
- gRPC bidi shutdown drains accepted messages and rejects late messages
- TCP half-close and reset do not leak resource ownership
- graceful server shutdown stops ingress, drains sessions, force-closes
  leftovers, emits report
- idle timeout does not leave a live owned stream with no owner
- simulator or protocol-fact replay records supported lifecycle facts;
  unsupported byte-level replay is declared explicitly
- HTTPS HTTP/2 ALPN interop succeeds and wrong ALPN fails visibly
- Host/SNI/authority mismatch is accepted/rejected by documented rule
- mTLS success and bad client cert failure if mTLS lands
- gRPC unary/server-stream/client-stream/bidi client proofs with status facts
- remote gRPC error status becomes typed caller outcome and runtime fact
- WebSocket client handles ping, pong, close, fragmented messages, slow peer,
  and oversized frame
- pooled HTTP/2 client proves one connection handles N requests where allowed
- dead pooled connection retires and next request reconnects or reports typed
  failure
- blast-radius proof: existing HTTP/1 keepalive, HTTP/2 strictness, native
  HTTP/2/gRPC/WebSocket server tests, Phase-116 client tests, and WebSocket
  browser tests still pass

## Hostile Review Notes

- Do not let each protocol invent names for the same lifecycle.
- Do not call a session closed if the rail is still owned and live.
- Do not hide slow-peer buffering.
- Do not make graceful shutdown docs stronger than the code.
- Do not sneak Tokio protocol clients under "native."
- Do not hide SNI/Host/ALPN defaults.
- Do not claim broad client parity without real interop.
- Do not make client pooling one fake abstraction across HTTP/2/gRPC/WebSocket
  if their lifecycles differ.
