# 096 Finish Native gRPC Streaming

## Status

- IDD phase.
- Not ready to implement until 095 HTTP/2 streaming substrate ships.
- One PR when implemented unless tonic/grpcurl interop forces a follow-up.
- Builds on Phase 057 unary gRPC and Phase 095 HTTP/2 bidirectional streaming
  substrate.
- Owns `tina-http/src/grpc.rs`, gRPC streaming tests, gRPC specimen/docs, and
  any narrow generated/manual service trait templates needed for streaming.
- Do not run beside HTTP/2 streaming substrate implementation unless file
  ownership is coordinated.

## Grug Truth

gRPC streaming is HTTP/2 streaming plus protobuf frames plus status.

If HTTP/2 pressure is hidden, gRPC streaming lies.

Do not build tonic.

Interop is a test, not a vibe.

One request stream and one response stream have independent lifecycles.

Final status trailers are not optional.

Peer cancel must reach the source/sink.

## Goal

Finish native gRPC enough that Tina can honestly claim:

```text
unary, server-streaming, client-streaming, and bidirectional gRPC over native
HTTP/2 h2c, with bounded messages, typed status, explicit cancel/deadline truth,
and no hidden Tokio runtime.
```

The claim this phase may make:

- unary path remains compatible with Phase 057;
- server-streaming: one request message, many response messages, final status;
- client-streaming: many request messages, one response message, final status;
- bidirectional streaming: many request messages and many response messages,
  full-duplex over the HTTP/2 substrate;
- typed `GrpcStatus` / `GrpcStatusCode` everywhere;
- message compression is still rejected unless explicitly implemented and
  tested;
- per-message and per-stream caps are explicit;
- peer reset/cancel/deadline reaches the Tina service/source/sink;
- tonic/grpcurl interop is tested for the modes claimed, or explicitly listed
  as deferred.

## Non-Goals

- no tonic compatibility layer;
- no interceptor framework;
- no reflection unless grpcurl interop requires and it is deliberately scoped;
- no load balancing;
- no health protocol unless a specimen needs it;
- no TLS ALPN unless a prior/follow-up HTTP/2 TLS phase lands;
- no generated build-system miracle;
- no hidden Tokio, hyper, tonic, or h2 runtime;
- no unbounded message queues;
- no retry/reconnect framework in the streaming service layer.

## Rock 0: Read First And Freeze The Claim

Read:

- `.intent/phases/057-native-grpc-service-stack/plan.md`;
- `.intent/phases/095-http2-streaming-substrate/plan.md`;
- `tina-http/src/grpc.rs`;
- `tina-http/src/http2.rs`;
- `tina-http/tests/grpc_live.rs`;
- new HTTP/2 streaming tests from 095;
- `examples/specimen_grpc_counter`;
- `docs/tina-user-guide/12-io-model.md`;
- `docs/tina-user-guide/18-bridge-crates.md`.

Before coding, edit this plan with:

- exact public streaming service trait/helper names;
- exact stream source/sink types reused from HTTP/2;
- exact deadline/cancel ownership model;
- exact client shape if implemented here;
- exact interop targets: grpcurl, tonic client, tonic server, or deferred;
- exact specimen shape.

Cut line:

- If bidirectional streaming reveals an HTTP/2 substrate bug, stop and fix 095
  substrate rather than layering around it.
- If interop requires TLS ALPN, keep h2c interop separate and record TLS ALPN
  as a future phase.
- If reflection grows, defer reflection; do not hold streaming hostage to it.

## Rock 1: Service Shapes

Add Tina-shaped service helpers/templates for all streaming modes.

Candidate vocabulary:

- `GrpcUnaryService` or existing `GrpcRouter::unary`;
- `GrpcServerStreamingService` / `GrpcRouter::server_streaming`;
- `GrpcClientStreamingService` / `GrpcRouter::client_streaming`;
- `GrpcBidiStreamingService` / `GrpcRouter::bidi_streaming`;
- `GrpcRequestStream<T>`;
- `GrpcResponseSink<T>`;
- `GrpcStreamOutcome`;
- `GrpcStreamReport`.

Rules:

- generated/manual templates are Tina-shaped, not tonic-shaped;
- users do not hand-build HTTP/2 DATA frames or trailers;
- user handlers see typed protobuf messages or explicit raw bytes;
- each stream has bounded message count/bytes/in-flight policy;
- final status is explicit;
- service code can distinguish peer cancel, deadline, malformed message,
  source/sink full, and internal error.

Do not require build.rs/protoc codegen in this phase. Hand-written
`prost::Message` test types are still enough unless interop demands a fixture
proto.

## Rock 2: Server-Streaming

Implement:

```text
one request message -> many response messages -> final trailers
```

Requirements:

- decode exactly one request message;
- reject zero/two request messages as unary/server-streaming protocol errors;
- response messages are gRPC-framed DATA over the 095 HTTP/2 response stream;
- response source/sink obeys HTTP/2 connection and stream windows;
- slow peer pressure is visible;
- peer reset cancels the response source;
- source error maps to typed `GrpcStatus`;
- final trailers are sent after the last message.

