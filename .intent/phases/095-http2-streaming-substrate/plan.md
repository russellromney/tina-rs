# 095 HTTP/2 Streaming Substrate

## Status

- IDD phase.
- In implementation on PR #85.
- Shipped in this branch so far:
  - HTTP/2 response DATA streaming from Tina `ResponseChunkMsg` sources;
  - streamed response DATA obeys HTTP/2 frame splitting and flow-control
    windows;
  - streamed response EOF sends final DATA or trailing HEADERS;
  - peer reset cancels accepted service work and response sources;
  - HTTP/2 request body pull source for gRPC-dispatched streams;
  - unary gRPC now exercises the HTTP/2 request pull path when request HEADERS
    and DATA arrive separately.
  - hostile-review fixes for request-trailer rejection, content-length
    overrun/underrun, total request body cap across consumed chunks, and
    preserved connection report replies.
- Still deferred in this branch:
  - generic non-gRPC HTTP/2 request-stream opt-in API;
  - full request-trailer support; request trailers are not a compatibility
    claim yet;
  - complete full-duplex blocked-one-way proof matrix;
  - production HTTP/2 client state machine.
- One PR when implemented.
- Blocks honest gRPC server-streaming, client-streaming, bidirectional
  streaming, and broader HTTP/2 streaming interop claims. Production gRPC
  client behavior also needs a future HTTP/2 client state-machine phase.
- Builds on shipped Phase 056 HTTP/2 first form and Phase 057 unary gRPC.
- Owns `tina-http/src/http2.rs`, HTTP/2 streaming tests, and docs around
  HTTP/2/gRPC streaming dependencies.
- Do not run beside broad `tina-http` WebSocket, gRPC streaming, TLS ALPN, or
  HTTP/2 client work unless file ownership is coordinated.

## Grug Truth

HTTP/2 exists today, but it is buffered.

gRPC streaming cannot be real until HTTP/2 DATA streaming is real in both
directions.

Do not invent a gRPC-only streaming model.

DATA frames must obey windows.

Trailers must be real trailing HEADERS.

END_STREAM must mean exactly one thing at exactly one point.

Peer reset must cancel accepted work.

Slow peers are pressure, not memory.

If the stream source is not cancelled on reset, the feature lies.

## Goal

Move `tina-http` HTTP/2 from buffered unary service transport to a streaming
substrate that can honestly carry:

```text
headers -> DATA... -> trailers
```

The claim this phase must make:

- server-side h2c HTTP/2 can stream response bodies over multiple DATA frames;
- server-side h2c HTTP/2 can expose request bodies incrementally to services;
- flow-control windows gate inbound and outbound DATA;
- bounded queues/bytes are visible as typed outcomes;
- peer `RST_STREAM` cancels request body sources, response body sources, and
  accepted service calls;
- final trailers are explicit and sent after the last DATA frame;
- HTTP/2 `END_STREAM` semantics are pinned for request DATA, response DATA, and
  trailing HEADERS;
- existing buffered HTTP/2 and unary gRPC tests still pass.

This phase is a substrate phase, not a gRPC streaming phase. It should leave
all gRPC streaming modes as the next layer over the substrate.

Dependency map:

- gRPC server-streaming needs HTTP/2 response DATA streaming plus final
  trailers.
- gRPC client-streaming needs HTTP/2 request DATA streaming plus final response
  trailers.
- gRPC bidirectional streaming needs both request and response streaming, plus a
  clear full-duplex ownership model. This phase should provide the HTTP/2
  substrate for that model on the server side.
- production pooled gRPC client needs HTTP/2 response/request streaming where
  relevant, but also a separate native HTTP/2 client connection state machine:
  stream id allocation, settings, concurrent streams, reset/cancel, reconnect,
  pooling, and pressure reports.
- tonic/grpcurl interop needs the shipped mode to be tested with those clients;
  this phase improves the substrate but does not itself claim interop.

## Non-Goals

- no gRPC server-streaming/client-streaming/bidi implementation in this phase;
- no production pooled HTTP/2 client, but do not choose server APIs that make a
  later HTTP/2 client impossible;
- no production pooled gRPC client;
- no TLS ALPN / HTTPS/2 unless it falls out as a tiny no-risk hook, which is
  unlikely;
