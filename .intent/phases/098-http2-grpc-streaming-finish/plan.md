# 098 HTTP/2 And gRPC Streaming Finish

## Status

- IDD phase.
- One PR unless interop tooling forces a tiny follow-up.
- Builds on shipped Phase 095 HTTP/2 streaming substrate and Phase 096 native
  gRPC first streaming modes.
- Owns remaining HTTP/2/gRPC streaming truth: bidirectional gRPC, fuller
  full-duplex HTTP/2 proof, grpcurl/tonic interop where practical, and clear
  client/TLS deferrals.
- Do not run beside broad `tina-http` protocol rewrites without file ownership.
- Current shipped HTTP/2 facts at start of Phase 098:
  - `Http2Listener` is server-side prior-knowledge h2c only;
  - response DATA can stream from `ResponseChunkMsg` chunk sources, is split by
    `max_frame_size`, obeys connection/stream send windows, and resumes on
    `WINDOW_UPDATE`;
  - gRPC requests are exposed early as `Http2RequestStream` pull sources, with
    window credit returned only as chunks are delivered;
  - peer `RST_STREAM` removes the stream, replies error to a pending request
    pull, cancels the accepted service call, and sends `ResponseChunkMsg::Cancel`
    to a response source;
  - request trailers are rejected/reset, not supported as a compatibility
    claim;
  - content-length overrun/underrun and body caps are pinned by live tests;
  - missing at start: explicit full-duplex proof while outbound response DATA is
    blocked and inbound DATA on the same connection continues.
- Current shipped gRPC facts at start of Phase 098:
  - public router names: `GrpcRouter::unary`, `GrpcRouter::server_streaming`,
    and `GrpcRouter::client_streaming`;
  - server-streaming returns `GrpcServerStreamingResponse` with a Tina
    `ResponseChunkMsg` source and final status in response trailers;
  - client-streaming currently buffers all pulled HTTP/2 request chunks inside
    `GrpcRouter` before invoking the user handler;
  - tonic h2c interop exists in the specimen for unary, server-streaming, and
    client-streaming with tonic `0.12`;
  - missing at start: bidirectional public API and tonic h2c bidi proof.
- Phase 098 target API names:
  - add `GrpcRouter::streaming` for bidirectional streaming RPCs;
  - add `GrpcStreamingCall<Req, Resp>`, `GrpcRequestStream<Req>`,
    `GrpcStreamReply<Req>`, and `GrpcStreamingResponse<Resp>`;
  - add `GrpcRouter::streaming_raw`, `GrpcRawStreamingRequest<T>`, and
    `GrpcRawStreamingResponse` as the advanced escape hatch.
- Request/response ownership model for bidi:
  - HTTP/2 owns the socket, frame parsing, stream table, and flow-control
    windows;
  - the Tina service receives an explicit `GrpcRequestStream<Req>` handle
    through `GrpcStreamingCall` and pulls request messages only when ready;
  - the response source is still a Tina `ResponseChunkMsg` source, but ordinary
    service code can use `grpc_stream_message` and `grpc_stream_finish` instead
    of hand-building gRPC DATA and trailers;
  - `streaming_raw` can still expose `Http2RequestStream` and already-framed
    response bytes for protocol adapters/tests; no Tokio/hyper/h2 runtime is
    hidden.
- Final-status ownership rule:
  - successful route construction owns exactly one final status, encoded as
    HTTP/2 trailers on the returned `HttpResponse`;
  - streaming sources may finish with `ResponseChunkReply::GrpcStatus` when the
    final gRPC status is discovered while processing request DATA;
  - peer reset/cancel does not send gRPC trailers after reset; HTTP/2 cancels
    pending request pulls, accepted service work, and response sources;
  - service construction errors map once to typed `GrpcStatus` trailers.
- Interop targets:
  - required: tonic `0.12` h2c client to Tina server for unary,
    server-streaming, client-streaming, and bidi in
    `examples/specimen_grpc_counter`;
  - grpcurl remains best-effort and is deferred if reflection/descriptor
    plumbing grows beyond a small command fixture.
- Deferrals remain exact:
  - no production pooled Tina gRPC client;
  - no TLS ALPN / HTTPS/2;
  - no reflection unless deliberately scoped;
  - no compression.
