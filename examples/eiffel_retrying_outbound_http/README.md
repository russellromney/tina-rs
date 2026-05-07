# eiffel_retrying_outbound_http

Tokio-vs-Tina caller-owned retry against a flaky HTTP upstream.

The upstream returns `503` for the first two requests and `200 OK`
after that. Both sides drive the same script: try, classify, retry on
transient, give up at the budget. Both end up with
`attempts_made=3, transient_failures=2, final_ok=true`.

## Run

```sh
cargo run --manifest-path examples/eiffel_retrying_outbound_http/Cargo.toml -- both
cargo test --manifest-path examples/eiffel_retrying_outbound_http/Cargo.toml
```

Both sides:

```
side=tokio attempts_made=3 transient_failures=2 final_ok=true exit_clean=true
side=tina  attempts_made=3 transient_failures=2 final_ok=true exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape

`reqwest::Client` plus a hand-rolled `for attempt in 1..=MAX_ATTEMPTS`
loop. Per-attempt timeout via `tokio::time::timeout`, sleep between
attempts via `tokio::time::sleep`. Classification is one big match on
the response status. The whole loop fits in 25 lines of one async
function.

## Tina shape

[`tina_reqwest_bridge::ReqwestWorker`] with
[`RetryPolicy::None`](https://docs.rs/tina-reqwest-bridge) — the bridge
does *not* retry. A small `Caller` isolate owns the retry loop:

```rust
enum CallerMsg {
    Begin,
    HttpReturned(ReqwestCallOutcome),
    BackoffElapsed(SleepReply),
}
```

The handler is one match per variant:

- `Begin` and `BackoffElapsed(Ok(()))` both call `send_request(...).reply(HttpReturned)`.
- `BackoffElapsed(Err(_))` (sleep cancelled) finishes immediately —
  matched separately so `SleepReply` is bound and read deliberately.
- `HttpReturned` classifies the outcome. Transient (any `5xx`,
  bridge timeout, reqwest transport) -> `sleep(BACKOFF).reply(BackoffElapsed)`
  if the budget is left, otherwise finish.
- Everything else is fatal: finish immediately.

The host reads the final `Report` via `runtime.observe_result::<Report>(caller_addr)`
(Phase 059 Rock 1). The Caller ends with `stop_with(self.report)` —
no `mpsc`, no atomics, no shared state.

## Discussion

What feels better:

- **Retry truth is local.** The retry budget, backoff, and classifier
  all live in `Caller::absorb`. There is nothing implicit. A future
  reader can answer "what does this app do under 503?" by reading
  thirty lines.
- **Every attempt is one trace event.** `IsolateCall` and `Sleep`
  both go through the runtime; the trace records every attempt and
  every backoff. With `reqwest::Client` you read about retries from
  log strings if the user added them.
- **Two-layer outcome shape.** `ReqwestCallOutcome =
  CallOutcome<Result<ReqwestResponse, ReqwestError>>` keeps "the
  bridge could not deliver this call" (`CallOutcome::Full / Closed /
  Timeout`) distinct from "the worker accepted it and produced an
  error" (`Replied(Err(...))`). Classifying transients is one match.
  `tina-reqwest-bridge::flatten_outcome` collapses them when the app
  edge does not need the distinction; this specimen keeps them
  separate to show the layering.

What feels worse:

- **`ReqwestCallOutcome` is a long type to write at every match.**
  The shape `CallOutcome<Result<ReqwestResponse, ReqwestError>>`
  with the two layers is honest, but matching it against six arms
  reads heavier than `Result<reqwest::Response, reqwest::Error>`.
  The `ReqwestCallOutcome` alias helps; it does not erase the
  arms.
- **Retry needs three message variants.** `Begin`, `HttpReturned`,
  and `BackoffElapsed` are three explicit continuation points for
  what is two `await`s in async/await form. The `BackoffElapsed`
  variant exists only because timer wakes are messages.

What this suggests:

- The two-layer outcome is the right shape for app code that wants
  to react differently to "bridge full" vs "upstream 503". Most app
  code does not. A small "treat as transient" classifier built into
  `tina-reqwest-bridge` would let users write
  `if matches!(outcome.classify(), Class::Transient { .. }) { retry }`
  without opening every variant.
- Tina-shaped retry sugar (Eiffel finding #2 — continuation /
  pipeline sugar) would help here. The variant trio
  `Begin`/`HttpReturned`/`BackoffElapsed` is the canonical "linear
  protocol" shape; a small "for each attempt with this backoff,
  call this address, classify, finish" helper would remove two
  variants without hiding the trace.

## What this is not

The bridge ships its own [`RetryPolicy::Bounded`] for the simple
retry-on-transient case. This specimen deliberately uses
`RetryPolicy::None` so the retry shape is visible at the app
boundary. Choose the bridge's retry when the policy is fixed and
boring; choose the caller-owned shape when the classifier is
non-trivial.