- no dynamic HPACK table;
- no priority scheduling;
- no PUSH_PROMISE;
- no HTTP/2 WebSocket;
- no tonic compatibility claim;
- no broad RFC-complete HTTP/2 rewrite;
- no unbounded stream queues;
- no hidden Tokio, hyper, h2, or tonic runtime.

## Rock 0: Read First And Freeze The Claim

Read:

- `.intent/phases/056-native-http2-service-stack/plan.md`;
- `.intent/phases/057-native-grpc-service-stack/plan.md`;
- `tina-http/src/http2.rs`;
- `tina-http/src/streaming.rs`;
- `tina-http/src/body_metrics.rs`;
- `tina-http/tests/http2_live.rs`;
- `tina-http/tests/grpc_live.rs`;
- `docs/tina-user-guide/12-io-model.md`;
- `docs/tina-user-guide/18-bridge-crates.md`;
- HTTP/1 response/request streaming tests for cancellation and pressure shape.

Before coding, edit this plan with:

- exact public service-facing request-stream type, if any;
- exact response-stream type reuse or adapter choice;
- exact full-duplex request/response ownership model;
- exact `END_STREAM` state machine for HEADERS, DATA, and trailing HEADERS in
  both directions;
- exact request-trailer support/rejection policy;
- exact trailer API shape;
- exact caps and default values;
- exact test list shipped/deferred.
- exact downstream gRPC modes unblocked by the completed substrate.

Emergency cut line:

- If trailers after streaming DATA grow, stop; all gRPC streaming remains
  blocked.
- If cancellation requires a second writer or second owner of the TCP stream,
  stop and redesign.
- If request streaming truly cannot land, do not call this phase done. Rename
  the partial work to a response-streaming phase and leave 095 open or replaced.

## Rock 1: Response DATA Streaming

Teach HTTP/2 to stream a response body from a bounded source instead of cloning
one buffered `Vec<u8>`.

Required shape:

- one connection isolate remains the only TCP writer;
- service returns a response variant or adapter that names a body source;
- connection sends response HEADERS first;
- connection pulls chunks/messages from the source with a timeout;
- each chunk is split into DATA frames no larger than `max_frame_size`;
- DATA emission stops when connection or stream window is exhausted;
- `WINDOW_UPDATE` resumes pending DATA without allocating an unbounded body;
- EOF sends final DATA with `END_STREAM` only if there are no trailers;
- if trailers exist, final DATA does not end the stream and trailing HEADERS
  does;
- after response `END_STREAM`, later DATA/HEADERS for that stream are rejected
  or reset visibly;
- source error maps to a typed stream failure and, when possible, trailers or
  reset;
- peer `RST_STREAM`, connection close, or owner stop sends source cancel.

Reuse `ResponseChunkMsg` / `ResponseChunkReply` if it fits. If it does not fit,
explain why before adding another streaming vocabulary.

Do not buffer the whole response in the HTTP/2 connection to reuse the old
unary path. That is not streaming.

## Rock 2: Request DATA Streaming

Expose inbound request DATA incrementally to services.

Required shape:

- service can receive headers before the full body is buffered;
- service pulls request chunks through a bounded source, or receives a clearly
  bounded stream handle;
- connection returns stream/connection window credit only after chunks are
  consumed or discarded;
- declared `content-length`, if present, is enforced;
- end-of-stream is distinct from truncation/reset/protocol error;
- request trailers are either supported as real trailing HEADERS or rejected
  with a pinned typed protocol outcome; do not silently treat them as body;
- request DATA after request `END_STREAM` is rejected/reset visibly;
- body/message caps fire before large allocation;
- service timeout/cancel closes the pull wait and releases buffered chunks;
- peer reset cancels the request stream source and accepted service call;
- buffered request body path remains available for small/unary services.

Do not defer this from 095 just because it is larger than response streaming.
Client-streaming and bidi need this, and the next gRPC phase should not have to
change the HTTP/2 substrate again.

## Rock 3: Trailer API

Make trailers explicit enough that gRPC can reuse them without hand-building
HTTP/2 frames.

Requirements:

- response can carry trailing headers separately from initial headers;
- trailing headers are sent only after all DATA;
- `grpc-status` / `grpc-message` stay trailers, not initial headers and not
  body bytes;
