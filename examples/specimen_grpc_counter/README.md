# Native gRPC Counter Specimen

This specimen runs a Tina-owned gRPC unary counter over the native
`tina-http` HTTP/2 h2c first form.

```sh
cargo run --manifest-path examples/specimen_grpc_counter/Cargo.toml
cargo test --manifest-path examples/specimen_grpc_counter/Cargo.toml
```

The service uses hand-written `prost::Message` types and
`GrpcRouter::unary("/specimen.Counter/Increment", ...)`. The wire path is:

```text
TCP -> Tina HTTP/2 h2c -> gRPC frame -> prost payload -> service handler
```

This is intentionally not tonic feature parity. It ships unary protobuf
messages, typed status trailers, per-message caps, explicit service-call
timeouts, and no compression. Server-streaming, client-streaming,
bidirectional streaming, interceptors, reflection, load balancing, and TLS ALPN
are later work.