- Completed in this PR:
  - added `GrpcRouter::streaming`, `GrpcStreamingCall<Req, Resp>`,
    `GrpcRequestStream<Req>`, `GrpcStreamReply<Req>`, and
    `GrpcStreamingResponse<Resp>`;
  - added `GrpcRouter::streaming_raw`, `GrpcRawStreamingRequest<T>`, and
    `GrpcRawStreamingResponse` for the raw substrate escape hatch;
  - added `ResponseChunkMsg::Http2RequestChunk` so a Tina response source can
    pull an HTTP/2 request stream and receive the continuation through the same
    bounded source protocol, without a hidden scheduler/runtime;
  - added `ResponseChunkReply::GrpcStatus` so bidi sources can end with
    non-`OK` gRPC status discovered mid-stream instead of lying with
    precomputed `OK` trailers;
  - added `grpc_stream_message` and `grpc_stream_finish` so user code does not
    hand-build gRPC frame bytes or trailer maps on the typed path;
  - proved stream-window blocked outbound DATA on one stream does not prevent
    an unrelated stream from completing when connection credit is available;
  - proved inbound request DATA can still be consumed while another response
    stream is blocked on its stream window;
  - proved bidi gRPC can send a response before request EOF;
  - proved concurrent bidi streams do not cross-talk;
  - proved the `streaming_raw` escape hatch can send a response before request
    EOF and still finish with typed gRPC status trailers;
  - proved malformed/compressed bidi request frames and declared oversized bidi
    messages set final gRPC status;
  - proved peer reset cancels a bidi response source;
  - extended the specimen tonic h2c interop test to unary, server-streaming,
    client-streaming, sequential bidirectional streaming, and concurrent
    bidirectional clients with tonic `0.12.3`;
  - fixed the existing deprecated `IsolateCall::reply` use in
    `tina-http/src/websocket.rs` because the required clippy command treats it
    as an error.
- Hostile self-review:
  - finding fixed: `bidi_streaming` naming was too implementation-shaped and
    the happy path was too raw; the primary API is now `streaming`, while
    `streaming_raw` holds the advanced HTTP/2 escape hatch;
  - finding fixed: late bidi parse/cap failures could previously inherit `OK`
    trailers; sources can now return explicit final trailers and tests prove
    non-`OK` status for hostile request frames;
  - finding fixed: the specimen/test route originally used one bidi source for
    the whole server; it now allocates from a bounded per-call source pool and
    tests concurrent tonic/native bidi clients;
  - finding fixed: `GrpcRequestStream` and `GrpcStreamingCall` were cloneable
    even though the request stream owns decoder state; the typed stream handle
    is now single-owner and examples borrow it only to construct pull effects;
  - finding fixed: the response chunk API exposed generic-looking trailers but
    only serialized gRPC status; streaming gRPC sources now finish with
    `ResponseChunkReply::GrpcStatus`, keeping final status typed and exact;
  - finding fixed: `streaming_raw` was public but not exercised; a live raw
    streaming test now proves response-before-request-EOF plus final status;
  - finding fixed: the specimen's source-pool capacity was implicit route
    plumbing; it is now a named pool with a visible `ResourceExhausted` message
    and README note;
  - finding: grpcurl was not added because reflection/descriptor plumbing would
    be a separate compatibility surface; docs defer it explicitly;
  - finding: no production Tina gRPC client or TLS ALPN was touched.
- Required check results:
  - `cargo fmt --all --check` passed;
  - `cargo test -p tina-http --test http2_live -- --nocapture` passed;
  - `cargo test -p tina-http --test grpc_live -- --nocapture` passed;
  - `cargo test --manifest-path examples/specimen_grpc_counter/Cargo.toml`
    passed, including tonic `0.12.3` h2c interop for all four modes;
  - `cargo test -p tina-http` passed;
  - `cargo clippy -p tina-http --tests -- -D warnings` passed;
  - `RUSTDOCFLAGS="-D warnings" cargo doc -p tina-http --no-deps` passed.

## Grug Truth

HTTP/2 streaming mostly exists.

gRPC unary, server-streaming, and client-streaming mostly exist.

The remaining gap is the hard one:

- both directions active;
- one peer stalls;
- one peer resets;
- service finishes one side first;
- final status still happens once;
- no hidden queue grows.

Do not paper over this with a gRPC-only shortcut.

HTTP/2 owns bytes and windows.

gRPC owns protobuf frames and status.

Tina owns capacity, cancellation, and lifecycle truth.

## Goal

After this phase, Tina can honestly claim:

```text
native server-side gRPC unary, server-streaming, client-streaming, and
bidirectional streaming over Tina-owned HTTP/2 h2c, with bounded messages,
visible pressure, peer reset cancellation, and interop proof for claimed modes.
```

This is server-side readiness. A production Tina gRPC client is a separate
phase unless the needed HTTP/2 client state machine already exists.

## Non-Goals

- no tonic runtime inside Tina;
- no hyper/h2 async runtime inside Tina;
- no production pooled gRPC client in this phase;
- no TLS ALPN unless it is already tiny and clearly separate;
- no reflection unless grpcurl proof requires a very small descriptor path;
- no compression support unless already implemented and tested;
- no load balancing;
- no retry/reconnect framework;
- no unbounded request/response queues;
- no hidden automatic retry on flow-control pressure.

## Rock 0: Read First, Freeze The Claim

Read:

- `.intent/phases/095-http2-streaming-substrate/plan.md`;
- `.intent/phases/096-finish-native-grpc-streaming/plan.md`;
- `tina-http/src/http2.rs`;
- `tina-http/src/grpc.rs`;
- `tina-http/tests/http2_live.rs`;
- `tina-http/tests/grpc_live.rs`;
- `examples/specimen_grpc_counter`;
- `docs/tina-user-guide/12-io-model.md`;
- `docs/tina-user-guide/18-bridge-crates.md`.

