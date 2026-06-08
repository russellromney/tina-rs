# Phase 154: gRPC Hot Path

Status: implemented in this PR. Evidence: `perf_macos_after.txt`.

## Goal

Make warmed native gRPC stop paying generic HTTP/2 request/header/body setup
where no policy boundary is crossed.

This is not a harness phase. It changes protocol code.

## Shipped

1. Compact unary submit

   - Add `Http2ClientMsg::SubmitGrpcUnary`.
   - Emit fixed gRPC request headers directly:
     `POST`, scheme, path, authority, `content-type`, `te`.
   - Keep the same HTTP/2 admission checks: closed, stream-id exhausted,
     max concurrent streams, peer cap, outbound queue cap, header size cap.

2. Reusable unary builders

   - `GrpcClient::unary_request` now uses compact submit.
   - `GrpcClient::unary_template(path)` validates and shares the method path.
   - `GrpcUnaryTemplate::preframed(&msg)` returns `GrpcPreframedUnary`.
   - `GrpcPreframedUnary::request()` reuses a shared framed body.

3. Shared outbound request bytes

   - HTTP/2 client request bodies are `Owned` or `Shared`.
   - Shared bodies are sliced directly into DATA frames.
   - Streaming request chunks that append to a shared body first become owned.

4. Buffered finite server-streaming

   - Add `GrpcRouter::server_streaming_buffered`.
   - Add `GrpcBufferedServerStreamingResponse`.
   - Add `GrpcBufferedStreamLimits` with service-owned message-count and
     framed-body byte caps.
   - Small fixed streams are framed once and returned as a shared buffered body.
   - Request-sized or unbounded streams are not accepted by this helper; use
     source-backed streaming instead.
   - Existing source-backed server-streaming remains for real streaming.

5. Shared buffered response bodies

   - Add `HttpResponseBody::Shared(Arc<[u8]>)`.
   - Treat it as a known-length buffered body.
   - HTTP/2 keeps it shared until final DATA framing.
   - HTTP/1 accepts it too; large HTTP/1 shared bodies degrade to owned staging.

6. Perf specimen

   - Warmed gRPC unary rows use `GrpcPreframedUnary`.
   - Pooled gRPC unary rows use one preframed request per client.
   - Server-streaming row uses buffered finite streaming instead of a source
     pool.

## Proof

Run:

```sh
cargo test -p tina-http grpc_client::tests:: -- --nocapture
cargo test -p tina-http grpc_buffered -- --nocapture
cargo test -p tina-http buffered_server_streaming_bound_failure_returns_resource_exhausted -- --nocapture
cargo test -p tina-http buffered_server_streaming_receives_all_messages_then_status -- --nocapture
cargo test -p tina-http grpc -- --nocapture
cargo fmt --all --check
cargo clippy -p tina-http --all-targets -- -D warnings
cargo clippy --manifest-path examples/systems/perf_native/Cargo.toml --all-targets -- -D warnings
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf native_protocol_rows_are_printable_and_bounded -- --nocapture
```

Final local macOS/aarch64 release sample:

| row | p50 | p90 | load allocations / 32 ops |
| --- | ---: | ---: | ---: |
| `grpc_h2c_unary_close` | 1060 us | 1191 us | 608 |
| `grpc_h2c_unary_warmed` | 1023 us | 1166 us | 56 |
| `grpc_h2c_unary_pooled_concurrent` | 660 us | 793 us | 56 |
| `grpc_h2c_server_streaming_steady_state` | 1271 us | 1557 us | 376 |

## Still Bad

- Whole-process gRPC rows still allocate thousands of times per 32 ops.
- Latency is better in some rows, but still not close enough to claim
  production performance.
- The big remaining work is protocol turn count and internal allocation shape.
- Linux/x86_64 evidence is still needed.
