# specimen_outbound_http

Tina as both the *server* and the *outbound client* of the same HTTP
counter, scripted through `GET /counter → POST × 3 → GET /counter →
GET /missing`. The Tokio side runs `axum` + `reqwest`. Both sides
end with `final_counter_value=3` and a 404 on `/missing`.

## Run

```sh
cargo run --manifest-path examples/specimen_outbound_http/Cargo.toml -- both
cargo run --manifest-path examples/specimen_outbound_http/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_outbound_http/Cargo.toml -- tina
```

Both sides:

```
side=tokio successful_get=2 successful_post=3 final_counter_value=3 got_404_for_missing=true exit_clean=true
side=tina  successful_get=2 successful_post=3 final_counter_value=3 got_404_for_missing=true exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape

A `current_thread` Tokio runtime hosts both:

- `axum::serve(listener, app)` with a `with_graceful_shutdown(rx)`
  hook on a `oneshot`.
- A `reqwest::Client` that issues five requests, parses bodies,
  classifies statuses.

State is shared via `Arc<CounterState>`; the counter increments
atomically per `POST`. The whole thing fits in one async block.

## Tina shape

Three first-class HTTP pieces, driven from the host:

- **`Counter`** — `#[isolate(message = HttpRequest, reply =
  HttpResponse)]`. The handler dispatches through a
  `StatefulRouter<Counter>` (the stateful router helper) and returns an
  `HttpResponse`. `.method_not_allowed()` distinguishes 405 from 404.
- **`HttpListener<SingleShard>`** — bound with
  `HttpServerConfig::dev()` plus
  `limits.keepalive_idle_timeout = Some(...)`; ties the counter
  address to the network. Address comes back via
  `runtime.observe_next_bound()`.
- **`build_keepalive_pool`** — registers one pool isolate plus one
  `KeepaliveConnection` isolate. The script acquires a `PoolLease`,
  sends every request through the leased connection, releases the
  lease, then calls `shutdown_keepalive_pool(...)` so close, drain,
  and per-connection stop outcomes are asserted together. The trace
  asserts the server saw exactly one `TcpAccept` for the whole
  sequence.
- **Host script** — uses `ThreadedRuntime::call_blocking` for the
  test/specimen boundary. This removes the old one-off Driver isolate
  without changing the service truth: acquire, request, release, close
  are still ordinary typed Tina calls.

## Discussion

What feels better:

- **Native HTTP types from beginning to end.** Both server and
  client speak `HttpRequest` / `HttpResponse` directly. There's no
  "convert axum's `Request<Body>` into a Tina message" hop.
- **Visible backpressure.** The client side acquires a bounded pool
  lease before it can send. `AcquireOutcome`, `KeepaliveOutcome`, and
  `ReleaseOutcome` keep pool pressure, request errors, and release
  truth separate.
- **One TCP accept.** The Tina side proves reuse by counting
  `CallCompleted { call_kind: TcpAccept }` in the runtime trace after
  the script finishes.
- **No Driver isolate for host scripts.** `call_blocking` is the
  copied host-test shape. Real services still use
  `call(...).then(...)` inside their handlers.
- **`HttpServerConfig::dev()` / `HttpClientConfig::dev()`.**
  Roomy presets for examples; `pressure()` is the cap-matters
  variant (per the checklist entry on HTTP server / client
  configs).

What feels worse:

- **Pool lifecycle is explicit.** The host must acquire and release,
  then call `shutdown_keepalive_pool(...)` so pool close, drain, and
  per-connection stop buckets are all named and testable. That is more
  ceremony than `reqwest::Client`, but every resource transition is
  visible.
- **Configuring one runtime to host both server and client** means
  the spawn order matters (server first, then pool), and shutdown is
  `shutdown_keepalive_pool(...)` + listener stop + checked
  `runtime.shutdown()`.
  Tokio's `with_graceful_shutdown` is shorter.
