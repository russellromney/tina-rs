# 057 Native gRPC Service Stack

## Status

- Shipped in PR for `codex/phase-057-native-grpc-service-stack`.
- One PR.
- Builds on shipped native HTTP/2.
- Unary server shipped with `prost` payloads, typed gRPC status trailers,
  bounded message caps, timeout/status mappings, h2c client specimen helper,
  docs, and live tests.
- Server-streaming deferred: unary plus trailer/status support was the clean
  first form; streaming should reuse the HTTP body/source cancellation model in
  a later slice.
- Production Tina gRPC client service deferred: this PR ships a tiny blocking
  h2c helper for tests/specimens only, not a pooled runtime-owned client
  topology.
- Do not run beside 087 WebSocket unless `tina-http` file ownership is
  coordinated.
- First PR is server-first. Client support is intentionally tiny and honest:
  `grpc_unary_call_h2c_blocking` proves the native HTTP/2/protobuf/status path
  without Tokio, hyper, tonic, pooling, runtime trace facts, or hidden runtime
  ownership.

## Grug Truth

gRPC is HTTP/2 plus protobuf plus status.

Do not build tonic.

Do not hide HTTP/2 pressure.

Unary first.

Server-streaming second.

Client-streaming and bidirectional streaming later.

## Goal

Add a native Tina gRPC first form on top of `tina-http::Http2Listener`.

Use `prost` for protobuf bytes. Tina owns:

- HTTP/2 transport;
- service dispatch;
- message size caps;
- per-stream lifecycle;
- timeout/cancel/status truth;
- trace/DST facts.

First form supports:

- unary server;
- unary client only if it does not require a fake Tokio/hyper path;
- server-streaming response if unary is boring;
- typed `GrpcStatus`;
- message compression rejected unless explicitly unsupported;
- specimen showing Tokio/tonic shape vs Tina shape.

## Non-Goals

- no tonic compatibility layer;
- no generated build system miracle;
- no client-streaming in first PR unless unary/server-streaming are already
  boring;
- no bidirectional streaming;
- no interceptors;
- no load balancing;
- no reflection;
- no health protocol unless the specimen needs it;
- no hidden Tokio runtime.
- no TLS ALPN / `h2` negotiation in this slice unless HTTP/2 already exposes
  it cleanly; h2c prior-knowledge is acceptable first form and must be named.

## Rock 0: API Home

Put first form in `tina-http` unless implementation proves it needs its own
crate.

Likely files:

- `tina-http/src/grpc.rs`;
- re-export narrow public types from `tina-http/src/lib.rs`;
- tests in `tina-http/tests/grpc_*.rs`;
- specimen under `examples/specimen_grpc_counter` or similar.

If `tina-grpc` crate is chosen instead, explain why in this plan before
coding. Default is no new crate.

## Rock 1: Wire Vocabulary

Pin boring names:

- `GrpcRequest`;
- `GrpcResponse`;
- `GrpcStatus`;
- `GrpcStatusCode`;
- `GrpcError`;
- `GrpcLimits`;
- `GrpcUnaryService` helper shape or adapter;
- `GrpcServerStreamingService` only if implemented.

Wire rules:

- content type must be `application/grpc` or `application/grpc+proto`;
- first transport may be prior-knowledge h2c if TLS ALPN is not ready;
- path maps to service/method;
- request body is gRPC message framing: 1 compression byte + 4 byte length +
  protobuf bytes;
- compression flag `1` rejects as unsupported in first form;
- message length is capped before allocation;
- response trailers carry `grpc-status` and optional `grpc-message`;
- gRPC errors are trailers/status, not broad HTTP 500, when the stream is still
  healthy enough to send trailers;
- HTTP/2 reset/cancel maps to typed status/cancel truth.

Status mapping must be pinned before coding:

- unknown method -> `Unimplemented`;
- malformed frame -> `InvalidArgument` or a named protocol error;
- message too large -> `ResourceExhausted`;
- service timeout/deadline -> `DeadlineExceeded`;
- peer reset/cancel -> `Cancelled`;
- internal Tina bug -> `Internal`.

If the current HTTP/2 first form cannot send trailers, add the smallest trailer
support needed for gRPC and prove HTTP/2 still passes. Do not encode
`grpc-status` as a fake response body.

