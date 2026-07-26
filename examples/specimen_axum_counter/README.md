# Specimen Axum Counter

Paired Tokio-vs-Tina implementation of a tiny stateful HTTP service. Both
sides expose:

```text
POST /counter/increment   →  the new counter value
GET  /counter             →  the current counter value
```

Both sides serve real HTTP/1.1 over `axum::serve(...)` and are exercised by
the same scripted client that issues `POST`, `POST`, `GET` and asserts the
bodies are `1`, `2`, `2` and statuses are all `200`.

Run both sides:

```bash
cargo run --manifest-path examples/specimen_axum_counter/Cargo.toml -- compare
```

Run one side:

```bash
cargo run --manifest-path examples/specimen_axum_counter/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_axum_counter/Cargo.toml -- tina
```

## What this comparison taught us

### Tokio side

- Trivial. `Arc<Mutex<CounterState>>` in app state, two handlers, four lines
  each. Axum makes the happy path obvious.
- The lie of omission is right there: nothing pushes back on you for using
  `Mutex` from inside an async handler. It works because the critical
  section is microscopic; with anything heavier the same shape would block
  the executor without warning.

### Tina side

- The bridge carries its weight. `BridgeHost::register_bridge(...)`
  hands back a `Clone` handle that gets wrapped in `TinaTowerService`
  and dropped straight into `Router::with_state(...)`. The handler
  calls `svc.call(req).await` and that is the entire call site.
  Composing with axum's `State` extractor is still the strongest part
  of the bridge story.
- The `Counter` isolate is a one-liner per arm: pattern-match the request
  variant, mutate `self.value`, `responder.respond(reply)`, `noop()`.
  Genuinely good ergonomics for a stateful HTTP endpoint.
- `BridgeError::{Full,Closed,Timeout}` reach the handler as a real
  variant, so HTTP-shaped pushback is visible at the call site instead
  of silently buffered. The error map at the top of `tina_impl.rs` is a
  small honest table — `Full`/`Closed` → `503`, `Timeout` → `504` —
  rather than the previous one-status-fits-all collapse. The Tokio side
  cannot offer this at all.
- The Tower shape (`Service::call(...)` instead of `bridge.call(...)`)
  feels right. It's the same surface Tower middleware speaks, so
  layering rate-limit / timeout / load-shed onto this service is a
  matter of stacking layers rather than weaving callbacks. That was a
  real "ah, this is the right abstraction" moment.
- `BridgeError` finally implements `Display` and `std::error::Error`,
  so `format!("{error}")` works and tower-http `BoxError` accepts our
  errors directly. Before this, debugging error variants meant
  `format!("{error:?}")` everywhere.

### What was awkward

- ~~Forty more lines of `BridgeMailbox` + `BridgeMailboxFactory`
  boilerplate before any service code can run.~~ **Resolved:**
  the example uses `DefaultThreadedMailboxFactory`.
- ~~The runtime + bridge wiring is verbose: build `ThreadedRuntime` →
  `register_with_capacity::<Counter, Infallible>` → `BridgeHandle::new` →
  pass `Arc` clones around.~~ **Mostly resolved:** `LocalSystem::single_shard(...)`
  builds the app, `BridgeHost::from_app(app)` takes ownership of the runtime,
  and `register_bridge` hands back the bridge handle in one place. There is
  still setup, but it now reads like one bridge-hosted service shape.
- ~~Shutdown of the Tina runtime requires unwrapping the `Arc` and calling
  `shutdown()` only when no clones remain.~~ **Resolved:**
  `BridgeHost::drain_and_shutdown(...)` waits for handle clones to drain,
  returns a structured report, and leaves the host retryable on timeout.
- Both sides need a `tokio::runtime` to host axum, so the Tina side ends
  up with **two** runtimes (Tina's own thread + a Tokio current-thread
  runtime). That is the bridge's nature, but it's a real comprehension
  cost the first time you see it.
- The state type used to be brutal — six generic params with a
  trailing `()` for `AR`. The bridge polish slice fixed that: the
  specimen reads `TinaService<CounterRequest, CounterReply>`.
- Every handler that calls `svc.call(...)` opens with `let mut svc =
  svc;` because Axum's `State<S>` extracts the value, not `&mut S`,
  and `Service::call` requires `&mut self`. Trivial but cluttery —
  every Tina-bridged Axum handler is going to have this line.
- Setup is still two-step: `BridgeHost::register_bridge(...)` then
  `TinaTowerService::new(bridge)`. The reqwest-bridge crate ships a
  one-call `install(&runtime, config)` helper; `tina-tokio-bridge`
  could expose the same.
