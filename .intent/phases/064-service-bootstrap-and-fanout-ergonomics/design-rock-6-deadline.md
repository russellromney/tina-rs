# Rock 6 Design Note — Deadline Propagation

## Status

Design only. No code shipped.

## Goal

Replace per-hop "outer = inner + slack" math in
`specimen_backpressure_chain` with one absolute deadline value.
Each hop converts to its own remaining `Duration` at the call
site.

## Candidate API (Not Frozen)

```rust
pub struct Deadline { /* clock-backed instant */ }

impl Deadline {
    pub fn after(d: Duration) -> Self;
    pub fn at(/* clock instant */) -> Self;
    pub fn remaining(&self) -> Duration;          // saturating
    pub fn is_expired(&self) -> bool;
}
```

Pair with the existing call APIs:

```rust
call(addr, msg, deadline.remaining()).reply(...)
sleep(deadline.remaining()).reply(...)
```

`Deadline` produces `Duration`s. Runtime call APIs still take
`Duration`. No second timeout shape.

## Clock Decision

Wall clock and simulator virtual time are not the same source.
A `Deadline` that captures `Instant::now()` is wrong inside a
simulator.

Two options:

1. Plumb the active clock. Public `Clock` trait, `Deadline`
   built from the runtime handle. Big surface change.
2. Live-only. Use `Instant`, document as wall-clock helper.
   Simulator code keeps using `Duration` budgets.

064 does **not** choose between them. The API above is a sketch,
not a frozen public shape.

If a later phase ships `Deadline` as live-only, it must say so
in the type/docs and must not be used by simulator/replay
examples. If a later phase wants `Deadline` to participate in
DST/replay claims, it must first define the runtime/simulator
clock abstraction and build `Deadline` on that clock.

## Why Not Shipped

- The migration target threads `Duration` through messages
  today. Swapping for `Deadline` swaps one explicit value for
  another. Win is real but small.
- The simulator clock story is the load-bearing piece. Ship
  this in the phase that owns the simulator clock public API.
- Migrating now and again later means rewriting message types
  twice.

## Tests Required When Shipping

- Past deadline saturates to `Duration::ZERO`.
- zero-duration deadline is expired.
- remaining time decreases according to the chosen clock.
- Migrated `specimen_backpressure_chain` no longer threads a
  per-hop `Duration` through messages.
