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
- `CallSelectSet` is the right semantic object for a classified first-success
  race: it keeps every named branch and cancellation outcome bounded.
- `CallSelectSet::start_service` reserves the branch token, stores the cancel
  handle, and returns one typed `CallSelectEvent` continuation.
- `CallSelectSet::advance_service` applies the business-success classifier,
  validates reply/cancel tokens, and returns any bounded loser-cancellation
  work without application-owned adapter variants.
- `SharedWork::reply_all_clone` and `drain_all_with` express batch completion
  and service drain without caller ids or a sidecar correlation table.
- Single-flight cache fill is a natural Tina shape: one explicit fill call, one
  bounded `SharedWork`, one flush of replies.

What remains explicit:

- A winner can reply to the original caller before loser cancellation settles.
  The service therefore retains the bounded `CallSelectSet` until its typed
  cancel acknowledgement arrives. This is the operation lifecycle, not
  adapter plumbing or caller-authority duplication.
- A timer-backed batch is one `SharedWork<BatchId, Reply>` plus one typed
  `flow!` step. `TimerFull` remains a work failure rather than masquerading as
  application admission pressure.
- `SharedWork` removes the old qid side table for single-flight cache waiters;
  the remaining ceremony is the honest fill-in-flight state.

Verdict:

- Keep the explicit classifier and settlement model.
- Use the unified select event/start/advance path rather than reintroducing
  application-owned key, token, or cancel adapters.
- Keep `SharedWork` as the keyed-waiter helper until another system proves it
  needs a higher-level `SingleFlight` wrapper.
- Do not add a batch-specific framework abstraction yet; the shared-work form
  is already smaller and keeps batch identity, admission, and settlement named.
