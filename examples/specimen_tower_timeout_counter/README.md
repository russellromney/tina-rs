# specimen_tower_timeout_counter

Tokio-vs-Tina counter behind the same Tower middleware stack:
`ConcurrencyLimit(2)` and `Timeout(150ms)` over a service that takes
`100ms` per call. Both sides drive a burst of 8 concurrent requests
through the stack via `tower::ServiceExt::oneshot`.

## Run

```sh
cargo run --manifest-path examples/specimen_tower_timeout_counter/Cargo.toml -- both
cargo test --manifest-path examples/specimen_tower_timeout_counter/Cargo.toml
```

Representative output:

```
side=tokio successful=8 service_unavailable=0 gateway_timeout=0 exit_clean=true
side=tina  successful=1 service_unavailable=0 gateway_timeout=7 exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape

Service is `tower::service_fn` over `Arc<Mutex<Counter>>`; the slow
work is `tokio::time::sleep`. The Tower stack is the textbook one:

```rust
ServiceBuilder::new()
    .layer(TimeoutLayer::new(150ms))
    .layer(ConcurrencyLimitLayer::new(2))
    .service(inner)
```

The driver calls `svc.clone().oneshot(req).await` for each of the
eight requests in parallel via `JoinSet`.

## Tina shape

Service is a registered Tina isolate behind `TinaTowerService`. Each
request is admitted to the isolate's mailbox; the handler stores the
`BridgeResponder<BrushReply>` in a `VecDeque` and schedules
`sleep(SLOW_HANDLER_MS).reply(Done)`. On `Done`, the handler pops the
front responder, increments the counter, responds, and either chains
another `sleep` or goes idle.

The Tower stack is identical:

```rust
ServiceBuilder::new()
    .layer(TimeoutLayer::new(150ms))
    .layer(ConcurrencyLimitLayer::new(2))
    .service(TinaTowerService::new(bridge))
```

`bridge_cancelled()` lets the handler short-circuit a request whose
Tower future has already given up (timeout fired upstream); the
runtime sees one fewer `sleep` event.

## Discussion

The two sides intentionally produce *different* outcome shapes under
the same middleware — that is the lesson:

- **Tokio**: `tower::service_fn`'s closure runs on the multi-threaded
  Tokio runtime. With `ConcurrencyLimit(2)` and 100 ms work, two
  futures run truly in parallel and each finishes well within the
  150 ms timeout. `oneshot`'s `poll_ready` waits for a slot before
  starting the timer (the timer wraps the inner call after admission),
  so queued requests don't tick down their own timeout while waiting.
  Result: 8 successful, 0 timeout.
- **Tina**: the `Counter` isolate handles one mailbox message per
  turn. Even with `ConcurrencyLimit(2)` admitting two BridgeRequests
  to the bridge, the isolate processes them serially via its `pending`
  queue. Each `Done` continuation takes 100 ms wall-clock; by the
  third or fourth call, the wait alone has exceeded 150 ms. Tower's
  `Timeout` layer fires; the isolate eventually pulls the cancelled
  `BridgeRequest` from the mailbox and short-circuits via
  `bridge_cancelled()`.
  Result: 1 successful, ~7 gateway timeouts.

What feels better:

- **Same middleware compiles unchanged.** The Tower stack is
  declarative and survives the swap from `service_fn` to
  `TinaTowerService` without code changes.
- **Pressure surfaces cleanly.** `BridgeError::Full` /
  `BridgeError::Closed` / `Elapsed` (Tower's `Timeout`) all appear
  at the call site. The Tower errors that wrap our typed Tina
  errors come out via `BoxError::downcast_ref::<BridgeError>()`.
- **Cancellation truth is reachable.** `BridgeRequest::is_cancelled`
  lets the handler drop work whose Tower future has gone away, so
  the trace doesn't accumulate hidden `sleep` events for nobody.

What feels worse:

- **`oneshot` on a `Service` clone is heavy ergonomics for a single
  call.** The Tower readiness contract makes `Service::call(req)`
  panic if `poll_ready` was not awaited first; `ServiceExt::oneshot`
  fixes that but means every call site clones the service.
- **`TinaService<M, R>` alias is fixed at `TM = BridgeRequest<M, R>`.**
  The moment a specimen needs a richer `CounterMsg` enum (because the
  isolate also handles its own `Done` continuation) the alias does
  not type-check. The full `TinaTowerService::new(bridge)` form
  works but is harder to read.
- **Cancellation is opt-in per handler.** The
  `BridgeMessage::bridge_cancelled` hook is wired in
  `BridgeGuard`, but a handler that does not use it still does the
  full `sleep(...)` and tries to `respond()` on a closed responder.
  That works (drop-respond is a `Result`), but it means a slow
  handler with no cancellation check accumulates `sleep` events for
  abandoned work.

## What this is not

This specimen is about middleware composition over the bridge. It is
not a benchmark of which side is "faster": the Tokio service has
real parallelism and the Tina isolate intentionally serializes — that
is the model difference, not a bug.
