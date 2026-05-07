# eiffel_outbound_http

Tina as both the *server* and the *outbound client* of the same HTTP
counter, scripted through `GET /counter → POST × 3 → GET /counter →
GET /missing`. The Tokio side runs `axum` + `reqwest`. Both sides
end with `final_counter_value=3` and a 404 on `/missing`.

## Run

```sh
cargo run --manifest-path examples/eiffel_outbound_http/Cargo.toml -- both
cargo run --manifest-path examples/eiffel_outbound_http/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_outbound_http/Cargo.toml -- tina
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

Three first-class HTTP isolates, plus one helper:

- **`Counter`** — `#[isolate(message = HttpRequest, reply =
  HttpResponse)]`. The handler matches on `(request.method,
  request.path)` and returns an `HttpResponse`.
- **`HttpListener<SingleShard>`** — bound with
  `HttpServerConfig::dev()`; ties the counter address to the
  network. Address comes back via `runtime.observe_next_bound()`.
- **`HttpClient<SingleShard>`** — long-lived client isolate
  registered with `HttpClientConfig::dev()`. Each request goes
  through `call(client, HttpClientMsg::call(target, request),
  timeout)`.
- **`Driver`** — a one-shot isolate per request that takes a
  `Begin { client, target, request }`, issues the `call(...)`,
  awaits `Returned(CallOutcome<...>)`, forwards the result to the
  host thread via `std::sync::mpsc`, and `stop()`s. The host's
  `run_request()` helper just spawns one of these per call and
  blocks on `recv_timeout`.

## Discussion

What feels better:

- **Native HTTP types from beginning to end.** Both server and
  client speak `HttpRequest` / `HttpResponse` directly. There's no
  "convert axum's `Request<Body>` into a Tina message" hop.
- **Visible backpressure.** `HttpClient`'s `call(...)` returns a
  `CallOutcome<Result<HttpResponse, HttpClientError>>`; the
  `Full`, `Closed`, `Timeout` arms surface as typed errors at the
  call site. The `reqwest::Client` version has no equivalent.
- **`HttpServerConfig::dev()` / `HttpClientConfig::dev()`.**
  Roomy presets for examples; `pressure()` is the cap-matters
  variant (per the new checklist entry on HTTP server / client
  configs).

What feels worse:

- **The `Driver` isolate is the bridge between sync host code and
  isolate-driven `call(...)`.** It's small (~25 lines) and reusable
  per call, but it's the friction of "the host wants to await an
  in-process call" without a typed observation handle for that.
- **`run_request` blocks on `mpsc::recv_timeout`.** That's the
  documented pattern for sync host code awaiting a Tina-side call,
  but a typed `IsolateResultWaiter<T>` would shrink it to one line.
- **Configuring one runtime to host both server and client** means
  the spawn order matters (server first, then client), and shutdown
  is two `try_send`s plus a `runtime.shutdown()`. Tokio's
  `with_graceful_shutdown` is one bookkeeping line shorter.

What this suggests:

- A typed "wait for this isolate's next call to complete" handle
  would replace `Driver` + `mpsc` in every example that does sync
  host-thread bridging (this one, `eiffel_persistent_counter`,
  `eiffel_outbound_fetch`).
- The native HTTP server + client pair is a strong demonstration of
  the runtime: `tina_http` is a real story on top of the same
  isolate model the rest of the runtime uses.