Before coding, update this status with:

- current shipped HTTP/2 streaming facts;
- current shipped gRPC modes;
- exact bidi public API names;
- exact request/response stream ownership model;
- exact final-status ownership rule;
- exact interop targets;
- exact deferrals.

Cut line:

- if HTTP/2 cannot make both directions progress independently, stop and fix
  HTTP/2 before adding more gRPC surface;
- if bidi needs a new scheduler, stop;
- if client work starts growing a real HTTP/2 client state machine, split it.

## Rock 1: HTTP/2 Full-Duplex Proof

Prove the substrate before layering gRPC bidi.

Required behavior:

- inbound DATA can be consumed while outbound DATA is flow-control blocked;
- outbound DATA can resume after `WINDOW_UPDATE`;
- peer `RST_STREAM` cancels accepted service work and response source;
- request EOF does not force response EOF unless the service says so;
- response EOF does not require request EOF unless the route says so;
- malformed DATA/trailer order resets the stream without killing unrelated
  streams;
- connection report still works after stream reset/cancel.

Required tests:

- one stream response blocked, another stream completes;
- inbound request chunks continue while outbound side is blocked;
- reset during blocked response cancels source;
- reset during request streaming cancels service call;
- content-length overrun/underrun remains pinned;
- trailers-after-end and DATA-after-end are rejected/reset visibly.

No sleeps as proof. Use barriers, socket deadlines, reports, or trace facts.

## Rock 2: Bidi gRPC API

Add the smallest Tina-shaped bidi surface.

Candidate shape:

```rust
router.streaming(path, |call| { ... })
```

The service must see explicit handles, not an async stream illusion:

- request stream handle/source;
- response sink/source;
- final status owner;
- per-message caps;
- per-stream caps;
- cancel/deadline outcome.

Rules:

- request and response lifecycles are independent;
- final gRPC status is sent once;
- service can finish response before request EOF only by explicit policy;
- request EOF does not auto-finish response;
- peer reset cancels request and response work;
- service error maps to typed `GrpcStatus`;
- no hidden per-message `Vec` grows without a cap.

If the API wants too many clever types, use one explicit state-machine specimen
first and extract only the dull names.

## Rock 3: Bidi gRPC Semantics

Implement full-duplex server-side gRPC over the HTTP/2 substrate.

Required proof:

- echo bidi: client sends N messages, server sends N messages;
- server sends before request EOF;
- client stops sending while server continues, if policy allows;
- server ends early with status and cancels/drains remaining request stream;
- peer reset cancels both sides;
- request message too large fails before user service sees decoded message;
- response message too large returns `ResourceExhausted`;
- malformed gRPC frame returns `InvalidArgument`;
- deadline maps to `DeadlineExceeded`;
- unrelated concurrent stream survives reset/failure.

Keep unary, server-streaming, and client-streaming tests green.

## Rock 4: Interop

Interop is required for any compatibility claim.

Minimum target:

- tonic h2c client -> Tina unary/server-streaming/client-streaming/bidi server
  for shipped modes.

Try grpcurl too:

- if grpcurl works without reflection, add a script/test command;
- if it needs reflection or descriptor plumbing, record exact reason and defer
  reflection.

Docs must say exactly what was tested:

- h2c or TLS;
- tonic version if pinned;
- grpcurl command if present;
- modes covered.

Do not claim broad gRPC ecosystem replacement without these tests.

## Rock 5: Client And TLS Deferral

Write down the line:

- server-side h2c streaming is in scope;
- production pooled Tina gRPC client is out of scope unless a native HTTP/2
  client state machine already exists;
- TLS ALPN / h2 over HTTPS is out of scope unless landed intentionally;
- tonic client interop can still prove server behavior.

If a tiny client helper exists only for tests, label it test-only. Do not let a
blocking helper pretend to be the production client.

## Rock 6: Docs And Specimen

Update:

- `docs/tina-user-guide/12-io-model.md`;
- `docs/tina-user-guide/18-bridge-crates.md` if bridge wording mentions gRPC;
- `examples/specimen_grpc_counter/README.md`;
- phase status.

Specimen must show:

- unary;
- server-streaming;
- client-streaming;
- bidi;
- one pressure/cancel case.

## Required Checks

Run at least:

```text
cargo fmt --all --check
cargo test -p tina-http --test http2_live -- --nocapture
cargo test -p tina-http --test grpc_live -- --nocapture
cargo test --manifest-path examples/specimen_grpc_counter/Cargo.toml
cargo clippy -p tina-http --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p tina-http --no-deps
```

If tonic/grpcurl tests are added, include exact commands in the status block.

## Success

No gRPC streaming mode depends on hidden buffering.

Peer reset and deadline reach the Tina service.

Flow-control pressure is visible.

Interop claims are backed by commands.

Production client and TLS ALPN are honestly deferred unless actually shipped.
