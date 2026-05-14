# Native gRPC Counter Specimen

This specimen runs a Tina-owned gRPC unary counter over the native
`tina-http` HTTP/2 h2c first form. The crate now also has live tests for native
server-streaming and incremental client-streaming route shapes, plus tonic h2c
interop.

```sh
cargo run --manifest-path examples/specimen_grpc_counter/Cargo.toml
cargo test --manifest-path examples/specimen_grpc_counter/Cargo.toml
```

With `grpcurl` installed, run the same service manually in one terminal and
call it from another:

```sh
cargo run --manifest-path examples/specimen_grpc_counter/Cargo.toml -- serve 127.0.0.1:57057
grpcurl -plaintext -proto examples/specimen_grpc_counter/proto/specimen_counter.proto \
  -d '{"delta": 7}' 127.0.0.1:57057 specimen.Counter/Increment
grpcurl -plaintext -proto examples/specimen_grpc_counter/proto/specimen_counter.proto \
  -d '{"delta": 5}' 127.0.0.1:57057 specimen.Counter/Watch
grpcurl -plaintext -proto examples/specimen_grpc_counter/proto/specimen_counter.proto \
  -d @ 127.0.0.1:57057 specimen.Counter/Sum <<'JSON'
{"delta": 10}
{"delta": 32}
JSON
```

The service uses hand-written `prost::Message` types and
`GrpcRouter::unary("/specimen.Counter/Increment", ...)`. The wire path is:

```text
TCP -> Tina HTTP/2 h2c -> gRPC frame -> prost payload -> service handler
```

This is intentionally not tonic feature parity. It ships unary protobuf
messages, server-streaming/incremental client-streaming route shapes, typed
status trailers, per-message caps, explicit service-call timeouts, tonic h2c
unary / server-streaming / normal and early-final client-streaming interop, and
no compression. Bidirectional streaming, reflection, load balancing, production
pooled clients, and TLS ALPN are later work.
