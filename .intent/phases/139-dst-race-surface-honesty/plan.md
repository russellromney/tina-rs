# Phase 139: DST Race-Surface Honesty

## Status

- Planned v2 (2026-05-24): folds in `Plan Review 1`, which found this phase's own
  enumerated surface was unverified and partly wrong. v2 makes "verify the surface
  first" Step 1, and the coverage half collapses accordingly (see below).
- **Honest correction:** the cross-shard transport is `std::sync::mpsc`
  (vendored stdlib, not ours to loom); there are no "runtime task-list /
  effect-batching atomics" as a shared structure; the real custom lock-free
  surface is likely just the already-loomed SPSC mailbox, with `shared_scope.rs`
  as the one genuine open question.
- **Verified against `origin/main` a6cbaa9:** live multi-shard `trace()` merges +
  sorts by event id (`threaded_multi_shard.rs:570-580`), "events across shards
  interleave freely" comment still at `:106`, loom still only on
  `tina-mailbox-spsc`, `LiveReplayCapture`/`LiveReplayFact` present
  (`tina-sim/src/dst/replay_case.rs`). The cross-shard-child-ownership work (#199)
  did not change the introspection/interleaving story. Premise holds.
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
- The physical-race surface is **small** — but the v1 enumeration was wrong and
  must be verified, not asserted (Step 1 of this phase). What main actually shows:
  - **SPSC mailbox** — custom lock-free, **already loom-covered**.
  - **Cross-shard transport = `std::sync::mpsc::SyncSender`/`Receiver`**
    (`threaded_multi_shard.rs:59,67,213`) — standard library. Not ours to loom;
    loom cannot meaningfully explore it.
  - **No "task-list / effect-batching atomics" shared structure exists.** The
    `AtomicU64`s on main (`wait_list`, `pool`, `deferred`, `admission`,
    `call/groups`, `guarded_pending`, `local_permit`, `persistence`) are
    shard-local id/capacity counters, not lock-free concurrency.
  - **Global event ids are not a shared atomic** — merged by sort across shards.
  - **`shared_scope.rs`** (`Arc<SharedScopeInner>` + `AtomicU64`/`AtomicUsize`) is
    the **one open question**: SYSTEM.md calls `SharedCapacityScope` *shard-local*,
    so its atomics are either removable shard-local defensiveness or a genuine
    cross-thread loom candidate. The phase must determine which.
- loom covers `tina-mailbox-spsc` **only** today. Given the above, "loom expansion
  beyond mailbox" likely has **little real target** — the honest outcome may be
  "SPSC is the only custom lock-free structure; everything else is stdlib or
  shard-local." That is a result to confirm and write down, not a gap to fill.
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
- **State the faithfulness claim conditioned on the verified surface.** "Sim is
  faithful for logical interleavings *because* isolate code is shared-nothing and
  each shard runs sequentially" holds only if no helper smuggles cross-thread
  shared state — the `shared_scope` question is the live test of that. Write the
  claim with that condition, or it is the same hand-wave this phase removes.
- Update the over-claiming prose: the "replayable all the way down" language in
  README / the review memo / user-guide DST chapter gets the logical-vs-physical
  qualifier.
- Home for the enumeration: SYSTEM.md **if** it stays the source of truth
  (see Open Decisions), else a `docs/tina-user-guide` concurrency-surface page.
  The plan is not blocked on that call — write the section, place it per the
  decision.

### Coverage (tests) — collapses to verification + one decision

- **Step 1 (do this first): verify the surface.** Grep the workspace for custom
  lock-free code (`UnsafeCell`, `unsafe impl Send|Sync`, hand-rolled atomics used
  as synchronization rather than counters). The expected result is: SPSC mailbox
  (loomed) + stdlib channels + the `shared_scope` question.
- **Resolve `shared_scope`:** confirm whether `SharedCapacityScope` is ever
  touched from more than one thread. If shard-local (one shard thread), its
  atomics are removable defensiveness — record that. If genuinely cross-thread,
  add a loom (or `shuttle`, if the state space is large) model for it.
- **Do not loom stdlib channels** (the cross-shard transport). Not ours to verify.
- Wire whatever real new model results into `make verify`. If the honest finding
  is "nothing new to loom," that conclusion is the deliverable — write it down.

### Discipline (guard) — defined precisely so it is implementable

- A maintained **allowlist file** names the verified shared-memory structures and
  their files (seeded from Step 1). A CI check fails when a **new** `UnsafeCell`,
  `unsafe impl Send|Sync`, or atomic-used-as-synchronization appears in a file not
  on the allowlist, forcing review + a model before it lands.
- This is `surrogate proof`: it catches *additions* to the surface. It does not
  prove the existing set race-free — that is what the per-structure loom models
  do. Ordinary atomic counters are explicitly out of scope (allowlist the files
  that legitimately hold them) so the guard is signal, not noise.

## Does Not Include

- A parallel deterministic oracle (impossible by definition; not a goal).
- Full bounded-exhaustive model checking (`tina-model-check`) — that stays a
  research item on `ROADMAP.md`.
- Any change to live runtime or simulator *behavior*. This phase adds tests and
  docs and a guard; it does not change semantics.
- Re-litigating that the oracle is single-threaded. It must stay single-threaded;
  that is the source of determinism.

## How We Prove The New Behavior (direct proof)

- Step 1 surface verification is recorded (the grep + its conclusion), and the
  `shared_scope` question is resolved one way or the other (atomics removed as
  shard-local, **or** a loom/shuttle model added).
- The concurrency-surface enumeration exists and matches the code; the allowlist
  CI guard fails if a new synchronization primitive appears off-list.
- The over-claiming prose is corrected (doc diff), with the faithfulness claim
  conditioned on the verified surface.

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

Plan v2 (Session A): Plan Review 1 folded in (surface corrected, coverage
collapsed to verify-first, guard defined as allowlist-diff). Remaining open:
the SYSTEM.md keep/demote/drop decision (Open Decisions) and the `shared_scope`
cross-thread question (Step 1). Begin only on go.
