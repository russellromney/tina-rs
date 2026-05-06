# Eiffel — Outbound HTTP

Paired comparison for **outbound HTTP** between Tokio and Tina.

Tokio side:
- Server: `axum` Counter on `tokio::net::TcpListener`.
- Client: `reqwest` driving the scripted endpoint sequence.

Tina side:
- Server: native `tina_http::HttpListener` + `Counter` service isolate.
- Client: service-shaped `tina_http::HttpClient`, invoked via the same
  `call(client, msg, timeout).reply(continuation)` shape Tina uses
  everywhere.

Both sides walk the same script: 1 GET (counter=0), 3 POSTs
(counter=1,2,3), 1 GET (counter=3), 1 GET /missing (404). The compare
mode asserts the per-side reports agree.

```
cargo run                    # both sides + assert equivalent
cargo run -- tokio           # axum + reqwest only
cargo run -- tina            # native HttpListener + HttpClient only
```

## What this comparison surfaces

This is the companion to `eiffel_outbound_fetch` (raw-TCP one-line
protocol) and `eiffel_native_http` (native server, scripted-stdlib
client). Together they prove:

- **Tina-as-server and Tina-as-client compose.** The Tina side has no
  Tokio anywhere — listener, accept, reads, writes, parser, encoder,
  outbound connect, outbound parse are all Tina-owned.
- **The user-facing client API is one expression.** The driver
  isolate's only client interaction is:

  ```rust
  call(client, HttpClientMsg::call(target, request), timeout)
      .reply(DriverMsg::Returned)
  ```

  This is the shape Tina services already use elsewhere — visible call
  boundary, typed `CallOutcome::{Replied,Full,Closed,Timeout}` on the
  reply path, no fn-pointer mapper or spawn-and-route-back. The
  reqwest-side equivalent (`client.get(url).send().await?.text().await`)
  is shorter only because it hides cancellation, backpressure, and the
  underlying state machine.

- **Service-shaped HTTP works.** The client is one long-lived isolate
  that processes calls sequentially (one in flight at a time). For
  parallelism, either spawn multiple client isolates or front the
  client with `HttpConnectionPool` for explicit admission control.
  This is documented as the right Tina shape: capacity and
  parallelism are explicit, not hidden behind a `&mut Future` somewhere.

## What this comparison does not surface

- **Multi-slot pool concurrency.** First-form `HttpConnectionPool` is
  capacity-1 by construction; multi-slot is its own slice once the
  call-shaped primitive proves out.
- **Streaming bodies.** HTTP framing is `Content-Length` only on both
  sides. Streaming is a separate slice.
- **Connection-level keep-alive.** Each call opens a fresh TCP
  connection. Pool reuse covers the keep-alive *use case* without the
  state-machine complexity.
