# Native gRPC Counter Specimen

This specimen runs a Tina-owned gRPC counter over the native `tina-http`
HTTP/2 h2c first form. The crate has live tests for unary, server-streaming,
client-streaming, and bidirectional-streaming route shapes, including tonic
`0.12` h2c interop.

```sh
cargo run --manifest-path examples/specimen_grpc_counter/Cargo.toml
cargo test --manifest-path examples/specimen_grpc_counter/Cargo.toml
```

The service uses hand-written `prost::Message` types and `GrpcRouter` routes:
`unary("/specimen.Counter/Increment", ...)`,
`server_streaming("/specimen.Counter/Watch", ...)`,
`client_streaming("/specimen.Counter/Sum", ...)`, and
`streaming("/specimen.Counter/Chat", ...)`. The wire path is:

```text
TCP -> Tina HTTP/2 h2c -> gRPC frame -> prost payload -> service handler
```

This is intentionally not tonic feature parity. It ships server-side h2c
unary, server-streaming, client-streaming, and bidirectional streaming, typed
status trailers, per-message caps, explicit service-call timeouts, and no
compression. The bidirectional route uses `GrpcStreamingCall`,
`GrpcRequestStream`, and `GrpcStreamingResponse` so service code does not
hand-parse gRPC frame bytes. The example keeps per-call response-source
ownership explicit with a named bounded source pool, so overload is a visible
`ResourceExhausted` status instead of a hidden queue. The tonic h2c proof is:

```sh
cargo test --manifest-path examples/specimen_grpc_counter/Cargo.toml specimen_grpc_counter_tonic_h2c_interop -- --nocapture
```

grpcurl reflection, interceptors, load balancing, production pooled clients,
and TLS ALPN are later work.
