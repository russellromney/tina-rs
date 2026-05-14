# 096 Finish Native gRPC Streaming

## Status

- IDD phase.
- In implementation on PR #85.
- Shipped in this branch so far:
  - server-streaming route shape over native HTTP/2 streamed DATA and final
    gRPC trailers;
  - client-streaming route shape that receives multiple gRPC request messages
    over the HTTP/2 request pull path and returns one response;
  - unary route path preserved over the same HTTP/2 request pull path;
  - live tests for server-streaming messages/status and client-streaming
    multiple request messages;
  - hostile-review live tests for repeated server-streaming calls, mixed
    server/client-streaming modes on one HTTP/2 connection, request-trailer
    rejection, content-length overrun/underrun, and total body cap across
    consumed chunks;
  - typed finite server-streaming helper and many-small-message
    client-streaming proof.
  - hostile user-proof tests for non-reading server-streaming peer reset and
    oversized declared client-streaming messages failing before service code.
  - tonic h2c client interop against the specimen for unary, server-streaming,
    and client-streaming; this also forced real incoming HPACK decode.
  - hostile-review fixes for large tonic unary responses, request-sensitive
    server-streaming specimen output, tight-queue final trailer preservation,
    and explicit grpcurl proto/command ownership.
- Still deferred in this branch:
  - true service-level client-streaming handler API; the current client
    streaming route still accumulates decoded request messages for the handler,
    so 096 cannot honestly claim final client-streaming until this lands;
  - true bidirectional streaming with independent request/response lifecycles;
  - automated grpcurl interop in CI; the proto and manual commands are owned,
    but the local environment used for this PR does not ship `grpcurl`;
  - tonic h2c bidi interop;
  - reflection;
  - production pooled Tina gRPC client;
  - TLS ALPN.
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
server-side unary, server-streaming, client-streaming, and bidirectional gRPC
over native HTTP/2 h2c, with bounded messages, typed status, explicit
cancel/deadline truth, and no hidden Tokio runtime.
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
- production Tina gRPC client is claimed only if a real native HTTP/2 client
  state machine exists; otherwise client work is a separate follow-up.

## Non-Goals

- no tonic compatibility layer;
- no interceptor framework;
- no reflection unless grpcurl interop requires and it is deliberately scoped;
- no load balancing;
- no health protocol unless a specimen needs it;
- no TLS ALPN unless a prior/follow-up HTTP/2 TLS phase lands;
- no production Tina gRPC client unless the HTTP/2 client state machine already
  exists;
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
- exact bidirectional lifecycle policy: request EOF, response EOF, early
  service error, peer reset, local cancel, and final status ownership.
- exact service-level client-streaming lifecycle policy: request `Next`,
  request EOF, malformed frame, oversized frame, early service success, early
  service error, local cancel/drain/reset choice, peer reset, and final status
  ownership.
- exact `GrpcRequestStream<T>` decoder ownership: partial 5-byte gRPC header,
  current message byte buffer, decoded message handoff, and when HTTP/2 window
  credit is returned for each consumed byte.
- exact user-facing command proofs: specimen command, grpcurl command, tonic
  command or test, and what each proves.

Cut line:

- If bidirectional streaming reveals an HTTP/2 substrate bug, stop and fix 095
  substrate rather than layering around it.
- If true service-level client-streaming requires a new HTTP/2 request-source
  primitive, stop and fix 095 rather than buffering inside gRPC.
- Do not start bidirectional streaming until `GrpcRequestStream<T>` is real and
  proven with early reject, split-frame decode, and pending-read cancellation.
- If interop requires TLS ALPN, keep h2c interop separate and record TLS ALPN
  as a future phase.
- If reflection grows, defer reflection; do not hold streaming hostage to it.
- If production client work starts creating an HTTP/2 client state machine,
  split that to its own phase instead of smuggling it into gRPC streaming.

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

- request messages are decoded incrementally from the 095 HTTP/2 request stream
  and exposed to user code through `GrpcRequestStream<T>` or an equivalent
  Tina-shaped pull handle;
- keep the existing buffered helper only if renamed or documented as buffered,
  for example `client_streaming_buffered`, so the default honest API is not
  secretly `Vec<T>`;
- `GrpcRequestStream<T>::next` must preserve gRPC frame decoder state across
  HTTP/2 DATA chunk boundaries and never require the whole request body in
  memory;
- resident memory proof must be measurable: use a route-side high-water
  counter, body/stream metrics, or a tiny resident cap that fails if messages
  accumulate with total stream length;
- the only unbounded-by-count state allowed in the request decoder is the
  current message buffer up to `max_message_bytes` plus one bounded HTTP/2
  chunk; completed messages must be handed to the user or dropped before the
  next completed message is admitted;
- per-message cap fires before allocating the protobuf body;
- malformed frame maps to `InvalidArgument`;
- request message too large maps to `ResourceExhausted`;
- service can finish with one response plus status;
- service can reject early before request EOF and the implementation has a
  pinned policy: either drain with bounded credit or reset/cancel the HTTP/2
  request stream visibly;
- service success before request EOF has a pinned policy and does not leave a
  request-body pull stranded; tests must prove the peer sees exactly one final
  status and unread request DATA cannot grow without bound;
- peer reset cancels the request stream and accepted service call;
- timeout/deadline maps to `DeadlineExceeded`.
- user code can distinguish clean EOF, peer cancel, malformed frame,
  oversized message, source full/closed, and timeout.
- live tests prove:
  - a handler reads one message and returns an error while the client is still
    sending;
  - a handler reads one message and returns success while the client is still
    sending;
  - a handler sums many messages without resident memory growing with message
    count, proven by a high-water/cap assertion rather than just completion;
  - a protobuf frame split across multiple HTTP/2 DATA frames decodes once;
  - malformed and oversized messages fail before invoking the user handler for
    that message;
  - peer reset while `next` is pending wakes the handler with cancel truth.

## Rock 4: Bidirectional Streaming

Implement full-duplex gRPC streaming only on top of the 095 substrate.

Requirements:

- depends on Rock 3's `GrpcRequestStream<T>`; do not build a second inbound
  message-stream abstraction for bidi;
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

Default stance: this phase is server-side gRPC streaming. Production client
support is included only if the HTTP/2 client state machine has already shipped
or is already in the branch as an explicit prerequisite, not as a side effect of
gRPC streaming.

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

Interop commands must be checked in as tests, scripts, or documented specimen
commands. A manually remembered command is not an interop gate.

Any documented command must be copy-paste runnable from the repo root or from
the specimen directory it names. If a command needs a generated descriptor,
temporary port, fixture server, or environment variable, the script owns that
setup.

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
- externally documented specimen/interop commands run in CI or in a scripted
  smoke target, not only in a README.

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
- service error while request DATA is still arriving cancels/drains the request
  stream according to the pinned policy;
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

Current PR status: partially done. Unary, first server-streaming, and first
client-streaming are implemented over the native HTTP/2 h2c server. The phase
is not complete until bidirectional lifecycle policy, interop commands, and any
claimed client behavior are proven or explicitly split out.

- Tina gRPC supports unary plus every streaming mode claimed in this phase.
- Streaming modes reuse the 095 HTTP/2 streaming substrate.
- Status, caps, timeout, cancel, reset, and pressure are typed and tested.
- Interop claims are backed by actual commands/tests.
- Deferred items are named plainly.
