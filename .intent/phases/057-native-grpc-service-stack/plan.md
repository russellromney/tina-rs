# 057 Native gRPC Service Stack

## Status

- Ready to implement.
- One PR.
- Builds on shipped native HTTP/2.
- Do not run beside 087 WebSocket unless `tina-http` file ownership is
  coordinated.

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
- unary client if small enough;
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
- path maps to service/method;
- request body is gRPC message framing: 1 compression byte + 4 byte length +
  protobuf bytes;
- compression flag `1` rejects as unsupported in first form;
- message length is capped before allocation;
- response trailers carry `grpc-status` and optional `grpc-message`;
- HTTP/2 reset/cancel maps to typed status/cancel truth.

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
- service receives typed protobuf payload or raw bytes plus user decoder;
- service returns typed payload/status;
- encode one response message plus trailers;
- all decode/status/body caps are typed outcomes.

Do not require users to hand-build HTTP/2 trailers for common unary service.

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
- service timeout/cancel maps to typed status;
- HTTP/2 stream reset is visible;
- trailers include `grpc-status`;
- content-type mismatch rejects;
- concurrent unary streams respect HTTP/2 stream cap.

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
- not tonic feature parity;
- no compression in first form;
- streaming support exactly as shipped;
- where caps live.

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina-http grpc --tests`
- `cargo test -p tina-http http2 --tests`
- `cargo clippy -p tina-http --tests -- -D warnings`
- specimen smoke test
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` if docs/rustdoc
  changed

## Done Means

- A Tina service can serve at least unary gRPC over native HTTP/2.
- Status, caps, timeout, cancel, and malformed wire are typed.
- HTTP/2 pressure remains visible.
- Specimen proves copied shape.
- Docs clearly name what is not implemented.
