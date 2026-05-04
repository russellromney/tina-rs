# 031 Ruud Lubbers Performance And Memory Hardening Plan

## Purpose

Make Tina's safety story cheap enough to keep using.

Willem Drees proved the local production-shaped runtime can run real
server-shaped work under bounded pressure. Ruud Lubbers should now measure and
reduce the costs of that model before Joop den Uyl makes porting easier and
before later phases add more cross-thread runtime work.

This phase is allowed to optimize broadly. It should not stop at the first
obvious hot path. But every optimization must preserve Tina's core promises:
bounded queues, visible failure, trace/replay semantics, synchronous handlers,
runtime-owned I/O, and shard ownership.

## Why This Comes Before More Cross-Thread Work

Cross-thread execution magnifies small costs:

- every boxed message or command crosses a worker boundary;
- every clone or allocation becomes more visible under transport pressure;
- every unbounded helper queue becomes a production bug;
- every trace/replay cost becomes harder to explain once multiple worker
  threads emit events.

Ruud Lubbers should make the current one-process runtime cost model explicit
and remove avoidable allocation/clone/round-count waste before the project adds
more live cross-thread substrate behavior.

## Starting Baseline

Known evidence already exists:

- `tina-mailbox-spsc` proves no per-message allocation after warm-up for the
  fixed-size SPSC hot path.
- `tina-runtime/tests/multishard_allocation.rs` pins selected allocation counts:
  multi-shard send, isolate call, Betelgeuse ingress handoff, timer, TCP read,
  TCP write.
- Ranger recorded allocation counts for several runtime paths but treated them
  mostly as honest non-claims.
- Willem Drees added a composed local-production workload with explicit
  capacities and backpressure guards, but not detailed cost accounting.

Ruud starts from those probes and turns them into a more complete cost model.

## Scope

### 1. Cost Audit

Append an implementation audit to `review.md`.

For each named path, record the current cost evidence, missing evidence, and
likely cost sources:

- SPSC mailbox send/recv;
- single-shard local send;
- multi-shard send;
- live Betelgeuse ingress handoff;
- isolate call reply/full/closed/timeout;
- timer call completion;
- TCP read/write completion;
- spawn;
- restart;
- trace/event recording;
- `Effect::Batch`;
- local production workload.

Classify costs as:

- **semantic**: needed for Tina's guarantees;
- **implementation**: likely removable without changing semantics;
- **debug/proof**: only present because trace/replay/proof is enabled;
- **user payload**: belongs to user data like `Vec<u8>`, not framework overhead.

### 2. Measurement Harness

Expand existing allocation/operation probes before optimizing.

Required measurement shapes:

- exact allocation/reallocation counts for current hot paths;
- operation/round counts where allocation is the wrong metric;
- one composed local-production cost probe;
- at least one live cross-thread/caller-thread ingress probe;
- trace growth pressure under repeated events.

Prefer deterministic counts in tests. Avoid wall-clock benchmarks unless a
number directly informs a design decision and is not used as a correctness
gate.

### 3. Broad Optimization Pass

Optimize many small costs where the code makes it safe.

Likely targets:

- avoidable `Vec` construction in effect/batch paths;
- avoidable clones in test workloads and runtime dispatch;
- repeated trace/event vector growth where capacity can be known or cheaply
  reserved;
- boxed erasure around messages, calls, completions, and commands;
- call translator storage;
- completion-slot allocation/reuse;
- per-step temporary buffers;
- cross-shard transport allocation;
- spawn/restart path allocation;
- local-production workload buffering.

The phase may introduce small internal helpers, pools, arenas, or reserve
methods if they are clearly internal and do not create a second user API.

### 4. Cross-Thread Readiness

For costs that matter more once work crosses OS threads, add direct probes or
notes:

- caller-thread ingress allocation;
- worker command allocation;
- live cross-shard transport allocation;
- payload erasure allocation;
- shutdown/cancel-drain allocation under pending work.

Do not build a new cross-thread runtime here. Do make sure the next live
cross-thread phase knows which costs are Tina-owned and which are backend-owned.

### 5. Semantic Guardrails

No optimization may:

- remove or weaken trace events to make counts prettier;
- turn bounded queues into unbounded queues;
- move I/O into handlers;
- make handlers async;
- merge source-time and destination-time cross-shard events;
- make stale-address rejection less explicit;
- make shutdown/cancellation less observable;
- hide costs in background threads or lazy global state.

If an optimization wants to weaken a guarantee, it must be rejected or recorded
as a separate future design decision with a preserved-vs-weakened table.

## Build Order

1. **Audit.** Append "Implementation Audit 1" to `review.md` with the current
   cost map and suspected removable costs.
2. **Measurement expansion.** Add missing allocation/operation probes before
   changing implementation.
3. **First broad optimization wave.** Remove low-risk allocation/clone/temp
   waste across runtime, simulator, driver, and local-production workload.
4. **Measure again.** Update review with before/after counts and explain every
   changed count.
5. **Second optimization wave.** Tackle medium-risk internal changes such as
   preallocation, pooling, or reuse if the first measurements show they matter.
6. **Cross-thread readiness check.** Record which remaining costs affect live
   worker transport and which phase should own them.
7. **Verify and review.** Run focused cost tests and `make verify`; perform an
   implementation review focused on semantic regressions disguised as speed.

## Proof Bar

This phase closes only with:

- before/after cost table in `review.md`;
- direct tests for all changed allocation counts;
- unchanged SPSC no-allocation proof;
- local-production workload still passing;
- `make verify` passing;
- no broadened performance claim without matching evidence.

## Done Means

- Tina has a current, numerical hot-path cost model.
- Avoidable framework-owned costs found by the audit are either removed or
  explicitly deferred with reason.
- The local-production workload remains semantically identical after
  optimization.
- Cross-thread future work has a clear cost map for ingress, transport, erasure,
  completion, and trace overhead.
- The project can say what Tina costs today without hand-waving.

## Refusals

- No throughput marketing benchmark as the main proof.
- No disabling trace/replay to make Tina look cheaper.
- No unbounded queues.
- No user-visible API churn unless measurement proves the existing shape blocks
  the framework's goals.
- No Tokio/Monoio/Glommio adapter implementation in this phase.
- No "unsafe for speed" change without a focused Miri/Loom/proof plan.
