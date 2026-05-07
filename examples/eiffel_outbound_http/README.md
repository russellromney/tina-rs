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

Three first-class HTTP isolates, plus a scripting Driver:

- **`Counter`** — `#[isolate(message = HttpRequest, reply =
  HttpResponse)]`. The handler dispatches through a
  `StatefulRouter<Counter>` (Phase 059 Rock 6) and returns an
  `HttpResponse`. `.method_not_allowed()` distinguishes 405 from 404.
- **`HttpListener<SingleShard>`** — bound with
  `HttpServerConfig::dev()`; ties the counter address to the
  network. Address comes back via `runtime.observe_next_bound()`.
- **`HttpClient<SingleShard>`** — long-lived client isolate
  registered with `HttpClientConfig::dev()`. Each request goes
  through `call(client, HttpClientMsg::call(target, request),
  timeout)`.
- **`Driver`** — single isolate that walks the whole script. A
  `Step` field tracks where the sequence is; each step dispatches
  one `call(client, ...).reply(DriverMsg::Returned)`. After the last
  step the driver `stop_with(report)`s and the host reads the
  typed `Report` through `runtime.observe_result::<Report>` (Phase
  059 Rock 1) — no `mpsc::channel`, no per-request bridge.

## Discussion

What feels better:

- **Native HTTP types from beginning to end.** Both server and
  client speak `HttpRequest` / `HttpResponse` directly. There's no
  "convert axum's `Request<Body>` into a Tina message" hop.
- **Visible backpressure.** `HttpClient`'s `call(...)` returns a
  `CallOutcome<Result<HttpResponse, HttpClientError>>`; the
  `Full`, `Closed`, `Timeout` arms surface as typed errors at the
  call site. The `reqwest::Client` version has no equivalent.
- **One typed final value.** `stop_with(report)` +
  `observe_result::<Report>` is the whole host-isolate bridge. The
  earlier `Driver` + `mpsc::Sender` per request is gone.
- **`HttpServerConfig::dev()` / `HttpClientConfig::dev()`.**
  Roomy presets for examples; `pressure()` is the cap-matters
  variant (per the checklist entry on HTTP server / client
  configs).

What feels worse:

- **Configuring one runtime to host both server and client** means
  the spawn order matters (server first, then client), and shutdown
  is two `try_send`s plus a `runtime.shutdown()`. Tokio's
  `with_graceful_shutdown` is one bookkeeping line shorter.
- **`Step` enum + `dispatch`/`absorb` is one explicit state
  machine per scripted scenario.** Tokio's async/await reads
  linearly. The continuation pattern from chapter 16 of the user
  guide names this shape; the trace is honest about every step.