## Rock 3: Client-Streaming

Implement:

```text
many request messages -> one response message -> final trailers
```

Requirements:

- request messages are decoded incrementally from the 095 HTTP/2 request
  stream;
- per-message cap fires before allocating the protobuf body;
- malformed frame maps to `InvalidArgument`;
- request message too large maps to `ResourceExhausted`;
- service can finish with one response plus status;
- service can reject early and cancel/drain the remaining request stream
  explicitly;
- peer reset cancels the request stream and accepted service call;
- timeout/deadline maps to `DeadlineExceeded`.

## Rock 4: Bidirectional Streaming

Implement full-duplex gRPC streaming only on top of the 095 substrate.

Requirements:

- request and response streams make independent progress;
- one TCP reader and one TCP writer remain owned by HTTP/2 connection;
- service owns explicit request stream and response sink handles;
- inbound and outbound message caps are separate;
- bounded in-flight request and response queues;
- response can finish before request EOF only with explicit policy;
- request EOF does not force response EOF unless the service says so;
- peer reset/cancel reaches both request stream and response sink;
- final status trailers are sent exactly once when the response side finishes.

If this starts inventing a new scheduler, stop. The service should still be a
Tina state machine with visible calls/effects.

## Rock 5: Client Shape

Decide whether this phase includes a production native gRPC client or only
server-side streaming.

If included, it must build on a real HTTP/2 client state machine, not the Phase
057 blocking h2c helper.

Client requirements:

- pooled or single-connection shape is explicit;
- stream id allocation is bounded and typed;
- concurrent stream cap is honored;
- response DATA/trailers parsed incrementally;
- request streaming uses bounded outbound DATA;
- deadlines/cancel send `RST_STREAM` where possible;
- connection retire/reconnect is explicit;
- pressure reports name stream table full, outbound queue full, window blocked,
  and peer reset.

If the HTTP/2 client state machine does not exist yet, defer production gRPC
client and add/point to the required HTTP/2 client phase.

## Rock 6: Interop

Interop tests are required for any interop claim.

Minimum h2c interop targets if tooling supports them:

- tonic client -> Tina unary/server-streaming/client-streaming/bidi server for
  the modes shipped;
- grpcurl with explicit proto/descriptor -> Tina server for unary and
  server-streaming at minimum.

If grpcurl requires reflection for the desired command, use explicit proto or
descriptor first. Reflection is not required for this phase unless deliberately
scoped.

If tonic server interop requires a native production client that is not ready,
defer tonic-server interop and say so.

## Rock 7: Specimen

Add or extend a specimen.

Good shape:

- counter/key-value service;
- unary `Get` / `Increment`;
- server-streaming `Watch`;
- client-streaming `Sum`;
- bidi `Chat` or `Pipe`;
- README names h2c, caps, no compression, no tonic runtime, and interop status.

Specimen must have smoke tests for every shipped streaming mode.

## Rock 8: Tests

Required common tests:

- status trailers arrive once and last;
- `grpc-message` percent encoding/decoding;
- compression rejected;
- content-type variants accepted/rejected correctly;
- per-message caps before allocation;
- per-stream caps/queue pressure;
- timeout/deadline maps to `DeadlineExceeded`;
- peer reset maps to cancel truth;
- late source/sink replies visible.

Required server-streaming tests:

- multiple messages arrive in order;
- slow peer/backpressure visible;
- peer reset cancels source;
- source error maps to status;
- final status trailers sent after last message.

Required client-streaming tests:

- multiple request messages delivered in order;
- zero-message stream behavior pinned;
- malformed request frame maps to status;
- oversized request message maps to status;
- service early error releases/cancels request stream.

Required bidirectional tests:

- request and response messages interleave;
- response can continue after request EOF when service policy allows;
- peer reset cancels both sides;
- outbound pressure does not block inbound consumption forever;
- final status sent exactly once.

Regression:

- `cargo test -p tina-http grpc --tests`
- `cargo test -p tina-http http2 --tests`
- 095 streaming substrate tests continue to pass.

## Docs

Update:

- `docs/tina-user-guide/12-io-model.md`;
- `docs/tina-user-guide/18-bridge-crates.md`;
- `tina-http` crate docs;
- gRPC specimen README;
- Phase 057 status if still present as the unary slice.

Docs must say:

- which gRPC modes ship;
- h2c vs TLS ALPN status;
- compression status;
- interop status;
- where caps live;
- how deadlines/cancel/reset map;
- what remains deferred.

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina-http grpc --tests`
- `cargo test -p tina-http http2 --tests`
- `cargo clippy -p tina-http --tests -- -D warnings`
- specimen smoke tests
- interop commands/tests for any claimed interop
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`

## Done Means

- Tina gRPC supports unary plus every streaming mode claimed in this phase.
- Streaming modes reuse the 095 HTTP/2 streaming substrate.
- Status, caps, timeout, cancel, reset, and pressure are typed and tested.
- Interop claims are backed by actual commands/tests.
- Deferred items are named plainly.
