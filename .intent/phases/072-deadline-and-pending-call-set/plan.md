# 072 Deadline And Pending Call Set

## Status

- Done: design drafted from 066 first-form cancellation experience plus Eiffel
  workflow ergonomics. Ring-based late-reply classification (066) and
  caller-owned `CallHandle` (066) are the load-bearing primitives this phase
  builds on.
- In progress: blocked on this phase to land before 067-pool waiter
  cancellation can reuse the deadline shape.
- Open: implement `Deadline` value (Rock 1 from 066) and bounded
  `PendingCallSet` helper (Rock 4 from 066).
- Deferred: deadline propagation into resource drivers, deadline-aware
  `select`-shaped helpers, automatic deadline arithmetic on retries,
  workflow macros that hide either primitive.

## Goal

066 shipped the first cancellation primitive. Two pieces of vocabulary were
deferred from that phase because they are ergonomics on top of the primitive,
not new semantics. They are this phase.

`Deadline` makes a call's mandatory timeout easy to thread through a chain of
isolates without re-doing timeout arithmetic at every hop. `PendingCallSet`
is the small bounded table every cancellable workflow keeps next to its
state — the alternative is hand-rolling `BTreeMap<RequestId, CallHandle>`
and forgetting to clean up.

Core rule:

```text
Deadline names when. PendingCallSet names what.
Neither hides why a wait closed.
```

This phase comes before any workflow macro work and before 067-pool
deadline integration. Pools that wait on a deadline need a deadline value
that can be passed by value, not reconstructed from a `Duration` and a
`now()` at every layer.

Compiler rule:

```text
If compiler can know wrong, make wrong not compile.
If only runtime can know wrong, make typed outcome plus trace fact.
```

Grug rules:

```text
deadline is wall time, not magic.
deadline expired, deadline say so.
many handle, many slot.
slot full, slot say so.
slot remove on done. slot remove on cancel. slot remove on timeout.
slot never remove by magic.
```

## Non-Goals

- No new cancellation cause. The existing `CancelCause::CallerTimedOut`
  produced by 066's timeout path is the only deadline cause.
- No deadline-aware `select`. Save for a later helper phase.
- No automatic deadline propagation into resource drivers. A driver that
  honors a deadline does so by reading `Deadline::remaining()` itself.
- No retry policy. Deadlines bound a wait; retries on top of a deadline are
  the caller's policy.
- No `PendingCallSet::cancel_one_by` predicate-style helpers in first form.
  Workflows that need that compose `iter` + `remove` + `cancel_call`.
- No hidden `Drop`-based cleanup of `PendingCallSet` entries. Cleanup is
  always explicit, like every other tina primitive.
- No `Deadline::after` shorthand that secretly reads `std::time::Instant`
  in DST-claimed code. See the clock rule below.

## Vocabulary

```rust
/// Absolute deadline. Same `Instant` semantics as the runtime/sim clock
/// the constructor was issued from.
#[derive(Clone, Copy, Debug)]
pub struct Deadline { /* opaque */ }

impl Deadline {
    /// Build a deadline `duration` from `now`. The `now` argument is
    /// taken explicitly so DST-claimed code does not silently depend on
    /// `std::time::Instant`. See `Context::deadline_after` for the
    /// runtime-aware sugar that reads the runtime/sim clock.
    pub fn from_instant(now: Instant, duration: Duration) -> Self;

    /// Returns the deadline's absolute instant.
    pub fn at(self) -> Instant;

    /// Returns `now`-relative remaining time. Saturates at zero.
    pub fn remaining_or_zero(self, now: Instant) -> Duration;

    /// Returns whether the deadline has passed.
    pub fn expired(self, now: Instant) -> bool;
}
```

```rust
/// Bounded table of `(RequestId, CallHandle<R>)`. One reply type per
/// instance; mixed-reply workflows compose two `PendingCallSet`s.
pub struct PendingCallSet<I: Eq + Hash, R> { /* opaque */ }

#[must_use]
pub enum InsertOutcome { Inserted, DuplicateKey, Full }

impl<I, R> PendingCallSet<I, R>
where
    I: Eq + Hash + Copy,
{
    pub fn with_capacity(capacity: usize) -> Self;
    pub fn len(&self) -> usize;
    pub fn capacity(&self) -> usize;
    pub fn is_empty(&self) -> bool;

    /// Insert a fresh `(id, handle)`. `DuplicateKey` and `Full` are
    /// typed errors, not panics.
    pub fn insert(&mut self, id: I, handle: CallHandle<R>) -> InsertOutcome;

    /// Pull a handle out by id. Returns `None` if absent.
    pub fn remove(&mut self, id: &I) -> Option<CallHandle<R>>;

    /// Iterate the keys without surrendering the handles. Useful for
    /// shutdown sweeps that need to compose effects per id.
    pub fn ids(&self) -> impl Iterator<Item = &I>;

    /// Build cancel effects for every stored handle, draining the set.
    /// Returns one `Effect<Iso>` per handle; the caller batches.
    pub fn drain_cancel<Iso, F>(&mut self, translator: F) -> Vec<Effect<Iso>>
    where
        F: Fn(I, CancelOutcome) -> Iso::Message + Clone + 'static,
        Iso: Isolate;
}
```

`Deadline` is `Copy` so it can be passed alongside an `Address<_, R>` into
nested calls without lifetime gymnastics. `PendingCallSet` is `!Clone` and
move-only inside isolate state, same shape as the underlying `CallHandle`.

