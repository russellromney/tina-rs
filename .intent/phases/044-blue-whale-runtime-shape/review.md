# Plan Review: Blue Whale Runtime Shape

Verdict: directionally right, not ready to hand to implementation yet.

The phase is correctly placed before Baobab. Tina should build the missing
Seastar-shaped local-runtime rocks before pretending a readiness gate can prove
them. The plan also correctly refuses kernel bypass, custom TCP/IP, NUMA policy,
remoting, durable mailboxes, and broad performance claims.

What is strong:

- Feature-before-hardening order is right.
- Seastar is used as a discipline checklist, not as a false parity claim.
- The plan keeps Tina's no-`await`, explicit-effect, bounded-queue model intact.
- It names the real missing local-runtime rocks: affinity, memory posture,
  substrate boundary, and fairness.
- It requires hard tests, not documentation-only closeout.

Load-bearing fixes before implementation:

1. **Affinity is underspecified.**
   "Where the platform supports pinning safely" is too soft. Pin the expected
   direction: reporting is mandatory; hard pinning is optional and must be
   capability-reported as `Applied`, `Unsupported`, or `Failed`. If this phase
   needs a dependency such as `core_affinity`, name it now; otherwise make hard
   pinning advisory-only and do not pretend OS scheduling is controlled.

2. **Fairness overclaims what Tina can enforce.**
   Tina cannot preempt a synchronous handler that loops forever. The plan must
   say fairness is between handler turns and runtime-owned completions only.
   A "hot isolate" can be bounded if it keeps re-enqueuing work; it cannot be
   forcibly interrupted mid-handler. Required tests should prove no starvation
   for cooperative/hot-message workloads, and explicitly document the
   non-preemption rule.

3. **Scheduling groups are too big for this phase.**
   The line "service-class weights only if the simpler budget is insufficient"
   is a trapdoor. Blue Whale should pin the expected implementation to a small
   per-step/per-shard turn budget or round budget. Weighted service classes
   belong later unless a test proves the simple budget fails.

4. **Per-shard memory posture needs exact targets.**
   "Preallocation knobs" can sprawl into allocator work. Pin the first targets:
   trace buffer capacity, per-step scratch capacity, cross-shard queue
   capacities, resource table reserves, and completion storage reserves. Keep
   global allocator, user payload arenas, and durable storage buffers out of
   scope unless explicitly added.

5. **Pooling/slabbing must not touch unsafe lifetime edges casually.**
   Completion-slot pooling is scary because prior phases fixed
   backend-owned-pointer shutdown bugs. The plan should forbid pooling
   backend-owned completion slots unless the ownership proof is amended and
   reviewed. Pool safe internal Vec/table storage first; leave user payload
   pooling and boxed `Any` erasure as named costs unless there is a small,
   obviously safe patch.

6. **Driver/substrate contract scope is unclear.**
   "Make the current Betelgeuse-backed driver look like one implementation" can
   mean a public trait redesign, a crate-private cleanup, or a new crate. Pin
   expected direction: crate-private/test-visible contract first, no public
   substrate API unless the implementation discovers a real need and stops for
   review. The fake substrate proof should exercise time/TCP-ish completion,
   cancel, late completion, drain, and shutdown report, not every rail.

7. **Swappable substrate proof may duplicate existing fake driver tests.**
   `tina-runtime` already has fake driver coverage for some paths. The plan
   should require an audit of existing fake-driver tests before adding a new
   abstraction, then fill only the missing lifecycle/completion/capability
   holes.

8. **The Seastar checklist must be executable or it will become theater.**
   "Executable or review-backed" is too loose. Make it a checked artifact: a
   test or generated markdown/table must fail/change when capability truth
   changes. It should classify each item as `True`, `Partial`, `Advisory`,
   `Future`, or `NonGoal` with evidence links.

Mid-tier tightenings:

- Name the public types/fields likely to change: affinity report, preallocation
  config, fairness config, substrate lifecycle report.
- Add pause gates for dependency additions, public API changes, and any unsafe
  pooling.
- Say whether simulator gets the same fairness semantics as the live runtime.
- Require live multi-shard e2e for affinity/reporting even if hard pinning is
  unsupported on the host.
