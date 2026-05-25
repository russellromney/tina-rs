# Phase 139 Review (append-only)

## Plan Review 1 — hostile (2026-05-24)

Verdict: the honesty half is sound; the **coverage half is built on an
unverified surface claim — which is exactly the sin this phase exists to fix.**
The plan must verify its own central fact before it is implementable.

### Finding 1 (blocking) — the enumerated race surface is asserted, not verified, and is partly wrong

The plan lists the physical-race surface as "SPSC mailbox, the cross-shard
shard-pair queue, runtime task-list / effect-batching atomics, vendored
Betelgeuse." Checked on main:

- **Cross-shard transport is `std::sync::mpsc::SyncSender`/`Receiver`**
  (`threaded_multi_shard.rs:59,67,180,213`). That is the **standard library**,
  not custom lock-free code. "Extend loom to the shard-pair queue" largely means
  loom-ing std channels — not ours to verify, and loom can't meaningfully explore
  them.
- **"runtime task-list / effect-batching atomics" do not exist** as a shared
  structure. The explicit-step runtime is single-threaded; the threaded runtime
  is one-shard-per-thread (shard-local). The `AtomicU64`s that exist
  (`wait_list.rs`, `pool.rs`, `deferred.rs`, `admission.rs`, `call/groups.rs`,
  `guarded_pending.rs`, `local_permit.rs`, `persistence.rs`) are **id/capacity
  counters inside shard-local helper types**, not lock-free concurrency.
- **Global event ids are not a shared atomic.** Live multi-shard merges per-shard
  traces by sort (`trace()` sort_by_key); the explicit-step path uses a
  `MonotonicClock`. No cross-thread event-id atomic to model.
- **The one genuine open question is `shared_scope.rs`** (`SharedScopeInner`
  behind `Arc` with `AtomicU64`/`AtomicUsize`). It is `Arc`-shareable but
  SYSTEM.md calls `SharedCapacityScope` *shard-local*. So either it is shard-local
  and those atomics are **removable defensiveness**, or it is genuinely
  cross-thread and is a **real loom candidate**. The phase must determine which.

**Required plan change:** Step 1 of this phase is "grep and confirm the actual
shared-memory surface," not "extend loom to a presumed list." The likely true
surface is: SPSC mailbox (already loomed) + std mpsc channels (vendored stdlib,
not ours) + the `shared_scope` question. If that holds, the "Coverage (tests)"
half **collapses to**: confirm SPSC is the only custom lock-free structure, and
resolve whether `shared_scope`'s atomics are load-bearing (loom them) or
shard-local (remove them). The phase becomes ~80% honesty-doc + discipline-guard.

### Finding 2 — the discipline guard is under-defined and may be unimplementable as written

"CI check that flags new atomics / `UnsafeCell` / `unsafe impl Send|Sync`." But
`AtomicU64` is used legitimately for ~9 shard-local counters today; a guard that
flags all atomics is pure noise. The guard needs a precise definition of
"shared-memory concurrency primitive" vs "ordinary counter," or it becomes a
disabled lint. **Required plan change:** define the guard as an allowlist-diff
(maintained list of the enumerated structures + their files; CI fails when a
`new` atomic/UnsafeCell/`unsafe impl Sync` appears in a file not on the list),
and seed the allowlist from Finding 1's verified surface. Mark it `surrogate
proof` honestly — it catches additions, it does not prove the existing set is
race-free.

### Finding 3 — "How could this be broken while tests pass?" on the honesty doc

The doc claims sim is faithful for logical interleavings "because isolate code is
shared-nothing and each shard runs sequentially." That is true only if no helper
type smuggles cross-shard shared state. The `shared_scope` Arc is the live
counter-example to investigate. The doc must state the faithfulness claim
**conditioned** on the verified surface, or it is the same hand-wave it sets out
to remove.

### Keep

The logical-vs-physical framing, the LiveReplayCapture bridge, and the refusal to
build a parallel oracle are all correct and stay.
