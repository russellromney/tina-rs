# Phase 045: Portable Local Runtime Completion

## Goal

Build the missing portable-runtime features that Baobab needs to judge.

This is not a survey phase. The first step maps coverage so we do not repeat
work. Every later step builds or hardens concrete runtime surface.

At closeout:

> Tina's current non-`io_uring` local runtime is a complete portable runtime
> target for serious local service experiments: bounded, observable,
> replay-tested, shutdown-honest, and boring enough for Baobab to gate.

## Non-Goals

- No `io_uring`, kernel bypass, DPDK, custom TCP stack, or hard OS pinning.
- No remoting, clustering, membership, placement, or durable mailbox.
- No general Tower/Axum middleware inside Tina.
- No broad performance claim.
- No `flow!` syntax or macro project.
- No hidden fallback queues.

## Rules

- Build missing runtime surface. Do not stop at audits.
- If something can overload, user code or topology must see pressure or `Full`.
- If something can fail, user code and trace must see typed failure.
- If something can race, DST or deterministic e2e must replay it.
- Blocking work is allowed only through bounded, reported lanes.
- Observation must survive failure: trace, topology, resources, unclean reason.
- No sleeps-as-proof.

## Build Rocks

1. **Coverage Map, Then Fix**
   Add a compact rail matrix in `review.md`: timers, TCP, TLS, DNS, UDP,
   file/path, process, signal, persistence, isolate calls, cross-shard sends,
   bridge ingress. Columns: positive, negative, overload, timeout/cancel,
   late completion, shutdown/resource report, trace, DST. Mark each cell
   `covered`, `weak`, `missing`, or `not-applicable(reason)`.

   Then fix every `missing` and every load-bearing `weak` cell needed for the
   portable local runtime claim.

2. **Executable Portable Capability Table**
   Add a Rust test/table as source of truth for the portable backend.
   Statuses: `Supported`, `Partial`, `Unsupported`, `NotClaimed`,
   `PlatformGated`, `NotApplicable(reason)`.

   It must assert against `RuntimeCapabilities`, bridge capabilities where
   relevant, and named non-claims. It covers execution shape, bounded capacity,
   cancellation, shutdown, replay, and blocking-lane truth.

3. **Unified Driver Lifecycle Surface**
   Build common lifecycle helpers/events where they reduce special cases:
   submit, accepted/full, timeout, cancel, late completion, close, drain,
   shutdown report, capability report. Do this across the portable rails that
   actually need it. Do not refactor rails that are already boring.

   Required outcome: public/user-facing behavior for each rail is predictable
   enough that Baobab can test it from the outside.

4. **Resource Inventory That Users Can Trust**
   Make topology and terminal reports distinguish:
   table-owned resources, worker-held resources, pending driver calls, queued
   lane work, bounded lane capacity/pressure, remote queue pressure, and failed
   shard state.

   If an ugly shutdown happens, the terminal report must still retain trace,
   topology, error, resource counts, and unclean reason.

5. **Fairness And Progress Rails**
   Prove cooperative progress for local ingress, remote inbound, hot mailboxes,
   driver completions, and lane completions. Add small budgets only where a
   test proves starvation.

   Do not claim preemption. A currently running synchronous handler or
   already-started blocking OS operation may still run until it returns; Tina
   must bound admission, report pressure, tombstone cancellation, and surface
   shutdown truth.

6. **Blocking-Lane Hardening**
   For storage, DNS, TLS, process, and persistence lanes, build/prove:
   lane full, queued cancellation, started-work tombstoning, late completion
   swallowing, shutdown drain timeout, worker-held accounting, and terminal
   reporting.

   Capability truth must say `LaneBackedBlocking` where that is the actual
   portable backend shape.

7. **Trace And Report API Hardening**
   Pin the user rule:
   `trace()` returns a `TraceSnapshot` that can be partial and names missing
   shards; `complete_trace()` is strict and may fail.

   Strengthen `LocalSystem`, `LocalMultiShardSystem`, `ThreadedRuntime`,
   bridge metrics, terminal reports, and shutdown reports so failure does not
   break the thing reporting the failure.

8. **Portable Service Harness**
   Build one reusable local service harness over the portable backend. It must
   use many real rails together: TCP or TLS ingress/loopback, DNS, file/path
   I/O, timer, process, persistence, cross-shard call, bounded queues, and
   graceful shutdown.

   Add one composed happy-path e2e. Add focused scary-edge e2e tests for:
   mailbox full, live ingress full, shard-pair full, resource lane full,
   timeout, cancellation, stale address, failed shard, corrupt persistence,
   slow peer, and shutdown while work is in flight.

9. **DST Families For Weird Rocks**
   Add new DST families with saved seeds and deterministic replay:

   - `tina-sim`: timeout + late completion; persistence corruption + restart;
     trace retention + failure.
   - `tina-runtime`: live-vs-sim projection for remote full + shard failure;
     requester stop + driver completion.
   - `tina-tokio-bridge`: bridge ingress + service shutdown + retry/cancel.

   At least one new family must exercise deletion shrinking.

10. **Portable Runtime Cost Report**
    Add one stable report command or test output for the portable backend.
    It prints backend, platform, profile, operation row, allocation count where
    probes exist, and rough timing where easy.

    Rows: local send, live ingress, cross-shard send, isolate call, timer, TCP
    loopback, TLS loopback, file read/write, journal append, bridge call.

    No thresholds. No external Tokio/Glommio baselines. No performance claims.
    Baobab owns comparison.

11. **Portable Runtime CI Gate**
    Add a named gate, preferably `make verify-portable-runtime`, and wire CI to
    run it on Linux and macOS. It should include the capability table, portable
    service harness, selected DST seeds, and cost report smoke run. Long DST
    stays behind a named env var such as `TINA_DST_LONG`.

12. **Baobab Handoff**
    Update `CHANGELOG.md`, `ROADMAP.md`, and Phase 046 Baobab with only landed
    truth. If 045 discovers a remaining non-claim, Baobab must compare that
    truth, not old hope.

## Required Proof

- `make verify` passes.
- `make verify-portable-runtime` exists and passes.
- The portable capability table is executable and current.
- Every `missing` matrix cell needed for portable runtime completeness is fixed.
- Every rail has positive, negative, overload/capacity, timeout/cancel or
  `not-applicable(reason)`, shutdown/resource, and trace proof.
- The portable service harness has composed happy path plus focused scary-edge
  tests.
- New DST families replay saved seeds; at least one new family shrinks.
- Cost report runs and prints numbers without claims.
- CI names platform exclusions honestly.

## Done Means

- Baobab gets to judge a built portable runtime, not a TODO list.
- A future porting experiment can target the current portable backend without
  immediately falling into known missing lifecycle/report/fairness holes.
- Tina remains honest: bounded work, visible overload, traceable failure,
  replayable races, no hidden queues, no fake speed story.
