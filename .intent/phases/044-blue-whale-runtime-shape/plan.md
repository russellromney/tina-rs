# Phase 044: Blue Whale Runtime Shape

## Goal

Build the missing local-runtime features before Baobab tries to harden and
compare them.

At closeout:

> Tina has a clearer Seastar-shaped local runtime core: shard/core ownership,
> per-shard memory posture, mature driver boundary, fairness rails, and a
> swappable substrate contract.

This is feature work with hard tests, not readiness theater.

## Why This Comes Before Baobab

Baobab is the hard gate. It should test a coherent thing.

If thread affinity, per-shard memory, fairness, and substrate boundaries are
still mostly gaps, Baobab can only report "missing" over and over. This phase
builds the missing rocks that make the readiness gate meaningful.

Seastar is the architectural north star:

- per-core ownership;
- explicit cross-core communication;
- no casual shared mutable service state;
- locality and preallocation matter;
- scheduler/fairness is a runtime feature, not user folklore;
- I/O substrate can evolve without changing the application model.

## Non-Goals

- No kernel bypass, DPDK, custom TCP/IP stack, or NUMA policy implementation.
- No broad performance claim.
- No remoting, clustering, placement, or distributed liveness.
- No durable mailbox.
- No flow syntax.
- No hidden fallback queues.

## Rules

- Every new feature gets positive, negative, overload, and shutdown tests.
- If a guarantee is only advisory, name it advisory.
- If a platform cannot support a capability, expose that as capability truth.
- No `unsafe` for affinity/allocation unless the plan is amended and reviewed.
- Do not weaken Tina's current bounded queue and explicit effect model.
- Pause before adding dependencies, public substrate traits, unsafe pooling, or
  service-class scheduling.
- Fairness is between handler turns and runtime completions. Tina does not
  preempt a synchronous handler that is currently running.

## Rocks

1. **Current Surface Audit**
   In `review.md`, audit current runtime/fake-driver/fairness/allocation
   surfaces before adding abstractions. List existing fake-driver tests, what
   lifecycle cases they cover, and which holes Blue Whale must fill.

2. **Shard/Core Ownership Reporting**
   Add explicit shard-to-worker/core ownership reporting. Reporting is
   mandatory. Expected shape: worker name/id, shard id, configured core,
   optional observed core when available, and affinity status. Portable minimum
   is worker thread id/name plus configured shard ownership; `observed_core`
   may be `None` when the platform cannot report it honestly. Prove shard
   identity does not drift and resource ownership stays shard-local.

3. **Optional Affinity Capability**
   Hard pinning is optional. If implemented, report
   `NotRequested | Applied | Unsupported | Failed(reason) | AdvisoryOnly`.
   If hard pinning needs a dependency such as `core_affinity`, pause first.
   If hard pinning is not boring, ship advisory reporting only and do not claim
   OS scheduling control.

4. **Per-Shard Preallocation Knobs**
   Add setup/preallocation knobs for the safe runtime-owned targets:
   trace event capacity, per-step scratch capacity, cross-shard queue capacity,
   resource table reserves, completion metadata reserves, and call-context
   reserves where practical. Separate setup, warm-up, steady-state, trace, and
   replay allocation behavior in tests.

   Out of scope unless the plan is amended: global allocators, user payload
   arenas, durable storage buffers, per-isolate custom allocators, and boxed
   `Any` elimination.

5. **Safe Pool/Slab Cleanup**
   Identify boxed/completion-slot/user-payload costs that remain. Pool or slab
   only storage with boring ownership: internal Vec/table records, resource-id
   tables, and small records that never cross backend raw-pointer boundaries.
   Do not pool Betelgeuse/backend-owned completion slots unless ownership proof
   is amended and reviewed. Leave user payload pooling and erased reply/message
   boxing as named costs unless a tiny safe patch is obvious.

6. **Driver Lifecycle Contract**
   Tighten the driver boundary so TCP/TLS/DNS/file/process/signal/persistence
   rails expose common lifecycle rules: submit, complete, cancel, drain,
   tombstone, shutdown report, capability report. Remove special cases that
   would make future substrates awkward.

   Expected direction: crate-private/test-visible contract first. No public
   substrate API and no new substrate crate unless implementation discovers a
   real need and pauses for review.

7. **Fake Substrate Proof**
   Make the current Betelgeuse-backed driver look like one implementation of a
   Tina substrate contract, not the definition of Tina. Fill gaps from the
   audit with a small fake substrate proof. It must exercise timer-ish and
   TCP-ish completion, cancel, late completion, drain, shutdown report, and
   capability truth without changing isolate semantics.

8. **Turn Fairness Budget**
   Add a minimal Tina-shaped fairness rail so one hot isolate or resource lane
   cannot silently monopolize a shard between turns. Expected implementation is
   to first prove whether current registration-order/round semantics already
   satisfy cooperative fairness. Add a new per-step/per-shard turn budget or
   round budget only for an identified gap, and preserve current semantics by
   default. No service-class weights in this phase unless a test proves the
   simple budget cannot work and the plan is amended.

   Required tests: hot self-sender does not starve quiet isolate, cross-shard
   delivery still progresses, runtime completion still delivers under hot
   mailbox pressure, simulator and live runtime agree on the fairness semantics
   they both claim, and an infinite-loop handler remains a documented
   non-preemptible user bug.

   Resource-lane fairness is audited separately from isolate-turn fairness:
   prove driver completions/lane queues cannot starve shard progress, or
   classify the behavior as lane capacity / worker-held accounting rather than
   fairness.

9. **Blue Whale Checklist**
   Add a checked checklist, not review theater. The source of truth should be a
   Rust test/table; review/changelog may summarize it but must not be the thing
   that enforces it. The table should classify each item as `True`, `Partial`,
   `Advisory`, `Future`, or `NonGoal` with evidence: per-core ownership,
   thread pinning, SPSC/cross-shard queues, bounded queues, preallocation,
   allocator locality, polling model, network/storage backend, NUMA, scheduler
   groups, DST/replay, and Tina's non-`await` user model.

10. **Combined E2E/DST Proof**
   Add e2e/DST tests that combine these rocks:
   hot isolate + normal isolate, cross-shard pressure + affinity report,
   shutdown during driver work + resource report, preallocated runtime under
   repeated workload, fake substrate parity, and fairness under seeded
   perturbation.

## Required Proof

- `make verify` passes.
- Affinity/core ownership is tested as supported or explicitly advisory.
- Preallocation tests distinguish setup allocation from steady-state behavior.
- Driver/substrate lifecycle tests cover cancel, late completion, shutdown, and
  capability truth.
- Fairness tests prove a hot cooperative workload does not starve normal work
  between handler turns, and non-preemption is documented.
- The checked Blue Whale checklist exists and fails if an item loses evidence.
- Review notes say what moved Tina closer to Seastar and what remains future.
- If Blue Whale discovers that a feature is only advisory or future-only,
  update the Baobab plan so the readiness gate does not overclaim it.
- Blue Whale closeout includes a Baobab plan review, because affinity,
  preallocation, substrate, and fairness results directly shape what Baobab can
  honestly compare.

## Done Means

- Baobab can compare a coherent local runtime rather than a pile of known gaps.
- Tina's Seastar-shaped claims are explicit: true, partial, advisory, future,
  or non-goal.
- No hidden fallback queues, hidden thread migration claims, or fake
  allocation wins.
