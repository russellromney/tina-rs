# Phase 116: Native Protocol Client Parity

## Status

- Future implementation plan for Wave A.
- Runs after Phase 115 lands. Also prefer Phase 123/124 HTTP/2 hardening on
  main first, because this phase reuses the same frame/header code.
- Can run in parallel with phases 117 and 118 if ownership stays in `tina-http`,
  TLS ALPN rail data, protocol facts, docs, and protocol specimens.
- Runs before Phase 119 resource maturity. HTTP/2/gRPC client pooling needs
  the real client connection shape first.

## Purpose

Make Tina a native client, not only a native server.

The user story:

```text
my Tina service calls another HTTP/2/gRPC service without Tokio
```

## Spike Facts

- Server-side HTTP/2 h2c already exists in `tina-http/src/http2.rs`.
- Server-side gRPC already exists in `tina-http/src/grpc.rs` with unary,
  server-streaming, client-streaming, bidi-shaped routes, and tonic h2c tests.
- `grpc_unary_call_h2c_blocking` is only a blocking specimen/test helper. It
  is not a Tina client isolate and emits no client runtime lifecycle truth.
- HTTP/2 frame decode/encode and HPACK helpers are private inside server
  `http2.rs`. Client work must split those into shared internal modules first.
- TLS has no ALPN today. `tls_connect` / `tls_accept` return only stream ids.
  This phase owns ALPN config and selected-protocol truth.
- Protocol facts now exist as runtime/sim facts. Client-received gRPC status
  and HTTP/2 lifecycle facts should use that path, not private counters only.

## Includes

- split HTTP/2 frame/header helpers out of server-only `http2.rs` into shared
  internal code used by server and client:
  - frame encode/decode
  - SETTINGS / PING / GOAWAY / RST_STREAM / WINDOW_UPDATE builders
  - HPACK header encode/decode helpers
  - protocol error mapping to HTTP/2 wire error codes
- native HTTP/2 client connection isolate
- bounded HTTP/2 client stream-slot admission; do not model one request as one
  leased connection
- one caller request owns one HTTP/2 stream slot until response trailers/EOF,
  reset, timeout, or caller cancellation
- native gRPC client surface over that HTTP/2 client connection
- unary, server-streaming, client-streaming, and bidi client paths
- TLS ALPN rail support for `h2`:
  - ALPN protocols on TLS bind/connect config
  - selected protocol in TLS connect/accept output
  - typed ALPN mismatch/failure truth
- authority/SNI/Host rules copied from the HTTP/1/TLS lessons
- h2c and h2/TLS target types; no string bag that can forget SNI/authority
- client connection reuse keyed by authority plus TLS/root config
- first-form reuse is "one client connection isolate can carry many admitted
  streams." Idle eviction, max lifetime, health policy, and pool ownership are
  Phase 119.
- received gRPC status as protocol fact after Phase 112
- live interop tests against a real tonic/h2c or h2 server
- simulator returns typed unsupported facts for live HTTP/2 client socket work
  until a scripted HTTP/2 client simulator lands; protocol status facts still
  use stable names for later replay plumbing
- update docs that currently say native gRPC is server-first
- replace the blocking helper docs so users copy the client isolate path, not
  the test helper

## Does Not Include

- no gRPC reflection
- no load balancing
- no interceptor framework
- no broad web framework
- no hidden Tokio client
- no generic resource pool policy; Phase 119 owns idle/max-lifetime/health
- no HTTP/2 server rewrite beyond sharing frame/header helpers
- no client load balancing; one configured authority is one client target

## Implementation Shape

- `tina-http` gains shared internal HTTP/2 modules before behavior changes:
  - `http2/frame.rs`
  - `http2/headers.rs`
  - `http2/errors.rs`
  - `http2/flow.rs` only if moving window/accounting code avoids duplication
- Existing server tests must stay green after the split before client behavior
  lands.
- Do not expose these frame/header modules as public API. They are internal
  shared code for server/client.