## Rock 2: Unary Server

Build the copied path:

```rust
GrpcRouter::new()
    .unary("/pkg.Counter/Get", |req| CounterMsg::Get(req))
```

Or a better Tina-shaped equivalent.

Requirements:

- decode exactly one request message;
- reject zero/multiple messages unless method says streaming;
- service receives typed protobuf payload through `prost::Message` or raw bytes
  plus explicit user decoder;
- service returns typed payload/status;
- encode one response message plus trailers;
- all decode/status/body caps are typed outcomes.

Do not require users to hand-build HTTP/2 trailers for common unary service.
Do not require a build.rs/codegen story in first form; hand-written
`prost::Message` test types are enough.

RequestContext rule:

- multi-turn gRPC services must carry caller authority explicitly, same as
  ordinary Tina request/reply;
- docs/specimen must not teach "reply context magically survives a DB call."

## Rock 3: Client Shape

Add unary client only if it stays small and reuses the HTTP/2 first form.

Copied path should be boring:

```rust
grpc_unary_call(target, "/pkg.Counter/Get", request, timeout)
    .reply(MyMsg::Returned)
```

If native HTTP/2 client support is too thin, defer client and say so clearly.
Do not fake it through Tokio.

## Rock 4: Server Streaming

If unary is done and clean, add server-streaming first form.

Rules:

- one request message;
- bounded response message stream;
- source isolate owns chunks/messages;
- slow peer pressure is visible;
- cancel from peer reaches source as typed cancel;
- final status/trailers are explicit.

Server-streaming must reuse the HTTP body/source cancellation truth where
possible. If it invents a second streaming model, stop and redesign.

If this grows, stop at unary and write the follow-up in the plan.

## Rock 5: Specimen

Add one specimen.

Good shape:

- counter or key/value service;
- unary `Get` / `Increment`;
- optional server-streaming `Watch`;
- Tokio side may use tonic if practical;
- Tina side uses native gRPC helper;
- README names the tradeoff: more explicit state, visible caps/status/cancel.

Specimen must have a smoke test.

## Rock 6: Tests

Required unary tests:

- happy unary request/response;
- unknown method -> `Unimplemented`;
- malformed gRPC frame rejects;
- compressed request rejects;
- request message too large rejects before big allocation;
- response message too large returns typed failure;
- zero messages rejects;
- two messages on unary rejects;
- service timeout/cancel maps to typed status;
- HTTP/2 stream reset is visible;
- trailers include `grpc-status`;
- trailers include percent-safe `grpc-message` when set;
- content-type mismatch rejects;
- concurrent unary streams respect HTTP/2 stream cap.
- multi-turn unary service uses `RequestContext` correctly.

Required if server-streaming ships:

- multiple messages arrive in order;
- slow peer/backpressure is visible;
- peer reset cancels source;
- final status trailers sent after last message;
- source error maps to status.

Regression:

- `cargo test -p tina-http http2 --tests`.

## Docs

Update:

- `docs/tina-user-guide/18-bridge-crates.md` or protocol chapter;
- `docs/tina-user-guide/12-io-model.md`;
- specimen README.

Docs must say:

- Tina gRPC first form is native over Tina HTTP/2;
- first transport is h2c unless TLS ALPN was explicitly added;
- not tonic feature parity;
- no compression in first form;
- streaming support exactly as shipped;
- where caps live.

## Required Checks

- Passed: `cargo fmt --all --check`
- Passed: `cargo test -p tina-http grpc --tests`
- Passed: `cargo test -p tina-http http2 --tests`
- Passed: `cargo clippy -p tina-http --tests -- -D warnings`
- Passed: `cargo test --manifest-path examples/specimen_grpc_counter/Cargo.toml`
- Passed: `cargo run --manifest-path examples/specimen_grpc_counter/Cargo.toml`
- Passed: `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`

## Done Means

- A Tina service can serve at least unary gRPC over native HTTP/2.
- Status, caps, timeout, cancel, and malformed wire are typed.
- HTTP/2 pressure remains visible.
- Specimen proves copied shape.
- Docs clearly name what is not implemented.
