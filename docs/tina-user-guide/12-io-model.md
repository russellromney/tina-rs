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
- HTTP types with `http`
- TLS state machine with `rustls` — driven by the runtime's TLS lane
  (`tls_bind` / `tls_accept` / `tls_connect` / `tls_read` / `tls_write`
  / `tls_close`). `tina-http`'s `HttpsListener` and `HttpClient` use
  these directly: HTTP/1.1 over a real `rustls` handshake, no Tokio
  edge. DER cert/key inputs are explicit; no system trust roots, no
  HTTPS/2, no ALPN. HTTP/2's first form is cleartext h2c server-side
  only; gRPC waits for the next protocol layer.
- JSON with `serde_json`
- protobuf with `prost`
- Postgres wire with `postgres-protocol`

Tina should drive sockets and backpressure. Codec crates should only turn bytes
into structs and structs into bytes.

Do not hide Tokio under a Tina service and call it native.

Bridges are allowed. Bridges should say they are bridges.