- `Http2ClientConnection` is an isolate over one TCP/TLS stream:
  - sends client preface and SETTINGS;
  - opens odd-numbered streams;
  - enforces `max_concurrent_streams`;
  - tracks per-stream request body, response body, trailers, reset, timeout,
    and caller cancellation;
  - reports pressure and lifecycle.
- User-facing names should be target-shaped:
  - `Http2Target::H2c { authority, addr }`
  - `Http2Target::Tls { authority, addr, server_name, trust_roots }`
  - `GrpcTarget` wraps `Http2Target` plus service defaults, not a string URL
    bag.
- HTTP/2 client admission returns typed outcomes:
  - `Admitted`
  - `Full`
  - `Closed`
  - `Timeout`
  - `Reset`
  - `ProtocolError`
  - `TlsAlpnMismatch`
- gRPC client wrappers sit above `Http2ClientConnection`; they do not own a
  hidden queue or hidden runtime.
- gRPC copied path:
  - `GrpcClient::unary(...)`
  - `GrpcClient::server_streaming(...)`
  - `GrpcClient::client_streaming(...)`
  - `GrpcClient::bidi(...)`
  each returns one Tina call/effect shape with explicit status outcome.
- TLS ALPN extends the existing TLS rail:
  - no ambient default;
  - h2 target asks for `["h2"]`;
  - h2c target never touches TLS;
  - selected ALPN is visible in typed connect/accept output.

## Proof Shape

- server split proof:
  - existing HTTP/2 and gRPC server tests pass unchanged after frame/header
    helper extraction
- live HTTP/2 client proof:
  - h2c GET/POST happy path against Tina server
  - response DATA arrives in bounded chunks
  - request DATA streaming is bounded
  - server RST_STREAM maps to typed reset
  - GOAWAY closes new admission but lets admitted streams settle visibly
  - flow-control blocked path is counted/reported
  - timeout/cancel frees stream slot and rejects late response truth visibly
- interop proof:
  - Tina HTTP/2 client talks to Tina HTTP/2 server
  - Tina gRPC client talks to Tina gRPC server
  - Tina gRPC unary talks to tonic h2c
  - tonic/h2 client talks to Tina server still passes after shared-code split
- TLS ALPN proof:
  - h2/TLS success selects `h2`
  - no shared protocol / wrong ALPN returns typed mismatch
  - cert/name failures remain distinct from ALPN failure
- gRPC client proof:
  - unary client against Tina server
  - unary client against tonic h2c server
  - server-streaming client receives all messages and final status
  - client-streaming sends multiple messages and receives final status
  - bidi client proves request and response streams progress independently
  - non-OK gRPC status is the caller outcome, not an HTTP transport success
- reuse/close proof:
  - N unary calls reuse one HTTP/2 client connection when healthy
  - closed/reset/protocol-bad connection rejects new admission visibly
  - caller cancellation does not poison unrelated streams
- replay/protocol fact proof:
  - protocol facts emitted for received statuses and stream lifecycle
  - simulator returns typed unsupported fact for live client socket work, not a
    silent no-op or fake replay
- compile-fail proof:
  - h2c target cannot carry TLS roots/SNI
  - h2/TLS target must carry server name/root policy
  - unary helper cannot accept a streaming request body
  - gRPC client status outcome must be handled as status, not collapsed into
    HTTP success

## User Specimens

- Add/update one protocol specimen that calls a Tina gRPC server from a Tina
  gRPC client without Tokio.
- Add/update one system specimen that uses outbound gRPC client calls from a
  service handler and shuts down cleanly.
- README must show copied path, not `grpc_unary_call_h2c_blocking`.

## Hostile Review Notes

- Do not build a second HTTP client that owns one TCP connection per request.
- Do not put ALPN in docs only. Selected protocol must be typed runtime truth.
- Do not let gRPC status disappear inside `Ok(HttpResponse)`.
- Do not add broad pool policy here. Reuse is connection capability; resource
  lifecycle policy is Phase 119.
