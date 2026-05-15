# Ergonomics Playground

Two tiny Tina service probes.

These are not product examples. They are small pressure points for API feel:

- `quote_race`: one service call races two provider isolates, accepts the first
  available quote, cancels the loser, and carries one original caller through
  the whole workflow.
- `debounced_batch`: callers submit work into a bounded pending-reply box; a
  timer flush replies to the admitted callers as one batch while excess callers
  get visible `Full`.

## Run

```sh
cargo test --manifest-path examples/systems/ergonomics_playground/Cargo.toml
```

## Findings

What felt good:

- `RequestContext` makes the original caller authority visible without making it
  ambient.
- `CallGroup` is the right semantic object for first-success races: it keeps
  winner, losers, and cancel outcomes named.
- `PendingReplies::drain_replies_with_into_effect` is exactly the helper a batch
  service wants at flush time.

What felt rough:

- Starting a two-branch race still has noticeable ceremony: reserve token,
  build cancelable call, insert handle, route token back in the continuation.
- A service that replies to the original caller before loser cancellations settle
  needs a little state dance: the request is gone, but the race is not complete.
- `PendingReplies` is smooth from `handle(...)` via `try_capture`; from
  `handle_call(...)`, you currently pre-check capacity and insert
  `call.into_request_context().into_deferred()` manually.

Verdict:

- Keep the explicit model.
- Consider a tiny "race two/all" builder later if more services repeat the
  token/insert/cancel plumbing without needing custom branch state.
- Consider a `PendingReplies::try_capture_call(call_ctx, key, full_reply)` style
  helper only if `handle_call(...)` pending boxes become common.

