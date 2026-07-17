# Scoped Request Tree

The small specimen for request-scoped cancellation. One HTTP request is one
request tree; when the caller goes away, the tree stops waiting.

## Shape

One route, `POST /upload`, with a streaming request body. Each request owns:

- a `RequestScope` held in a bounded `RequestScopeSet` (capacity from a const,
  one scope per concurrent upload);
- a request-deadline `ScopedTimer` (a tombstone timer, because plain `sleep`
  is not `CallHandle`-cancelable);
- one cancelable "enrich" child registered into the scope;
- one `ScopedRequestReport` per torn-down request.

```text
POST /upload (streaming body)
  -> RequestScope + RequestScopeSet entry
  -> arm ScopedTimer (deadline) + sleep
  -> cancelable enrich child registered in scope
  -> pull body chunks
clean body  -> release enrich -> 200, tombstone timer, retire scope
short body  -> client disconnect -> cancel scope (ClientDisconnect)
            -> enrich child wait closes (ack Cancelled)
            -> timer tombstoned; later physical fire is IgnoredLate
            -> ScopedRequestReport: cause, cancelled children, capacity 0
```

## Honesty

- No fake cancellation. The enrich child's wait really closes; its async
  cancel ack is `CancelOutcome::Cancelled`.
- The deadline timer is not physically cancelled — it is tombstoned. When the
  real sleep fires after the request is gone, the continuation observes
  `ScopedTimerFire::IgnoredLate` and skips the user work. The ignored count
  is the visible truth.
- A short body (declared length not delivered) is the disconnect signal: the
  streaming pull reports `Eof` with fewer bytes than declared.
- Sim/replay agreement: the request-scope-set capacity surface is captured as
  a live fact and round-tripped through `tina_sim::dst::check_captured_replay`,
  which fails closed on divergence.

## Run

Public runner (LocalSystem host, typed split-service HTTP handle, actor-owned
terminal report via `stop_with` / `observe_result`):

```sh
cargo test --manifest-path examples/systems/system_scoped_request_tree/Cargo.toml --test public_smoke public_smoke -- --exact
```

Focused smoke:

```sh
cargo test --manifest-path examples/systems/system_scoped_request_tree/Cargo.toml --test smoke
```

## Findings

What felt good:
- `RequestScope::cancel_into_effect` + `ScopedRequestReport` made the teardown
  one typed value: cause, cancelled-vs-settled children, reclaimed capacity.
- `ScopedTimerSet` made the "physical timer fired late" case honest and small
  — no pretending the sleep was un-fired.
- Detecting client disconnect from a short streaming body read needed no new
  rail; the existing pull outcome carried the truth.

What felt rough:
- The body-pull loop is the request's own control flow; registering every
  chunk pull into the scope would accumulate settled children, so the scope
  holds the long-lived children (the enrich child) and the pull stays the
  detector. The per-pull cancel is proven separately in the `tina_http::scope`
  adapter tests.

Tina capability pulled:
- `RequestScope`, `RequestScopeSet`, `ScopedRequestReport`, `ScopedTimerSet`,
  native streaming HTTP, `tina_sim::dst` live-replay capture.

Verdict:
- keep