- ordinary HTTP/2 services can use trailers without depending on gRPC types;
- invalid pseudo-headers in trailers reject or reset cleanly;
- trailer header bytes count against a cap;
- trailers must not bypass outbound queue limits.

This can be a narrow internal API if public naming is not ready, but it must be
real enough for gRPC server-streaming in the next phase.

## Rock 4: Flow Control And Pressure

Streaming must prove that windows and caps are doing work.

Required outcomes/counters, names may differ but distinctions must remain:

- stream window blocked;
- connection window blocked;
- outbound queue full;
- outbound bytes full, if byte caps exist;
- source call full/closed/timeout;
- source cancelled on peer reset;
- request stream full;
- request stream cancelled on service timeout/reset;
- late source reply after reset/close visible in trace or report.
- full-duplex progress while one direction is flow-control blocked.

Hard rules:

- no `Vec` growth proportional to an unbounded response;
- no one queued frame per unbounded source chunk without a queue cap;
- no `usize::MAX` caps;
- no separate gRPC pressure model;
- no hidden retry on blocked windows.
- no deadlock where outbound window exhaustion prevents inbound DATA
  consumption or reset handling.

## Rock 5: Tests

Required response-streaming tests:

- streamed response sends multiple DATA frames in order;
- final trailers arrive after the last DATA frame;
- large chunks split by `max_frame_size`;
- connection window blocks DATA until `WINDOW_UPDATE`;
- stream window blocks DATA until stream `WINDOW_UPDATE`;
- slow peer does not grow unbounded pending body;
- peer `RST_STREAM` cancels response source;
- source error maps to typed failure/reset/trailers;
- source timeout maps visibly;
- DATA/HEADERS after response `END_STREAM` rejects/resets visibly;
- existing buffered HTTP/2 responses still pass;
- unary gRPC trailers still pass.

Required request-streaming tests:

- service receives chunks before full body arrives;
- two chunks arrive in order;
- request window credit returns only after consumption;
- oversized body rejects before large allocation;
- truncated body is distinct from clean EOF;
- request trailers are supported or rejected exactly as documented;
- DATA/HEADERS after request `END_STREAM` rejects/resets visibly;
- peer reset cancels request source and accepted service call;
- service timeout/cancel releases buffered request chunks;
- buffered request path still passes.

Required full-duplex substrate tests:

- service can consume inbound request DATA while outbound response DATA is
  blocked by stream window;
- peer `RST_STREAM` is processed while outbound response DATA is blocked;
- request EOF does not implicitly close response streaming;
- response completion does not leak unread request chunks.

Required regression:

- `cargo test -p tina-http http2 --tests`
- `cargo test -p tina-http grpc --tests`
- HTTP/1 body streaming tests continue to pass if shared streaming types are
  touched.

## Rock 6: Docs

Update:

- `docs/tina-user-guide/12-io-model.md`;
- `docs/tina-user-guide/18-bridge-crates.md`;
- `tina-http` crate docs;
- Phase 057 plan status if gRPC streaming remains blocked or becomes unblocked.

Docs must say:

- HTTP/2 first form has become server-side request/response streaming-capable;
- first transport remains h2c unless TLS ALPN separately lands;
- which paths are buffered vs streamed;
- where caps live;
- how resets/cancel/source cleanup work;
- gRPC streaming is next, not part of this phase.

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina-http http2 --tests`
- `cargo test -p tina-http grpc --tests`
- `cargo clippy -p tina-http --tests -- -D warnings`
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` if public docs
  change

## Done Means

Current PR status: partially done. The response-streaming substrate and gRPC
request pull path are implemented and tested; the whole 095 done bar remains
open until generic HTTP/2 request-streaming policy and full-duplex pressure
proofs land.

- HTTP/2 can stream at least response DATA from a bounded source with real flow
  control, trailers, reset cancellation, and pressure reports/tests.
- HTTP/2 can expose request DATA incrementally with real flow control, EOF vs
  truncation truth, reset cancellation, and bounded-buffer proof.
- gRPC server-streaming, client-streaming, and bidirectional streaming remain
  blocked only by the gRPC layer and future client work, not by missing
  server-side HTTP/2 DATA/trailer substrate.
- Roadmap clearly shows gRPC streaming stacks on this phase.
