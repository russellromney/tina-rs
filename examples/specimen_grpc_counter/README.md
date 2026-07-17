# Native gRPC Counter Specimen

This specimen runs a Tina-owned gRPC counter over the native `tina-http`
HTTP/2 h2c first form, and calls it back with the **native gRPC client** —
no Tokio, no `grpc_unary_call_h2c_blocking`. The crate has live tests for
unary, server-streaming, client-streaming, and bidirectional-streaming
route shapes, including tonic `0.12` h2c interop.

## Native client (the copied path)

`run_smoke()` and `SpecimenServer::native_grpc_smoke()` show the path users
should copy: register one `Http2ClientConnection` isolate, wrap it in a
`GrpcClient`, then build a submit / call the connection / fold the reply:

```rust,ignore
let conn = runtime.register_with_capacity::<Http2ClientConnection<S>, _>(
    Http2ClientConnection::new(target, Default::default())?, 32)?;
runtime.try_send(conn, Http2ClientMsg::Begin)?;
let client = GrpcClient::new(conn, GrpcLimits::default());

let submit = client.unary_request("/specimen.Counter/Increment", &req)?;
let CallOutcome::Replied(reply) =
    runtime.call_blocking(client.connection(), submit, timeout)? else { return; };
match client.unary_outcome_from_reply::<CounterReply>(reply) {
    GrpcUnaryOutcome::Ok(msg)       => { /* OK + decoded message */ }
    GrpcUnaryOutcome::Status(s)     => { /* non-OK status, e.g. PermissionDenied */ }
    GrpcUnaryOutcome::Transport(t)  => { /* HTTP/2 transport failure */ }
    GrpcUnaryOutcome::Malformed(e)  => { /* not well-formed gRPC */ }
}
```

The specimen exercises an OK call (`Increment`), a non-OK status call
(`Forbidden` → `PermissionDenied`), and a client cancellation
(`Http2ClientMsg::Cancel`) — and proves the connection survives the cancel.
`grpc_unary_call_h2c_blocking` remains only as a test-only convenience.

```sh
cargo run --manifest-path examples/specimen_grpc_counter/Cargo.toml
cargo test --manifest-path examples/specimen_grpc_counter/Cargo.toml
```

The service uses hand-written `prost::Message` types and typed `GrpcRouter`
routes. Stateful unary and bidirectional methods point directly at split-service
request addresses:

```rust,ignore
let router = GrpcRouter::<S>::new(limits)
    .with_actor_route_capacity(16)?
    .try_unary_actor("/specimen.Counter/Increment", counter.requests, timeout)?
    .try_streaming_actor("/specimen.Counter/Chat", streams.requests, timeout)?;
```

The complete route set uses `try_unary_actor` for `Increment` and `Forbidden`,
`server_streaming_buffered("/specimen.Counter/Watch", ...)`,
`client_streaming("/specimen.Counter/Sum", ...)`, and
`try_streaming_actor` for `Chat`. The wire path is:

```text
TCP -> Tina HTTP/2 h2c -> gRPC frame -> prost payload -> service handler
```

This is intentionally not tonic feature parity. It ships server-side h2c
unary, server-streaming, client-streaming, and bidirectional streaming, typed
status trailers, per-message caps, explicit service-call timeouts, and no
compression. The bidirectional route atomically moves `GrpcStreamingCall` and
its `GrpcRequestStream` into a streaming-factory service. That service observes
one child spawn and returns the child's typed response-source address. There is
no mutex stream slot, router-state mutex, or preallocated response pool.

Actor-route admission is bounded once for the router. Over-capacity calls become
`ResourceExhausted`; target `Closed`, route deadline, and runtime rejection map
to distinct `Unavailable`, `DeadlineExceeded`, and `FailedPrecondition`
statuses. If the HTTP caller disappears before a stream source returns, the
router cancels that source so the child releases its owned request stream. The
tonic h2c proof is:

```sh
cargo test --manifest-path examples/specimen_grpc_counter/Cargo.toml specimen_grpc_counter_tonic_h2c_interop -- --nocapture
```

grpcurl reflection, interceptors, load balancing, production pooled clients,
server-side HTTPS/2 ALPN, and mTLS are later work.
