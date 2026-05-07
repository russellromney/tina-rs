# Rock 6 Design Note — Deadline Propagation

## Status

Design only. No code shipped.

## Goal

Replace per-hop "outer = inner + slack" math in
`eiffel_backpressure_chain` with one absolute deadline value.
Each hop converts to its own remaining `Duration` at the call
site.

## API (Frozen Here)

```rust
pub struct Deadline { inner: Instant }

impl Deadline {
    pub fn after(d: Duration) -> Self;
    pub fn at(at: Instant) -> Self;
    pub fn remaining(&self) -> Duration;          // saturating
    pub fn is_expired(&self) -> bool;
    pub fn into_instant(self) -> Instant;
}
```

Pair with the existing call APIs:

```rust
call(addr, msg, deadline.remaining()).reply(...)
sleep(deadline.remaining()).reply(...)
```

`Deadline` produces `Duration`s. Runtime call APIs still take
`Duration`. No second timeout shape.

## Why Live-Only

Wall clock and simulator virtual time are not the same source.
A `Deadline` that captures `Instant::now()` is wrong inside a
simulator.

Two options:

1. Plumb the active clock. Public `Clock` trait, `Deadline`
   built from the runtime handle. Big surface change.
2. Live-only. Use `Instant`, document as wall-clock helper.
   Simulator code keeps using `Duration` budgets.

Option 2 is cheaper. Plan rule: live-only is allowed if the
design says so. The design says so.

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
- `Deadline::after(0).is_expired() == true`.
- `Deadline::after(50ms).remaining()` decreases over wall time.
- Migrated `eiffel_backpressure_chain` no longer threads a
  per-hop `Duration` through messages.
