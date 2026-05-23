# Ergonomics Playground

A few tiny Tina service probes.

These are not product examples. They are small pressure points for API feel:

- `quote_race`: one service call races two provider isolates, accepts the first
  available quote, cancels the loser, records the late cancelled reply, and
  carries one original caller through the whole workflow. A no-winner variant
  waits for both unavailable replies before answering `Unavailable`.
- `debounced_batch`: callers submit work into a bounded pending-reply box; a
  timer flush replies to the admitted callers as one batch while excess callers
  get visible `Full`. A drain variant closes admission and replies `Closed` to
  already parked callers.
- `single_flight_cache`: several callers request the same missing key; one
  upstream fill runs, admitted waiters share the result, and overflow callers
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
- `CallGroup::start_cancelable` is the copied path now: it reserves the token,
  stores the cancel handle, and only then returns the child effect.
- `PendingReplies::drain_replies_with_into_effect` is exactly the helper a batch
  service wants at flush time.
- Single-flight cache fill is a natural Tina shape: one explicit fill call, one
  bounded `WaitList`, one flush of replies.

What felt rough:

- Race handling still has honest state: after the winner replies to the
  original caller, loser cancellation completions can arrive later and must
  still be recorded.
- A service that replies to the original caller before loser cancellations settle
  needs a little state dance: the request is gone, but the race is not complete.
- `PendingReplies` is smooth from `handle(...)` via `try_capture`; from
  `handle_call(...)`, you currently pre-check capacity and insert
  `call.into_request_context().into_deferred()` manually.
- `WaitList` removes the old qid side table for single-flight cache waiters;
  the remaining ceremony is the honest fill-in-flight state.

Verdict:

- Keep the explicit model.
- Consider a tiny "race two/all" builder later if more services repeat the
  token/insert/cancel plumbing without needing custom branch state.
- Keep `WaitList` as the keyed-waiter helper until another system proves it
  needs a higher-level `SingleFlight` wrapper.
