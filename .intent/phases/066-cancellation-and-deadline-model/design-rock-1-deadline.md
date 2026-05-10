# Rock 1 design note — `Deadline` value

## Status

Design only. Not shipped this phase.

## Why deferred

The plan's clock rule is the load-bearing constraint:

> Pick one before coding:
>   - live-only `Deadline`, documented as outside simulator/DST claims; or
>   - runtime/sim clock-backed deadline, with simulator parity from day one.
> Do not ship a `Deadline::after` helper that examples can copy into
> replay-claimed code while secretly depending on `std::time::Instant`.

Today neither story is fully wired. `tina-runtime`'s `Clock` trait
backs the live wall-clock path; `tina-sim` advances `virtual_now: Duration`
with its own deterministic-step machinery. A correct `Deadline` would
need either:

1. A live-only `Deadline::after(Duration)` that takes a wall-clock
   `Instant` snapshot from the runtime's `Clock`, plus loud docs that
   simulator code must not call it (or the simulator must reject it).
   Risk: an example that uses `Deadline::after` in a handler is no
   longer DST-replayable, and that fact is invisible at the call site.

2. A clock-aware `Deadline` that stores an opaque "tick" value the
   runtime stamps on construction. Requires threading the runtime
   clock through every effect that builds a deadline, and a sim-side
   parity proof. Larger surface than 066 set out to ship.

The first-form cancel + caller timeout already covers the "stop
waiting at deadline" use case — call timeout is mandatory and
deterministic in the simulator. A `Deadline` value is the helper that
makes chained timeouts (A -> B -> C with one budget) easier to write,
not the load-bearing primitive.

## Decision

Hold. The simulator-clock / DST work in `tina-sim/dst.rs` is the right
place to pin the clock story. When that lands, `Deadline` becomes a
small wrapper around the same tick source, and live-only / sim-backed
behaviors are forced to agree.

## Done means (when later phase ships)

- `Deadline::after(Duration)` returns a copy-able value;
- `remaining()` returns a `Duration` whose meaning is the same under
  live and simulator clocks;
- `expired()` is visible truth;
- examples that use `Deadline` replay deterministically under DST
  *or* the docs say "live deadline helper" and simulator-facing
  examples avoid it.
- live and sim integration tests prove the same scenario yields the
  same expired/remaining numbers in both runtimes.
