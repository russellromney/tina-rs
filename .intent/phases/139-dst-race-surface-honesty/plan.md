# Phase 139: DST Race-Surface Honesty

## Status

- Planned v3 (2026-05-25): folds in `Plan Review 1`, which found this phase's own
  enumerated surface was unverified and partly wrong, and `Plan Review 2`, which
  keeps `.intent/SYSTEM.md` load-bearing and treats `shared_scope` as
  cross-thread-capable by public type shape.
- **Honest correction:** the cross-shard transport is `std::sync::mpsc`
  (vendored stdlib, not ours to loom); there are no "runtime task-list /
  effect-batching atomics" as a shared structure; the real custom lock-free
  surface is the already-loomed SPSC mailbox plus `shared_scope.rs`, which is
  public, `Arc`-backed, and therefore must be treated as cross-thread-capable
  unless we change its type shape. This plan chooses to model it rather than
  pretend intent makes it shard-local.
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
    public and cloneable. Even if the intended service pattern is shard-local,
    the type can cross threads. **Decision:** treat it as a real shared-memory
    surface and add a loom/shuttle model for reserve/admit/release/drop/high-water
    behavior, unless the implementation first makes the type non-cross-thread at
    compile time.
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

- **Enumerate the verified shared-memory concurrency surface** in one place:
  SPSC mailbox (custom lock-free, already loomed), `shared_scope.rs`
  (`SharedCapacityScope`, modeled in this phase), standard-library cross-shard
  channels (named as stdlib/trusted, not loom-targeted), and vendored Betelgeuse
  internals (trusted-vendored, loom-where-feasible upstream). Do **not** list the
  old false targets ("runtime task-list" / "effect-batching atomics").
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
- Home for the enumeration: **keep and renew `.intent/SYSTEM.md`** as the
  internal truth contract, then add a short user-guide pointer for humans. A
  stale truth doc is bad; the fix is to make this section load-bearing and guard
  it, not to scatter the truth.

### Coverage (tests) — collapses to verification + one decision

- **Surface inventory artifact.** Add/update a checked file that records the
  verified custom shared-memory surface: SPSC mailbox (loomed), `shared_scope`,
  standard-library channels, vendored Betelgeuse internals, and ordinary
  shard-local atomic counters. The expected surface is named here; if code grep
  finds a different surface, stop and update the plan before changing semantics.
- **Model `shared_scope`:** add a loom (or `shuttle`, if the state space is
  large) model for `SharedCapacityScope` reserve/admit/release/drop/high-water
  behavior. Do not "fix" this by changing the public type shape in this phase;
  shared scopes are public and cloneable today, so prove the existing shape.
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

- Surface verification is recorded, and `shared_scope` is modeled with
  loom/shuttle.
- The concurrency-surface enumeration exists and matches the code; the allowlist
  CI guard fails if a new synchronization primitive appears off-list.
- The over-claiming prose is corrected (doc diff), with the faithfulness claim
  conditioned on the verified surface.

## How We Prove We Did Not Break Old Intent (blast-radius proof)

- All existing sim/DST/replay suites pass unchanged (no behavior change).
- `LiveReplayCapture` round-trips still work — the bridge story is intact.
- Existing SPSC loom coverage still green; new coverage is additive.

## Decisions

- **Keep and renew `.intent/SYSTEM.md`.** It stays the internal shape-protection
  contract. This phase gives it a load-bearing race-surface section and a CI guard
  so it cannot silently rot.
- **`shared_scope` is treated as cross-thread-capable.** Model it. No public type
  shape change in this phase.
- loom vs shuttle per structure is an implementation choice by state-space size;
  the required outcome is a checked model or compile-fail non-cross-thread proof.

## IDD Next Step

Plan v3 (Session A): Plan Review 1 folded in, then second-review fixes applied:
surface corrected, `.intent/SYSTEM.md` kept/renewed, `shared_scope` treated as
cross-thread-capable, guard defined as allowlist-diff. Begin only on go.