- Require allocation tests to separate setup, warm-up, steady-state, trace, and
  replay allocations.
- Require Baobab plan update at closeout if Blue Whale discovers a feature is
  advisory or future-only.

Recommended implementation shape:

1. Start with an audit of current runtime/fake-driver/fairness/allocation
   surfaces.
2. Add reporting/config types first, with no semantic behavior change.
3. Add small fairness budget between handler turns.
4. Add preallocation reserves for safe internal storage.
5. Tighten crate-private driver lifecycle contract and fake substrate proof.
6. Add optional/advisory affinity reporting, then hard pinning only if the
   dependency and platform behavior are boring.
7. Add the checked Seastar/Blue Whale principles table.
8. Run hard e2e/DST combinations and update roadmap/changelog with only landed
   truth.

After these pins, Blue Whale should be ready to execute.

# Second Hostile Review

Verdict: much closer. The plan is now mostly implementable, but still needs a
few pins before execution so the phase does not accidentally build fake
affinity, fake fairness, or unsafe-ish memory wins.

What improved:

- Affinity is no longer a silent claim; reporting is mandatory and hard pinning
  is optional.
- Fairness is correctly scoped to handler-turn boundaries, not preemption.
- Weighted service classes are no longer smuggled into the phase.
- Preallocation has named targets and clear out-of-scope allocator work.
- Backend-owned completion-slot pooling is blocked unless ownership proof is
  reopened.
- Substrate work is crate-private/test-visible first.
- The checklist is now checked, not review-only.

Remaining fixes:

1. **Fairness may already exist in the explicit-step runtime.**
   Current explicit-step delivery has registration-order/round semantics. The
   plan should let Rock 1 prove existing one-turn-per-isolate behavior is
   enough for part of the fairness claim instead of forcing a new budget knob.
   If implementation adds a knob anyway, it must explain the gap it closes.

2. **Fairness default is not pinned.**
   A new turn budget can silently change old deterministic traces. Pin the
   default before coding: recommended default is "preserve current semantics";
   add explicit config only for live/local-system pressure if a test proves the
   current behavior can starve normal work.

3. **Resource-lane fairness is separate from isolate-turn fairness.**
   The plan says "hot isolate or resource lane" but the Turn Fairness Budget
   rock mostly describes isolate turns. Driver completions, lane queues, and
   blocking lane workers need their own fairness/audit line: prove they cannot
   starve shard progress, or classify the remaining behavior as lane capacity /
   worker-held accounting rather than fairness.

4. **Observed core reporting can become fake portability.**
   `observed_core` may not be available portably. The plan should allow
   `observed_core: None` with `AdvisoryOnly`/`Unsupported` status rather than
   pressuring implementation into dubious OS calls. Worker thread id/name and
   configured shard ownership are the portable minimum.

5. **"Completion storage reserves" needs a safety qualifier.**
   This must mean runtime-owned metadata reserves, not backend-owned completion
   slot pooling. The plan should say "completion metadata reserves" to avoid
   reopening the raw-pointer lifecycle bug by accident.

6. **Checklist location and update rule are unpinned.**
   Decide whether the checked Blue Whale table lives in a Rust test,
   generated markdown, or both. Recommended: a Rust test owns the source table;
   the review/changelog summarizes it. The source table must be the thing that
   fails when a required item loses evidence.

7. **Baobab dependency is one-way but should be explicit.**
   Blue Whale closeout should not merely "update Baobab if needed"; it should
   require a Baobab plan review after Blue Whale lands, because affinity and
   fairness results directly shape what Baobab can honestly compare.

Suggested small plan edits:

- In Rock 8, add: "First prove whether current runtime round semantics already
  satisfy cooperative fairness; add config only for an identified gap."
- In Rock 8, add a line for runtime completion/lane fairness separate from
  isolate turns.
- In Rock 2, make `observed_core` optional and portable-minimum reporting
  explicit.
- In Rock 4, rename `completion storage reserves` to `completion metadata
  reserves`.
- In Rock 9, pin the checklist source to a Rust test/table.
- In Required Proof, require a Baobab plan review after closeout.

After those edits, I would call the plan ready to implement.
