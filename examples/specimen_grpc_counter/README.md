# Native gRPC Counter Specimen

This specimen runs a Tina-owned gRPC unary counter over the native
`tina-http` HTTP/2 h2c first form. The crate now also has live tests for the
first native server-streaming and client-streaming route shapes.

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
messages, first server-streaming/client-streaming route shapes, typed status
trailers, per-message caps, explicit service-call timeouts, and no compression.
Bidirectional streaming, tonic/grpcurl interop scripts, interceptors,
reflection, load balancing, production pooled clients, and TLS ALPN are later
work.