## Rock 1: Deadline Value

Add a tiny absolute-time helper. Used by every isolate that fans out and
wants to share "we have N ms total" across hops without re-doing math.

Candidate API is in the Vocabulary section above.

Rules:

- absolute deadline, not retry policy or relative timeout;
- no hidden cancellation — `Deadline` does not own a `CallHandle`;
- no hidden retry — `Deadline` does not retry on expiry;
- expired deadline is visible through `expired()`, not silent;
- live/sim clock truth must be named at the call site.

Clock rule (settled this phase, not deferred):

- `Deadline` itself is clock-agnostic. It stores an `Instant` and a
  remaining duration; arithmetic uses whatever `Instant` the caller passes.
- `Deadline::from_instant(now, dur)` takes `now` explicitly so a
  DST-claimed call site cannot silently depend on `std::time::Instant`.
- `Context::deadline_after(duration) -> Deadline` is the runtime-aware
  sugar; it reads the live runtime clock or the simulator clock that the
  current handler runs under, so DST replay produces identical deadlines.
- A live-only convenience `Deadline::after(duration)` is **not** shipped
  this phase. Earlier 066 design considered it; we drop it to avoid
  examples copying a non-DST helper into replay-claimed code.

Proof:

- `Deadline::from_instant(now, dur).remaining_or_zero(now + dur/2)` is
  `dur/2`;
- `Deadline::from_instant(now, dur).expired(now + dur)` is `true`;
- `Deadline::from_instant(now, dur).expired(now + 2*dur)` is `true`;
- a deadline minted on the simulator clock at virtual `t = 0` and
  inspected at virtual `t = dur` is `expired`;
- a chain of three isolates can pass one `Deadline` through messages
  without recomputing timeout math at each hop;
- DST replay test: same scenario two simulator runs, deadline expiry
  fires at the same trace event in both.

## Rock 2: Bounded PendingCallSet

The blessed bounded table for cancellable workflows. Backed by a fixed-
capacity `HashMap`-shaped slab so insert/remove are O(1) and the storage
will not grow under load.

Rules:

- bounded storage; full table is a typed `InsertOutcome::Full`, not a
  panic and not an unbounded `HashMap`;
- duplicate key is `InsertOutcome::Duplicate`, not "newer wins";
- remove on completion / cancel / timeout / owner-stop is **explicit**.
  Every continuation that handles a `CallOutcome` for an entry must
  remove its key. The set will not auto-clean.
- `drain_cancel` produces visible cancel effects/outcomes for each
  stored handle; calling it on shutdown is the canonical owner-stop
  pattern;
- the helper does not own the workflow. It owns the table only.

Storage shape:

- a fixed-capacity slab indexed by a small `u32` slot, plus a
  `HashMap<I, u32>` for id lookup. Both are bounded by `capacity()`.
- no `Box<dyn Trait>`; the slab stores typed handles directly.
- `Drop` of `PendingCallSet` does NOT cancel held handles. That is
  caller-explicit via `drain_cancel` because cancel produces effects
  the runtime needs to enqueue, and `Drop` cannot return effects.

Proof:

- `PendingCallSet::with_capacity(N)` rejects the `(N+1)`th insert with
  `Full` and the existing `N` are still callable;
- duplicate key returns `DuplicateKey` and the prior handle is
  unchanged;
- `remove(&id)` after completion returns `Some(handle)` once and
  `None` thereafter;
- a workflow that inserts on every fan-out and removes on every
  `Returned` continuation drains to empty after all calls settle;
- `drain_cancel` builds N effects for N held handles and the set is
  empty afterward;
- fill-then-drain-then-fill: filling, draining, then filling again
  to capacity returns `Inserted` on every entry — the slab reclaimed;
- DST replay: same workflow on two simulator runs produces byte-
  identical event records including the order of `drain_cancel`
  output.

## Order

1. `Deadline` value + runtime/sim clock sugar (`Context::deadline_after`).
2. `PendingCallSet` helper backed by a slab.
3. Update `examples/eiffel_cancellation_chain` to demonstrate both:
   the driver passes one `Deadline` to each worker and stores handles
   in a `PendingCallSet`. Owner-stop drains and cancels via the set.
4. Update `examples/FINDINGS.md` with one paragraph per primitive
   describing the gap each closed.

No new specimen unless an existing one cannot show the new model.

## Done Means

- `Deadline` is documented, tested, and used by at least the
  `eiffel_cancellation_chain` example;
- `PendingCallSet::insert`, `remove`, `drain_cancel`, and `Full` /
  `Duplicate` paths are tested in both the threaded runtime and the
  simulator;
- DST replay holds: the same scenario on two simulator runs produces
  byte-identical event records;
- `examples/FINDINGS.md` records the deadline + pending-set findings;
- 067-bounded-pool-vocabulary's pool-waiter cancellation can express
  its deadline argument as a `Deadline` instead of a relative
  `Duration`.

## Why this is its own phase, not part of 067

067 (bounded pools) needs a deadline shape but cannot define one without
front-running the cancellation vocabulary. 066 already defined the
cancellation cause; this phase finishes the deadline vocabulary so 067
can reach for `Deadline` instead of inventing a third shape. Splitting
keeps each phase reviewable in isolation and avoids bundling pool
mechanics with deadline ergonomics.
