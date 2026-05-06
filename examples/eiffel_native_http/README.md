# Eiffel Native HTTP

Paired Tokio-vs-Tina implementation of a tiny HTTP/1.1 service. The
Tokio side is `axum` over the standard `tokio::net::TcpListener`. The
**Tina side is `tina-http` running on `tina-runtime`'s threaded
runtime — no Tokio anywhere on the server edge.**

This is the first Eiffel comparison where Tina speaks the wire protocol
itself. The previous `eiffel_axum_counter` comparison used the
`tina-tokio-bridge` to put a Tina-supervised core behind an axum edge;
here Tina does both edge and core.

The comparison runs the same scripted client against each side:

```text
GET  /counter          -> 200, body "0"
POST /counter (x3)     -> 200, body "1", "2", "3"
GET  /counter          -> 200, body "3"
GET  /missing          -> 404
```

Both sides emit the same numbers and the run is asserted in
`assert_equivalent`:

```text
successful_get=2 successful_post=3 final_counter_value=3
got_404_for_missing=true exit_clean=true
```

Run both sides:

```bash
cargo run --manifest-path examples/eiffel_native_http/Cargo.toml -- compare
```

Run one side:

```bash
cargo run --manifest-path examples/eiffel_native_http/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_native_http/Cargo.toml -- tina
```

## What this comparison taught us

### Tokio side

- `axum::Router::new().route("/counter", get(...).post(...))` plus an
  `Arc<CounterState>` with `AtomicU32::fetch_add` is a fifteen-line
  service. The HTTP framing, keep-alive, header parsing, and response
  serialisation are all done by `hyper` (axum's underlying server) and
  `tokio::net`. None of it is in this file.
- `axum::serve(listener, app).with_graceful_shutdown(rx).await` is the
  canonical shutdown shape. It returns when `rx` resolves; clean.
- HTTP/1.1 keep-alive is the default. The client must send
  `Connection: close` (or close its own write side) to cleanly stop a
  request — otherwise `read_to_end` blocks waiting for FIN. We learned
  this the slightly-painful way in the scripted client.

### Tina side

What worked well:

- `HttpListener::<AppShard>::new(bind_addr, slot, service, limits,
  timeout, capacity)` plus `runtime.try_send(listener,
  HttpListenerMsg::Start)` is the entire bind + accept setup. The
  listener spawns one `HttpConnection` isolate per accepted socket;
  each one reads, parses, calls the user's `Counter` service via
  `tina_runtime::call`, writes the response, and closes.
- The `Counter` service isolate handles `HttpRequest` directly and
  replies `HttpResponse`. No middleware, no wrappers, no traits to
  implement beyond `Isolate`. `match (request.method.clone(),
  request.path.as_str())` is the routing — small enough to fit on
  screen.
- `CallOutcome::{Full, Closed, Timeout}` map to `503`, `500`, and
  `504` respectively — visible HTTP-shaped pushback the bridge
  comparison foreshadowed but a native server can deliver
  end-to-end.
- One request per connection (no keep-alive in 048a) means the
  scripted client doesn't need `Connection: close` against the Tina
  side; the server closes after each response. The client sends it
  anyway for the Tokio side.

What was awkward or surprising:

- Same `Mailbox` + `MailboxFactory` boilerplate that every Eiffel
  comparison has. The `tina-http` crate doesn't help here — the
  surrounding service still needs the standard ~40 lines. 047's
  default mailbox factory removes this when it lands.
- The bound socket address is still smuggled through
  `Arc<Mutex<Option<SocketAddr>>>`. The `BoundAddr` slot pattern
  shows up in every TCP server example. 047's host observation
  handles will replace it.
- The `HttpListener::<AppShard>` turbofish is required because
  `tina-http`'s listener and connection isolates are generic over
  `S: Shard`. This is the cost of making the crate work with any
  user-chosen shard.
- `axum`'s `Router::route("/counter", get(...).post(...))` is
  shorter than the `match (method, path) { ... }` body inside the
  Tina service isolate. Once routes go beyond a handful, the user
  will want a tiny routing helper; that's 048c's job.

### Tokio shape vs. Tina shape, in one paragraph

`axum` is what people reach for when they want a Rust HTTP service in
2026. The Tokio side here is sixty lines, leverages a mature parsing
+ HTTP/1.1 framing + keep-alive + multiplexed-request stack, and is
correct. The Tina side is closer to a hundred lines once you count
the service isolate, the listener wiring, and the boilerplate; in
exchange every step of HTTP handling — accept, read, parse, dispatch,
write, close — is a separately observable event in the runtime trace,
the supervision and bounded-mailbox stories are first-class, and the
service runs with no Tokio runtime in the process.
