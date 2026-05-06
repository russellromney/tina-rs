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

# Third Hostile Review

Verdict: ready to implement.

The gruggening did not lose the load-bearing constraints. The plan is shorter,
but still pins:

- mandatory shard/core reporting;
- optional/advisory hard affinity;
- no OS scheduling claim without proof;
- no preemption of running handlers;
- current round semantics tested before adding fairness knobs;
- resource-lane fairness audited separately;
- safe-only preallocation/pooling;
- no backend-owned completion-slot pooling without amended ownership proof;
- crate-private substrate contract first;
- checked Blue Whale table as Rust test/table;
- Baobab plan review after closeout.

No new blocker found.

Implementation watchpoints:

1. **Public names can still drift.** The plan intentionally leaves exact type
   names open. During implementation, keep names boring and obvious:
   `AffinityStatus`, `ShardExecutionReport`, `PreallocationConfig`,
   `FairnessConfig`, or similarly plain shapes. Do not invent clever names.

2. **"Boring-owned" is a judgment call.** Treat it conservatively. If storage
   crosses into a backend, worker thread, raw pointer, user payload, or erased
   `Any` boundary, do not pool it in this phase unless the plan is amended.

3. **Fairness tests must avoid fake sleeps.** Use deterministic step counts,
   bounded queues, manual/scripted completions, or explicit synchronization.
   No "sleep and hope the quiet isolate ran" proof.

4. **Advisory affinity must stay visibly advisory.** A platform that cannot
   prove core pinning should still pass with honest `Unsupported` /
   `AdvisoryOnly` evidence, not fake zeroes or guessed CPU ids.

If implementation follows those watchpoints, Blue Whale is a good next rock.

# Rock 1 Audit

Verdict: starting surface is real. Blue Whale is not green-field runtime work.
It is completion work around already-visible seams.

## Runtime Surface

- `ThreadedRuntimeConfig` already names bounded ingress, shard-pair, storage,
  DNS, TLS, process, signal, trace-retention, idle-wait, and shutdown-drain
  knobs.
- `LocalSystemConfig` mirrors the user-facing bounded resource manifest and
  validates zero capacities.
- `LiveShardReport` already exposes shard id, worker name, lifecycle state,
  ingress pressure, lane capacities, trace retention, resource counts,
  worker-held resource counts, and pending driver-call counts.
- `LiveTopologyReport` already snapshots shards and source-target remote queue
  reports without probing failed workers.
- Missing for Blue Whale: configured core, optional observed core, affinity
  status, thread/core ownership vocabulary, and preallocation posture.

## Fairness Surface

- The explicit-step runtime already advances driver completions, harvests
  isolate-call timeouts, then gives each registered isolate at most one
  delivery chance in registration order per step.
- The explicit multi-shard runtime steps shards in stable shard order and
  moves next-step remote queues deterministically.
- Live shard workers run the same `Runtime::step()` loop, so isolate-turn
  fairness mostly exists for cooperative handlers.
- Suspicious gap: live `drain_remote_inbound` drains every queued remote
  envelope for each inbound source before local ingress and before `step()`.
  Under heavy cross-shard pressure, remote harvesting can monopolize a worker
  turn. Blue Whale should cap or prove this path.
- Non-gap: a synchronous handler that loops forever cannot be preempted. That
  stays a documented user bug, not a runtime promise.

## Allocation / Preallocation Surface

- `Runtime` preallocates entries, child records, supervisors, trace,
  in-flight calls, translators, pending isolate calls, round scratch, and
  driver-completion scratch with fixed initial capacities.
- Runtime round scratch has a regression test proving reserve covers more than
  the initial capacity.
- Multi-shard explicit-step runners prebuild queue/index storage and
  per-pair `VecDeque`s with shard-pair capacity.
- `BetelgeuseDriver` preallocates timers, resources, pending vectors, signal
  waits, and lane pending storage with fixed constants/capacity caps.
- Missing for Blue Whale: user-configurable safe reserves for runtime-owned
  metadata, and tests that separate setup/warm-up/steady-state/trace/replay
  allocation behavior.
- Named costs to keep honest: boxed erased messages/replies, user payloads,
  translator boxes, backend-owned completion slots, and trace/replay growth.

## Driver / Fake-Driver Surface

- `RuntimeDriver` is crate-private and already has `submit`, `advance`,
  `has_pending`, `cancel_pending`, `cancel`, `notify_signal`, and
  `resource_report`.
