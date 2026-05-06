# Phase 044: Seastar-Core Runtime Shape

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

## Rocks

1. **Thread/Core Affinity**
   Add explicit shard-to-worker/core ownership reporting. Where the platform
   supports pinning safely, add opt-in pinning and visible success/failure.
   Where it does not, report advisory ownership only. Prove shard identity does
   not drift and resource ownership stays shard-local.

2. **Per-Shard Memory Posture**
   Add per-shard setup/preallocation knobs for runtime-owned structures where
   practical: per-step scratch, cross-shard queues, trace buffers, completion
   storage, and resource tables. Separate setup allocation from steady-state
   allocation in tests.

3. **Pool/Slab Audit**
   Identify boxed/completion-slot/user-payload costs that remain. Pool or slab
   the ones that are clearly safe and worth it. Leave the rest named in the
   cost model with a reason.

4. **Driver/Substrate Contract Maturity**
   Tighten the driver boundary so TCP/TLS/DNS/file/process/signal/persistence
   rails expose common lifecycle rules: submit, complete, cancel, drain,
   tombstone, shutdown report, capability report. Remove special cases that
   would make future substrates awkward.

5. **Swappable Substrate Boundary**
   Make the current Betelgeuse-backed driver look like one implementation of a
   Tina substrate contract, not the definition of Tina. Add compile-time or
   crate-private tests proving a small fake substrate can satisfy the same
   contract without changing isolate semantics.

6. **Scheduling/Fairness Groups**
   Add a minimal Tina-shaped fairness rail so one hot isolate or resource lane
   cannot silently monopolize a shard forever. Start small: per-step budget,
   yield/continue semantics, or service-class weights only if the simpler
   budget is insufficient. Prove no starvation in deterministic tests.

7. **Seastar Principles Checklist**
   Add an executable or review-backed checklist for:
   per-core ownership, thread pinning, SPSC/cross-shard queues, bounded queues,
   preallocation, allocator locality, polling model, network/storage backend,
   NUMA, scheduler groups, DST/replay, and Tina's non-`await` user model.

8. **Hard Proof**
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
- Fairness tests prove a hot isolate does not starve normal work.
- Review notes say what moved Tina closer to Seastar and what remains future.

## Done Means

- Baobab can compare a coherent local runtime rather than a pile of known gaps.
- Tina's Seastar-shaped claims are explicit: true, partial, advisory, future,
  or non-goal.
- No hidden fallback queues, hidden thread migration claims, or fake
  allocation wins.
