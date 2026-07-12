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
tcp_read(stream, 4096).then(ConnMsg::Read)
tcp_write(stream, bytes).then(ConnMsg::Wrote)
sleep(duration).then(Msg::TimerDone)
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

## Shard Worker Pinning (opt-in)

By default a shard worker thread floats across cores at the OS scheduler's
whim. `configured_core` pins it to one OS CPU id, where the platform can:

```rust
use tina_runtime::{AffinityStatus, LocalSystem};

// Pin shard 0's worker to OS CPU id 2. In a multi-shard system the next
// shard gets CPU 3, and so on (core + ordinal).
let app = LocalSystem::single_shard(MyShard(0), MyMailboxFactory)
    .configured_core(2)
    .build();

let topology = app.topology();
let shard = topology.shard(ShardId::new(0)).unwrap();
match shard.affinity_status() {
    // Linux: a real sched_setaffinity pin, proven by reading the core back.
    AffinityStatus::Applied => assert_eq!(shard.observed_core(), Some(2)),
    // macOS and other platforms have no hard pin; the worker runs unpinned.
    AffinityStatus::Unsupported => {}
    // CPU 2 was not in the process's allowed affinity mask (e.g. a cgroup or
    // cpuset). The worker runs unpinned rather than mis-pinning; the reason
    // is on the value.
    AffinityStatus::Failed(reason) => eprintln!("pin failed: {reason}"),
    other => unreachable!("configured_core never reports {other:?}"),
}
```

Honest rules:

- `configured_core` is an **OS CPU id**, checked against the process's allowed
  affinity mask — not an index into `0..num_cpus`. Containers and cpusets can
  expose sparse ids, so do not assume CPU 0 exists.
- A real hard pin happens only on Linux (`sched_setaffinity`). macOS offers
  only affinity *hints*, so it reports `Unsupported` rather than pretending.
- An id outside the allowed mask reports `Failed` and the worker keeps running
  unpinned — never a silent mis-pin to the wrong core.
- Only the shard worker is pinned. Helper lanes (DNS, storage, process, TLS)
  stay unpinned so they soak spare cores instead of fighting a shard for its
  core.
- Default is unpinned (`NotRequested`); no affinity syscall is made.

This ships the mechanism and honest reporting, not a throughput claim.

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
- HTTP/2 server/client with Tina-owned frames, bounded stream tables, explicit
  flow-control windows, h2c server, and h2c/h2-TLS client targets
- gRPC unary, server-streaming, client-streaming, and bidirectional streaming
  over Tina HTTP/2 with `prost`, typed `GrpcStatus` trailers, explicit message
  caps, service-call timeout mapping, and no compression
- HTTP types with `http`
- TLS state machine with `rustls` — driven by the runtime's per-shard TLS
  worker and bounded TLS queue
  (`tls_bind` / `tls_accept` / `tls_connect` / `tls_read` / `tls_write`
  / `tls_close`). `tina-http`'s `HttpsListener` and `HttpClient` use
  these directly: HTTP/1.1 over a real `rustls` handshake, no Tokio
  edge. DER cert/key inputs are explicit; no system trust roots.
  HTTP/2 client targets can use cleartext h2c or h2 over TLS with
  explicit ALPN. HTTP/2 server is still prior-knowledge h2c; HTTPS/2
  server ALPN and mTLS are future work.
- JSON with `serde_json`
- protobuf with `prost`
- Postgres wire with `postgres-protocol`

Tina should drive sockets and backpressure. Codec crates should only turn bytes
into structs and structs into bytes.

Do not hide Tokio under a Tina service and call it native.

Bridges are allowed. Bridges should say they are bridges.

## Boring Loop Helpers

One-shot rails stay truthful: `tcp_write`, `unix_write`, and file writes
may make partial progress, and reads return one chunk at a time. For
normal "write all" and "read until EOF/cap" service code, use the loop
helpers instead of hand-rolling byte counters:

- `TcpWriteAll` / `TcpReadToEof`
- `UnixWriteAll` / `UnixReadToEof`
- `FileReadChunks` / `FileWriteAll` / `FileCopyBounded`

Each helper exposes one step at a time. Your message enum still sees
the continuation, the runtime trace still shows every rail call, and
the helper owns the boring offset/progress math. See
[`../tcp-loops.md`](../tcp-loops.md).

For an event-only split service, `UnixWriteAll::next_service_event` and
`advance_service_event` accept a domain-event translator and supply the private
`ServiceMessage::Event` envelope. They preserve the full
`UnixWriteOwnedReply`, including the caller-owned buffer on failure. Generic
message isolates continue to use `next_effect` and `advance`. Both Unix and
TCP write-all helpers reject unarmed or wrong-allocation replies as invariant
violations; `is_in_flight` distinguishes an armed write from a completed loop.

## Native gRPC

`tina-http::GrpcRouter` is the native gRPC server layer. It sits on
`Http2Listener`, so the server transport is prior-knowledge cleartext h2c in
this slice. `tina-http::GrpcClient` is the native client layer over
`Http2ClientConnection`; it can target h2c or h2/TLS through `Http2Target`.

What ships:

- unary request/response;
- first server-streaming response path: one request message, many response DATA
  chunks, final gRPC status trailers;
- first client-streaming request path: many request messages over HTTP/2 DATA,
  one response message, final gRPC status trailers;
- first bidirectional streaming path: register it with `GrpcRouter::streaming`.
  The handler receives a `GrpcStreamingCall` with an explicit
  `GrpcRequestStream` pull handle and returns a Tina response chunk source, so
  request DATA and response DATA can progress independently over the HTTP/2
  connection owner without user code parsing gRPC frame bytes;
- `prost::Message` payload encode/decode;
- gRPC frame parsing (`compressed flag + u32 length + protobuf bytes`);
- `GrpcStatus` / `GrpcStatusCode` in HTTP/2 trailers;
- explicit per-message caps through `GrpcLimits` (`512 KiB` by default);
- service-call timeout mapped to `DeadlineExceeded`;
- compression rejected as `Unimplemented`.
- native `GrpcClient` for unary, server-streaming, client-streaming, and
  bidirectional streaming;
- tonic `0.12` h2c interop against the specimen for unary,
  server-streaming, client-streaming, and bidirectional streaming.

## Native WebSocket Client

`tina-http::WebSocketClientConnection` is the native WebSocket client
session. It owns a TCP or TLS rail, performs the HTTP/1.1 upgrade, masks
client frames, parses server frames, auto-answers ping with pong, exposes
typed send/receive/report calls, and emits WebSocket close facts through
the runtime trace.

It is session-shaped on purpose: no hidden reconnect, no retry loop, no
unbounded receive stream, and no Tokio fallback. The caller registers one
client connection isolate, calls `WebSocketClientMsg::Connect`, then uses
bounded `Send`, `Receive`, and `Report` calls. `Receive` arms the next inbound
read after the handshake, so a peer cannot fill an invisible background receive
queue while the app is not pulling. `WebSocketTarget` carries the explicit
`Host`, path, and for `wss`, SNI plus DER trust roots.

What does not ship yet: permessage-deflate, Autobahn compliance
classification, HTTP/2 WebSocket, proxy/cookie/redirect behavior, and a
pooled/reconnecting client manager.

What does not ship yet:

- grpcurl scripts or reflection;
- tonic feature parity, interceptors, reflection, health, or load balancing;
- pooled production gRPC clients and HTTP/2 mTLS.

The tiny `grpc_unary_call_h2c_blocking` helper exists to prove the native wire
path in tests and specimens without pulling in Tokio, hyper, or tonic. It is a
blocking helper, not a Tina client service, and it does not emit runtime trace
facts. Prefer `GrpcClient` for Tina services because it runs through the
runtime HTTP/2 client and emits protocol facts.