- `BetelgeuseDriver` already treats TCP, storage, DNS, TLS, process, signals,
  timers, and resource accounting as runtime-owned substrate work.
- Unit fake-driver coverage exists for timer-ish submission/completion,
  shutdown cancellation, per-call cancel, and driver shutdown failure.
- Driver tests cover many lane-level resource reports and late-completion /
  cancellation paths, especially after 043.
- Missing for Blue Whale: one compact fake-substrate proof that treats the
  driver contract itself as the thing under test, including cancel, late
  completion, drain, shutdown report, capability truth, and a TCP-ish
  resource-like path.

## E2E / DST Surface

- Local-system tests already cover topology, trace retention, live resource
  reports, terminal shutdown reports, live cross-shard calls, TLS/DNS/file/
  signal/process rails, and composed service workloads.
- `tina-sim` has reusable DST/randomized tests over storage, TCP cancellation,
  bridge ingress, resource rails, topology/failure, and live-vs-sim
  projections.
- Missing for Blue Whale: combined tests for hot isolate + normal isolate,
  hot remote pressure + local progress, affinity/core reporting, configured
  preallocation posture, and checked Blue Whale/Seastar status evidence.

## Implementation Direction From Audit

1. Add reporting/config fields first; do not change scheduling yet.
2. Add advisory affinity status without a new dependency unless hard pinning is
   obviously boring.
3. Add preallocation config only for runtime-owned metadata reserves.
4. Add a remote-inbound drain budget if tests prove the live gap.
5. Add fake-substrate contract tests around existing `RuntimeDriver` before
   extracting any public substrate API.
6. Add a checked Blue Whale table as a Rust test.

# Implementation Review

Verdict: Blue Whale closes on its own terms.

## Positive Review

- Shard/core ownership is now visible in topology. Each live shard reports
  worker name, worker thread id, configured core, optional observed core, and
  `AffinityStatus`.
- Affinity stays honest. The portable backend reports `NotRequested` or
  `AdvisoryOnly`; it does not claim hard OS pinning.
- Multi-shard configured core ownership is deterministic: `configured_core:
  Some(n)` assigns sorted shards to `n`, `n + 1`, ...
- `PreallocationConfig` reserves only runtime-owned metadata. It does not pool
  user payloads, erased boxes, durable buffers, or backend-owned completion
  slots.
- Live remote inbound harvest is now bounded by
  `remote_inbound_drain_budget`, so remote harvesting cannot keep looping
  forever before the shard gets a local runtime turn.
- The fake driver now has a TCP-ish pending resource proof, and its resource
  counts clear on cancel.
- Cooperative fairness is directly pinned: a self-sending hot isolate gets one
  delivery chance in a round and does not starve a quiet isolate.
- Blue Whale's Seastar-shaped claims live in
  `tina-runtime/tests/blue_whale_checklist.rs`, not only in prose.
- A combined e2e proves advisory core reporting, preallocation posture, bounded
  remote drain config, and live cross-shard isolate-call behavior together.

## Hostile Review

No P1/P2 code bugs found after implementation fixes.

Important non-claims remain visible:

- Hard OS pinning is not implemented.
- `observed_core` is `None` on the portable backend.
- No allocator-locality or NUMA claim exists.
- No user payload / erased `Any` / backend completion-slot pooling was added.
- Fairness is cooperative between handler turns. A synchronous infinite loop in
  a handler is still a user bug Tina cannot preempt.
- Resource-lane fairness remains bounded by lane capacity, worker-held
  accounting, and shutdown reporting rather than service-class scheduling.

One design wart to watch later: `configured_core: Some(n)` means "first core"
for multi-shard systems. That is simple and tested, but a later hard-pinning
phase may want an explicit shard-to-core map.

## Blast Radius

- Public config surface changed: `ThreadedRuntimeConfig` and
  `LocalSystemConfig` gained `configured_core`, `preallocation`, and
  `remote_inbound_drain_budget`.
- Public topology surface changed: `LiveShardReport` gained worker thread id,
  configured/observed core, affinity status, and preallocation reporting.
- `LocalSystemConfig::validate()` now rejects zero remote inbound drain budget.
- Live multi-shard worker scheduling changed only at the remote-harvest edge:
  a worker harvests at most the configured remote inbound budget before
  running a local step. Existing tests and `make verify` pass.
- Baobab was updated to consume Blue Whale's advisory affinity truth and avoid
  overclaiming hard OS pinning.

## Proof Run

- `cargo +nightly test -p tina-runtime blue_whale -- --nocapture`: passed.
- `make verify`: passed.
