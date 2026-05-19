# Phase 128: Native Protocol Completion

## Status

- Future implementation plan for the second post-122 core wave.
- Runs after Phase 116. Should share lifecycle words with Phase 127.

## Purpose

Close the remaining "I still need Tokio for this protocol" holes.

User story:

```text
my HTTP/2, gRPC, and WebSocket client/server paths can stay native Tina for
normal production-shaped services
```

## Includes

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

- no web framework
- no full Envoy replacement
- no transparent reconnect
- no unbounded client pool
- no HTTP/3/QUIC
- no gRPC reflection unless it stays bounded and useful
- no redo of Phase 116 first-form clients. This phase finishes production
  client gaps: security, pooling, interop, and protocol facts.

## Must Not Change

- Native server protocol behavior from HTTP/1, HTTP/2, gRPC, and WebSocket stays
  compatible with current tests.
- Existing client APIs from Phase 116 remain source-compatible unless this
  phase adds a compile-time safety improvement with migration tests.
- Authority/SNI/Host truth stays explicit; no convenience default may hide a
  mismatch.

## Implementation Shape

Names should be protocol-user words:

```text
Http2Client
GrpcClient
WebSocketClient
TlsAlpn
ClientProtocolFact
ClientSessionReport
```

Rules:

- Authority, SNI, Host, and ALPN are explicit and tested.
- Client pools report connection count, active streams, full, closed, retired,
  late, high-water.
- gRPC status/trailers become typed outcomes and protocol facts.
- WebSocket client owns ping/pong, close handshake, slow-peer policy, and
  bounded outbound queue.
- Client cancellation means Tina stops waiting; Tina-owned rails close when the
  contract supports it.

## User Proof Specimens

- Tina WebSocket client talks to Tina server and browser-compatible server
- Tina HTTP/2 client talks to Tina server over TLS ALPN
- Tina gRPC client talks to tonic server and Tina server
- pooled HTTP/2/gRPC client reuses one connection across many requests and
  retires bad connections

## Required Proof

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
- blast-radius proof: existing native HTTP/2/gRPC/WebSocket server tests and
  Phase-116 client tests still pass

## Hostile Review Notes

- Do not sneak Tokio protocol clients under "native."
- Do not hide SNI/Host/ALPN defaults.
- Do not claim broad client parity without real interop.
- Do not make client pooling one fake abstraction across HTTP/2/gRPC/WebSocket
  if their lifecycles differ.
