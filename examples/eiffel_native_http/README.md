# eiffel_native_http

A tiny HTTP/1.1 counter service. Tokio side: `axum` on
`tokio::net::TcpListener`. Tina side: `tina_http::HttpListener` +
`Counter` isolate. The shared `scripted_client` is a tiny std::net
HTTP/1.1 client that hits both sides identically: `GET /counter →
POST × 3 → GET /counter → GET /missing`.

This is the inbound HTTP comparison; the
[`eiffel_outbound_http`](../eiffel_outbound_http/README.md) is the
*client* comparison. The complement.

## Run

```sh
cargo run --manifest-path examples/eiffel_native_http/Cargo.toml -- both
cargo run --manifest-path examples/eiffel_native_http/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_native_http/Cargo.toml -- tina
```

Both sides:

```
side=tokio successful_get=2 successful_post=3 final_counter_value=3
           got_404_for_missing=true exit_clean=true
side=tina  successful_get=2 successful_post=3 final_counter_value=3
           got_404_for_missing=true exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)
- [`src/lib.rs`](src/lib.rs) — the shared scripted HTTP/1.1 client.

## Tokio shape

`axum::Router` with two handlers and a fallback. State is
`Arc<CounterState>` with an `AtomicU32`. `serve(listener, app)` with
a `with_graceful_shutdown` hook on a `oneshot`. The whole server fits
in one async block.

## Tina shape

A `Counter` isolate, declared with `#[tina::isolate(message =
HttpRequest, reply = HttpResponse)]`, and an
`HttpListener::with_config(addr, counter, HttpServerConfig::dev())`.
The listener owns the bind + accept dance and dispatches each
parsed request to the counter.

The counter handler matches on `(request.method, request.path)` and
returns an `HttpResponse`. The runtime hands the response back to
the connection isolate, which writes it on the wire.

## Discussion

What feels better:

- **The handler is a method on a state-owning isolate.** No
  `Arc<Mutex<_>>`, no `State<Arc<...>>` extractor. `self.value += 1`
  is the increment because the counter *owns* the value.
- **`HttpRequest` and `HttpResponse` are real types.** Status,
  headers, body all directly accessible. No `IntoResponse` trait
  dance, no axum extractors.
- **`HttpServerConfig::dev()` is one knob, not five.** Limits,
  service-call timeout, connection mailbox capacity all preset.
  `pressure()` is the cap-matters preset. (047 / new checklist.)
- **Bound-address waiter is typed.** `runtime.observe_next_bound()`
  returns the address as a typed `BoundAddressWaiter`; no
  `Arc<Mutex<Option<SocketAddr>>>`.

What feels worse:

- **Routing is a `match`.** Tokio's `Router::route("/counter",
  get(...).post(...))` is shorter than the Tina handler's `match
  (request.method, request.path)` arms. A small routing helper
  would close the gap (047 / new checklist mentions a "tiny routing
  shape" as deferred — `Router::new().get(...)` style).
- **Handler return is `reply(HttpResponse)`.** `axum`'s
  `IntoResponse` trait collapses `String → 200 OK`, `StatusCode →
  empty body`, etc.; Tina's `HttpResponse::text(...)` /
  `with_status(...)` builders are more explicit but also more
  verbose. Same trade-off as the macro vs hand-rolled byte
  service.

What this suggests:

- The full inbound HTTP story is on the runtime now. `HttpListener`
  + `HttpServerConfig::dev()` is genuinely one preset away from
  reasonable defaults; the gap to axum's `Router` is mostly the
  routing macro shape.
- The next ergonomics win for HTTP services is routing: a
  `Router`-shaped helper that handles `(method, path)` matching
  with declarative routes would shrink most service handlers to
  the same shape as axum's.
