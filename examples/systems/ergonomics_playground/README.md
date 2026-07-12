# Ergonomics Playground

A few tiny Tina service probes.

These are not product examples. They are small pressure points for API feel:

- `quote_race`: one service call races two provider isolates, accepts the first
  available quote, cancels the loser, records the late cancelled reply, and
  carries one original caller through the whole workflow. A no-winner variant
  waits for both unavailable replies before answering `Unavailable`.
- `debounced_batch`: callers join one bounded `SharedWork` batch; a
  timer flush replies to the admitted callers as one batch while excess callers
  get visible `Full`. Operation admission is capped separately from live waiter
  occupancy, so timed-out callers cannot make a batch grow past its bound. A
  drain variant closes admission and replies `Closed` to already parked callers.
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
- `SharedWork::reply_all_clone` and `drain_all_with` express batch completion
  and service drain without caller ids or a sidecar correlation table.
- Single-flight cache fill is a natural Tina shape: one explicit fill call, one
  bounded `SharedWork`, one flush of replies.

What felt rough:

- Race handling still has honest state: after the winner replies to the
  original caller, loser cancellation completions can arrive later and must
  still be recorded.
- A service that replies to the original caller before loser cancellations settle
  needs a little state dance: the request is gone, but the race is not complete.
- A timer-backed batch is one `SharedWork<BatchId, Reply>` plus one typed
  `flow!` step. `TimerFull` remains a work failure rather than masquerading as
  application admission pressure.
- `SharedWork` removes the old qid side table for single-flight cache waiters;
  the remaining ceremony is the honest fill-in-flight state.

Verdict:

- Keep the explicit model.
- Consider a tiny "race two/all" builder later if more services repeat the
  token/insert/cancel plumbing without needing custom branch state.
- Keep `SharedWork` as the keyed-waiter helper until another system proves it
  needs a higher-level `SingleFlight` wrapper.
- Do not add a batch-specific framework abstraction yet; the shared-work form
  is already smaller and keeps batch identity, admission, and settlement named.
