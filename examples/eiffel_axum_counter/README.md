# Eiffel Axum Counter

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
cargo run --manifest-path examples/eiffel_axum_counter/Cargo.toml -- compare
```

Run one side:

```bash
cargo run --manifest-path examples/eiffel_axum_counter/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_axum_counter/Cargo.toml -- tina
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

- The bridge crate carries its weight. `BridgeHandle::new(...)` produced a
  `Clone` handle that goes straight into `Router::with_state(...)`, and
  `bridge.call(req).await` is the whole call site. The fact that this
  composes with axum's `State` extractor is the strongest thing about the
  bridge story.
- The `Counter` isolate is a one-liner per arm: pattern-match the request
  variant, mutate `self.value`, `responder.respond(reply)`, `noop()`. That
  is genuinely good ergonomics for a stateful HTTP endpoint.
- `BridgeError::{Full,Closed,Timeout}` reach the handler as a real error
  variant, so HTTP-shaped pushback is visible at the call site instead of
  silently buffered. This is the property the Tokio side cannot offer at
  all.

### What was awkward

- ~~Forty more lines of `BridgeMailbox` + `BridgeMailboxFactory`
  boilerplate before any service code can run.~~ **Resolved in phase 047:**
  the example uses `DefaultThreadedMailboxFactory`.
- ~~The runtime + bridge wiring is verbose: build `ThreadedRuntime` →
  `register_with_capacity::<Counter, Infallible>` → `BridgeHandle::new` →
  pass `Arc` clones around.~~ **Mostly resolved in phase 047:** `BridgeHost`
  owns the runtime and registers the bridge handle in one place. There is
  still setup, but it now reads like one bridge-hosted service shape.
- ~~Shutdown of the Tina runtime requires unwrapping the `Arc` and calling
  `shutdown()` only when no clones remain.~~ **Resolved in phase 047:**
  `BridgeHost::drain_and_shutdown(...)` waits for handle clones to drain,
  returns a structured report, and leaves the host retryable on timeout.
- Both sides need a `tokio::runtime` to host axum, so the Tina side ends
  up with **two** runtimes (Tina's own thread + a Tokio current-thread
  runtime). That is the bridge's nature, but it's a real comprehension
  cost the first time you see it.
