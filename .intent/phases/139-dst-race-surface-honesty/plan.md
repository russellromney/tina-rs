# Phase 139: DST Race-Surface Honesty

## Status

- Planned (2026-05-24). Half honesty-doc edit, half loom/shuttle expansion.
- Closes the gap the thread-per-core review named: the deterministic oracle is
  single-threaded on purpose, so it proves **logical** interleavings, not
  **physical** memory-ordering races on the live parallel substrate — and the
  positioning ("replayable all the way down") quietly papers over that line.

## Starting Facts

- The explicit-step runtime + `tina-sim` are single-threaded and deterministic.
  They are **faithful for logical interleavings** (message delivery order, timer
  wake order, mailbox-full timing, restart races, TCP completion order) precisely
  because isolate code is shared-nothing and each shard runs sequentially. This
  single-threadedness is *why* replay works; it is a feature, not a bug.
- The live `ThreadedMultiShardRuntime` **keeps full introspection**: it merges
  each shard's trace by global event id (`threaded_multi_shard.rs:545-555`), wires
  a per-shard `TraceObserver`, and emits topology / live-shard-metrics / terminal
  shutdown reports. Introspection is **not** lost under parallelism.
- What the live parallel path does **not** give: byte-reproducible replay of the
  physical interleaving — "events across shards interleave freely"
  (`threaded_multi_shard.rs:104-106`). That is physics, not a defect.
- The physical-race surface is **small and enumerable**: the SPSC mailbox, the
  cross-shard shard-pair queue, runtime task-list / effect-batching atomics, and
  the vendored Betelgeuse internals.
- loom covers `tina-mailbox-spsc` **only** today. Runtime internals are not
  loom-covered. `ROADMAP.md` already lists "loom expansion beyond mailbox" and a
  possible `shuttle`/model-check backend as future work — this phase promotes the
  honest-doc + the first real expansion from someday to done.
- `LiveReplayCapture` / `LiveReplayFact` (`tina-sim/src/dst/replay_case.rs`) is
  the bridge that carries a live anomaly (seed/config/history/declared facts) into
  the deterministic oracle.

## Purpose

Tell the truth about what the oracle proves, and close the small physical-race
surface that the oracle cannot see.

```text
I can trust that: the sim deterministically explores logical interleavings and
replays them; the live parallel runtime is fully introspectable but not
byte-reproducible; and the handful of shared-memory structures are checked by
loom/shuttle, not hand-waved — and the docs say exactly this
```

The satisfying part: the fix *is* the Tina discipline. Shared-nothing keeps the
physical-race surface tiny; loom proves the tiny surface; everything else is
logical and lives in the deterministic oracle.

## Includes

### Honesty (docs)

- **Enumerate the shared-memory concurrency surface** in one place: SPSC mailbox,
  shard-pair queue, runtime task-list / effect-batching atomics, vendored
  Betelgeuse (named as trusted-vendored, loom-where-feasible). Short by design.
- **Draw the line explicitly:** sim = logical interleavings + replay (everything
  in handler / shard-sequential space); loom/shuttle = physical memory ordering on
  the enumerated surface; live parallel = real + introspectable + **not**
  byte-reproducible. Stop implying the sim catches data races.
- Update the over-claiming prose: the "replayable all the way down" language in
  README / the review memo / user-guide DST chapter gets the logical-vs-physical
  qualifier.
- Home for the enumeration: SYSTEM.md **if** it stays the source of truth
  (see Open Decisions), else a `docs/tina-user-guide` concurrency-surface page.
  The plan is not blocked on that call — write the section, place it per the
  decision.

### Coverage (tests)

- Extend loom to the **cross-shard shard-pair queue** and **runtime task-list /
  effect-batching atomics**. Use `shuttle` (randomized) instead where the loom
  state space explodes.
- Add these to the `make verify` gate so the enumerated surface is actually
  exercised, not just listed.

### Discipline (guard)

- A new shared atomic / lock-free structure / `unsafe impl Sync` outside the
  enumerated set must arrive **with** a loom (or shuttle) model and be added to
  the enumeration. Enforce with a CI check that flags new atomics / `UnsafeCell` /
  `unsafe impl Send|Sync` outside the listed files for review.

## Does Not Include

- A parallel deterministic oracle (impossible by definition; not a goal).
- Full bounded-exhaustive model checking (`tina-model-check`) — that stays a
  research item on `ROADMAP.md`.
- Any change to live runtime or simulator *behavior*. This phase adds tests and
  docs and a guard; it does not change semantics.
- Re-litigating that the oracle is single-threaded. It must stay single-threaded;
  that is the source of determinism.

## How We Prove The New Behavior (direct proof)

- New loom/shuttle tests for the shard-pair queue and task-list atomics pass and
  are wired into `make verify`.
- The concurrency-surface enumeration exists and matches the code (the CI guard
  fails if a new atomic appears outside the listed set without an entry).
- The over-claiming prose is corrected (doc diff).

## How We Prove We Did Not Break Old Intent (blast-radius proof)

- All existing sim/DST/replay suites pass unchanged (no behavior change).
- `LiveReplayCapture` round-trips still work — the bridge story is intact.
- Existing SPSC loom coverage still green; new coverage is additive.

## Open Decisions

- **SYSTEM.md keep / demote / drop.** It is currently fairly honest about
  non-claims (the review found it useful), but the owner says it is not actively
  maintained — and a *stale* truth-doc is worse than none. Three options:
  (a) keep it and give it this race-surface section as a renewed, load-bearing
  job; (b) demote it to an index/pointer and move truth into `docs/`; (c) drop it
  and rely on `docs/` + phase reviews. Recommendation: **(a)** — this phase gives
  SYSTEM.md exactly the kind of current-truth content it is meant to hold. Decide
  before placing the enumeration.
- loom vs shuttle per structure — decide by state-space size in implementation.

## IDD Next Step

Plan only (Session A). Next: `Plan Review 1` in
`.intent/phases/139-dst-race-surface-honesty/review.md` (resolve the SYSTEM.md
decision and loom-vs-shuttle) before any code.
