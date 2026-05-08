# Rock 5 Design Note — Scatter/Gather Happy Path

## Status

Design only. **No helper.** The explicit `ScatterCoord` isolate
plus `register_reply_adapter_on` is the blessed shape.

## The Pain

`eiffel_sharded_fanout_read` is ~80 lines for "fan a typed read
out to three shards, sum the replies":

- `ScatterCoord` isolate with `Bind`, `Start`, `Reply` variants;
- `ReplyAdapter` registered on the coord shard
  (`register_reply_adapter_on` cleans this up);
- caller-owned `pending_targets` and `outcomes` accumulators;
- typed `ScatterGatherReport` returned via `stop_with`.

Rich pressure form (per-target timer, aggregate timer, partial
outcomes) needs every piece. Happy path "three shards reply,
sum" pays the same setup.

## Candidate Shapes

```rust
let waiter = scatter_gather_all(&runtime, &table, &targets, &config, make_msg, fold)?;
let report = waiter.wait(timeout)?;
```

or

```rust
let coord = ScatterCoord::register(&runtime, &table, config, on_complete)?;
runtime.try_send(coord, ScatterCoordMsg::Start)?;
```

Visible inputs that must stay user-facing: ordered targets,
collector capacity, max targets, per-target timeout, aggregate
timeout, partial-outcome policy, result cap, address-table
generation.

Visible outputs that must stay user-facing: per-target
`Replied` / `Full` / `Closed` / `Timeout` / `AggregateTimeout`
/ `MissingShard`, plus partial result.

Hard rules: no unbounded result vec, no hidden retry, no
collapsing of distinct target outcomes, owner-side
wrong-shard validation stays in the owner services.

## Why No Helper

- **Visible-input list is the helper.** Eight inputs, seven
  outputs. The signature is roughly the size of the explicit
  coord registration block today. No win.
- **Multi-shard self-address-at-registration is deferred.**
  Without it, the coord still needs a `Bind` step. A helper
  that embeds `Bind` hides a message; one that exposes it
  isn't shorter.
- **Partial-outcome policies are not yet boring.** A helper
  that returns a collapsed `Result<u64, ScatterError>` loses
  per-target outcome facts. A helper that returns the same
  typed report does what the explicit coord does.
- **Plan rule.** "If the helper takes longer to explain than
  the explicit coord, reject it." Every shape considered
  takes at least as long.

## What Could Ship Later

A narrow helper after multi-shard
`register_with_capacity_using_on` ships:

```rust
let report = ScatterCoord::register_using(
    &runtime,
    coord_shard,
    table.clone(),
    targets,
    config,
).wait(timeout)?;
```

Folds the bind-then-start handshake into the registration.
Visible inputs unchanged. Win is one line, not a framework.

Even that is optional. If the explicit coord still reads
clearly, leave it.

## Decision

No helper. `eiffel_sharded_fanout_read` already migrated to
`register_reply_adapter_on`. The remaining ceremony is part of
the typed-pressure surface and stays explicit.

Finding 2 stays open with this note: the explicit coord is the
answer until self-address-at-registration ships on the
multi-shard runtimes.
