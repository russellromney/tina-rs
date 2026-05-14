# I/O Model

Tina user code should not care which live I/O backend runs underneath it.

The user shape is:

```text
handler returns runtime call
runtime owns I/O
runtime later sends continuation message
```

## Current Stack

Tina has three important layers:

- `tina-runtime` owns Tina semantics: effects, scheduling, calls, timeout,
  tracing, shutdown, and resource IDs.
- Betelgeuse is the canonical portable live I/O machine under `tina-runtime`.
- `tina-sim` is the deterministic simulator for replay and DST.

On Linux, Betelgeuse uses its native Linux backend. On macOS, Betelgeuse uses
its native Darwin backend. Tina does not plan a duplicate I/O substrate unless
Betelgeuse cannot satisfy a named Tina contract.

## What Betelgeuse Does Here

Betelgeuse is the portable live I/O substrate today.

Grug version:

```text
submit work
step/poll backend
get completion
give completion back to Tina runtime
```

Tina wraps that in isolate-friendly effects:

```rust
tcp_read(stream, 4096).reply(ConnMsg::Read)
tcp_write(stream, bytes).reply(ConnMsg::Wrote)
sleep(duration).reply(Msg::TimerDone)
```

Application code should not call Betelgeuse directly.

## What Tina Owns

Tina owns:

- resource IDs like `StreamId` and `ListenerId`
- same-resource rules, like one pending read per stream lane
- deadlines and timeout outcomes
- cancellation and close behavior
- trace events
- live-vs-sim semantic contract
- bounded lane capacity
- shutdown accounting

The backend owns platform mechanics.

## What The Simulator Owns

The simulator replaces physical I/O with scripted I/O.

Same service shape:

```text
tcp_read effect
scripted read completion
same continuation message
same handler code
```

That is why Tina can do DST. Effects are data. Runtime decisions can be
recorded and replayed.

## Adopt Codecs, Not Runtimes

For protocols, prefer boring sync codec crates:

- HTTP/1 parse with `httparse`
- HTTP/2 cleartext server first form with Tina-owned frames, bounded stream
  table, and explicit flow-control windows
- gRPC unary plus server-streaming/incremental client-streaming form over Tina
  HTTP/2 h2c with `prost`, typed `GrpcStatus` trailers, explicit message caps,
  service-call timeout mapping, tonic h2c smoke interop for shipped modes, and
  no compression
- HTTP types with `http`
- TLS state machine with `rustls` — driven by the runtime's TLS lane
  (`tls_bind` / `tls_accept` / `tls_connect` / `tls_read` / `tls_write`
  / `tls_close`). `tina-http`'s `HttpsListener` and `HttpClient` use
  these directly: HTTP/1.1 over a real `rustls` handshake, no Tokio
  edge. DER cert/key inputs are explicit; no system trust roots, no
  HTTPS/2, no ALPN. HTTP/2's first form is cleartext h2c server-side
  only. gRPC's first form also uses that h2c path; TLS ALPN remains future
  work.
- JSON with `serde_json`
- protobuf with `prost`
- Postgres wire with `postgres-protocol`

Tina should drive sockets and backpressure. Codec crates should only turn bytes
into structs and structs into bytes.

Do not hide Tokio under a Tina service and call it native.

Bridges are allowed. Bridges should say they are bridges.

## Native gRPC First Form

`tina-http::GrpcRouter` is the current native gRPC layer. It sits on
`Http2Listener`, so the transport is prior-knowledge cleartext h2c in this
slice.

What ships:

- unary request/response;
- first server-streaming response path: one request message, many response DATA
  chunks, final gRPC status trailers;
- incremental client-streaming request path: many request messages over HTTP/2
  DATA, one response message, final gRPC status trailers;
- `GrpcRequestStream<T>::next` pull handle for Tina service isolates that need
  explicit request-message cadence before bidirectional streaming;
- `prost::Message` payload encode/decode;
- gRPC frame parsing (`compressed flag + u32 length + protobuf bytes`);
- `GrpcStatus` / `GrpcStatusCode` in HTTP/2 trailers;
- explicit per-message caps through `GrpcLimits` (`512 KiB` by default);
- service-call timeout mapped to `DeadlineExceeded`;
- tonic h2c smoke interop for unary, server-streaming, and normal/early-final
  client-streaming;
- compression rejected as `Unimplemented`.

What does not ship yet:

- TLS ALPN / HTTPS/2 gRPC;
- true bidirectional streaming with independent request/response lifecycles;
- automated grpcurl interop scripts or reflection;
- full tonic feature parity, interceptors, reflection, health, or load
  balancing;
- a pooled Tina gRPC client service.

The tiny `grpc_unary_call_h2c` helper exists to prove the native wire path in
tests and specimens without pulling in Tokio, hyper, or tonic. Production
client topology should become a normal Tina client service once HTTP/2 client
plumbing grows beyond this first form.
