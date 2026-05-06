# Phase 045: Portable Local Runtime Completion

## Goal

Make the current non-`io_uring` local runtime feel like a complete Tina runtime
before Baobab judges it.

Baobab should test a real thing. This phase builds the remaining portable
runtime rocks: driver lifecycle, resource truth, fairness, capability truth,
blocking-lane honesty, trace/report survival, local-service e2e, DST pressure,
cost reporting, and CI rails.

At closeout:

> Tina's portable local runtime can run serious local service-shaped workloads
> with visible overload, traceable failure, replayable races, bounded live
> rails, and honest non-claims.

## Non-Goals

- No `io_uring`, kernel bypass, DPDK, custom TCP stack, or hard OS pinning.
- No broad performance claim.
- No remoting, clustering, membership, placement, or durable mailbox.
- No general Tower/Axum middleware inside Tina.
- No `flow!` syntax or macro expansion project.
- No hidden fallback queues.

## Rules

- If something can overload, a test observes pressure or `Full`.
- If something can fail, a test observes typed failure and trace.
- If something can race, DST or deterministic e2e replays it.
- If work is blocking, bounded lane capacity and shutdown truth must expose it.
- If the portable backend cannot support a capability, report that plainly.
- Tests must cover positive, negative, overload, shutdown, and weird paths.
- No sleeps-as-proof. Use bounded queues, scripted completions, barriers,
  deterministic seeds, or explicit synchronization.

## Rocks

1. **Driver Lifecycle Contract Cleanup**
   Audit TCP, TLS, DNS, UDP, file/path, process, signal, persistence, timers,
   and isolate calls. For each rail, pin the same lifecycle vocabulary:
   submit, accepted, full, timeout, cancel, late completion, close, drain,
   shutdown report, and capability report. Remove special-case behavior where a
   common helper or common event shape is clearly safer.

2. **Resource Ownership Inventory**
   Make topology and terminal reports trustworthy for the portable backend.
   Owned resources, worker-held resources, pending driver calls, lane capacity,
   lane pressure, and failed-shard state must survive ugly shutdown. If a count
   is table-owned only or worker-held only, name that truth.

3. **Fairness Completion**
   Prove local ingress, remote inbound, hot mailboxes, driver completions, and
   blocking lanes all get turns under cooperative workloads. Add the smallest
   budget only for a proven starvation path. Do not claim preemption of a
   currently running synchronous handler.

4. **Portable Capability Truth**
   Add one executable portable-backend capability table. It must cover timers,
   TCP, TLS, DNS, UDP, files, process, signals, persistence, calls, cross-shard
   sends, bridge ingress, cancellation, shutdown, backpressure, replay, and
   known non-claims. Review and docs may summarize it, but the Rust test/table
   is the source of truth.

5. **Blocking-Lane Honesty**
   Storage, DNS, TLS, process, and persistence lanes may use blocking work in
   this backend. Prove lane full, queued cancellation, started-work
   tombstoning, late completion swallowing, shutdown drain timeout, and terminal
   resource reporting. Make the capability table say "lane-backed blocking"
   where that is the truth.

6. **Trace And Report Survival**
   When failure happens, observation must not fail worse. Strengthen
   `trace()`, `complete_trace()`, topology, terminal report, shutdown report,
   and bridge metrics so failures return partial trace, missing-shard truth,
   unclean reason, capability truth, and resource counts instead of panicking or
   hiding the mess.

7. **Full Local Service E2E**
   Build one real-ish Tina local service over the portable backend. It should
   use TCP or TLS ingress/loopback, DNS, file/path I/O, timer, process,
   persistence, cross-shard call, bounded queues, and graceful shutdown. Prove
   one composed happy path, then focused negative paths for each scary edge:
   full queues, timeout, cancellation, stale address, failed shard, corrupt
   persistence, slow peer, and shutdown while work is in flight.

8. **DST Pressure Over Core Runtime**
   Add random histories that combine weird rocks rather than only happy rocks:
   timeout plus late completion, requester stop plus driver completion, remote
   full plus shard failure, persistence corruption plus restart, bridge ingress
   plus service shutdown, lane full plus cancellation, trace retention plus
   failure. Each family needs saved seeds and deterministic replay. At least
   one new family must exercise deletion shrinking.

9. **Cost And Allocation Report**
   Add a stable local command or test report for the portable backend. It
   should print backend, platform, profile, selected operation rows, allocation
   counts where probes exist, and rough timings where easy. Rows: local send,
   live ingress, cross-shard send, isolate call, timer, TCP loopback, TLS
   loopback, file read/write, journal append, bridge call. No thresholds and no
   marketing claim.

10. **CI Rails For The Portable Runtime**
    Extend the existing CI instead of replacing it. The default gate should run
    fmt, check, clippy, docs, workspace tests, portable capability table,
    readiness e2e, and selected DST seeds on Linux and macOS. Slow or
    host-specific tests must be named and gated honestly.

## Required Proof

- `make verify` passes.
- The portable capability table is executable and current.
- Every public rail has at least one positive, negative, overload/capacity,
  cancellation/timeout, and shutdown/resource-accounting proof.
- The full local service e2e proves composed happy path plus focused scary
  edges.
- New DST histories replay from saved seeds; at least one new family exercises
  shrinking.
- Cost report runs locally and prints numbers without claims.
- CI names the portable readiness gate and platform exclusions honestly.
- `ROADMAP.md`, `CHANGELOG.md`, and the Baobab plan are updated with only
  landed truth.

## Done Means

- Baobab can judge the portable local runtime instead of rediscovering known
  missing rocks.
- A future porting session has a serious runtime to try before worrying about
  `io_uring`.
- Tina still says no to hidden queues, hidden failure, hidden blocking, and
  fake performance claims.
