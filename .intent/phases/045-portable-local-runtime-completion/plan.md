# Phase 045: Portable Local Runtime Completion

## Goal

Build the missing portable-runtime features that Baobab needs to judge.

This is not a survey phase. The phase builds concrete runtime surface first.
The coverage ledger records what was built, what proof exists, and what stays
a non-claim.

This phase must leave eight portability gates in place for Baobab:
complete local runtime story, a public app runner, clear app/service shape,
good enough ergonomics, brutal tests, visible capability matrix, non-toy
examples, and enough cost numbers to discuss performance without hand-waving.

At closeout:

> Tina's current non-`io_uring` local runtime is complete for serious local
> service experiments: bounded, observable, replay-tested, shutdown-honest,
> and boring enough for Baobab to gate.

## Non-Goals

- No `io_uring`, kernel bypass, DPDK, custom TCP stack, or hard OS pinning.
- No remoting, clustering, membership, placement, or durable mailbox.
- No general Tower/Axum middleware inside Tina.
- No broad performance claim.
- No `flow!` syntax or macro project.
- No hidden fallback queues.

## What Will Not Change

- Isolate handlers stay synchronous and return effects.
- Mailboxes stay bounded and surface `Full`/`Closed`; no hidden overflow queue.
- Existing direct construction paths remain supported: `LocalSystem`,
  `LocalMultiShardSystem`, and low-level `ThreadedRuntime`/current runtime.
- Runtime capability truth does not absorb Tokio bridge semantics.
- Isolate code does not gain async handlers, raw backend handles, or Tokio
  tasks.
- Cancellation does not become a preemption claim for already-started blocking
  OS work.
- `SYSTEM.md` is updated only after direct proof lands.

## Rules

- Build missing runtime surface. Do not stop at audits.
- If something can overload, user code or topology must see pressure or `Full`.
- If something can fail, user code and trace must see typed failure.
- If something can race, DST or deterministic e2e must replay it.
- Blocking work is allowed only through bounded, reported lanes.
- Observation must survive failure: trace, topology, resources, unclean reason.
- No sleeps-as-proof.
- Public API changes are allowed when they are the user path. The canonical
  app runner is the user path, not a test-only helper.
  Prefer crate-private helpers only for lifecycle internals.
- The service harness must use ordinary Tina effects only: no async handlers,
  no raw backend handles, no Tokio tasks inside isolates.
- Ergonomics may remove ceremony, but must not hide overload, failure,
  timeout, cancellation, queueing, or shutdown.
- Tiny helpers are allowed only when repeated public runner/service ceremony
  proves the need. No helper may create a second semantic path, hidden retry,
  hidden capacity, hidden timeout, or hidden shutdown behavior.

## Build Rocks

1. **Implementation Ledger And Gap Closure**
   Keep a compact rail ledger in `review.md` while building. This ledger is
   evidence, not the main deliverable. It must not replace code, tests, or
   public-path proof.

   Rows: timers, TCP, TLS, DNS, UDP, file/path, process, signal, persistence,
   isolate calls, cross-shard sends, bridge ingress.

   Columns: positive, negative, overload, timeout/cancel, late completion,
   shutdown/resource report, trace, DST.

   Mark each cell `covered`, `weak`, `missing`, or
   `not-applicable(reason)`. Close each needed cell as `covered`, `fixed`, or
   `deferred-nonclaim`. `deferred-nonclaim` must update capability truth and
   Phase 046 Baobab.

2. **Executable Portable Capability Table**
   Add a Rust test/table as source of truth for the portable backend.
   Statuses: `Supported`, `Partial`, `Unsupported`, `NotClaimed`,
   `PlatformGated`, `NotApplicable(reason)`.

   Expected shape: typed rows with `status` plus an optional static `reason`
   field, so `NotApplicable` and `Partial` stay assertable instead of becoming
   prose.

   It must assert against `RuntimeCapabilities` and named runtime non-claims.
   Bridge capability truth must stay separate, with its own adapter-facing
   table or assertions. Cross-check bridge truth where relevant, but do not let
   `tina-runtime` claim Tokio bridge semantics.

   It must also prove the user-facing resource budget manifest is complete.
   Runtime-owned budgets live in the public runner/config: ingress,
   shard-pair, remote-drain, DNS/TLS/process/storage lanes, signal capacity,
   trace retention, preallocation, and shutdown drain timeout. Isolate mailbox
   capacities remain explicit per root registration, spawn, or child
   definition. Tests must prove both halves are visible without requiring users
   to drop into low-level `ThreadedRuntimeConfig`.

3. **Unified Driver Lifecycle Surface**
   Build common lifecycle helpers/events where they reduce special cases:
   submit, accepted/full, timeout, cancel, late completion, close, drain,
   shutdown report, capability report. Do this across the portable rails that
   actually need it. Do not refactor rails that are already boring.
   Prefer crate-private helpers first; add public events/types only when the
   user-facing semantics require them.

   Required outcome: public/user-facing behavior for each rail is predictable
   enough that Baobab can test it from the outside.

   Closure rule: changed rails need direct proof. Unchanged rails can close by
   existing proof plus blast-radius proof. Unsupported or intentionally
   untouched rails must be named non-claims in capability truth.

4. **Resource Inventory That Users Can Trust**
   Make topology and terminal reports distinguish:
   table-owned resources, worker-held resources, pending driver calls, queued
   lane work, bounded lane capacity/pressure, remote queue pressure, and failed
   shard state.

   If exact queued-lane depth cannot be reported honestly for a bounded
   `sync_channel`, report capacity plus accepted/rejected pressure and pending
   worker-held work instead of guessing.

   If an ugly shutdown happens, the terminal report must still retain trace,
   topology, error, resource counts, and unclean reason.

5. **Fairness And Progress Rails**
   Prove cooperative progress for local ingress, remote inbound, hot mailboxes,
   driver completions, and lane completions. Add small budgets only where a
   test proves starvation.

   Required scenarios:
   local ingress under hot self-sender; remote inbound under local ingress
   pressure; driver completion under hot mailbox pressure; lane completion
   under unrelated mailbox pressure; shutdown signal under in-flight lane work.

   Add two standard backpressure patterns to the service harness: immediate
   reject/busy reply, and explicit retry/backoff through Tina-owned timers. Do
   not add a broad policy framework.

   Do not claim preemption. Queued blocking work can be cancelled when the
   backend supports removing it before start. Already-started blocking OS work
   may still run until it returns; Tina must bound admission, report pressure,
   tombstone cancellation, and surface shutdown truth.

6. **Blocking-Lane Hardening**
   For storage, DNS, TLS, process, and persistence-over-storage lanes,
   build/prove:
   lane full, queued cancellation, started-work tombstoning, late completion
   swallowing, shutdown drain timeout, worker-held accounting, and terminal
   reporting. Prove queued cancellation and started-work tombstoning with
   separate tests.

   Capability truth must say `LaneBackedBlocking` where that is the actual
   portable backend shape. Signals and UDP are poll-backed, not blocking-lane
   work; keep them in capability truth and resource tests, not fake lane tests.

7. **Trace And Report API Hardening**
   Pin the user rule:
   `trace()` returns a `TraceSnapshot` that can be partial and names missing
   shards; `complete_trace()` is strict and may fail.

   Strengthen `LocalSystem`, `LocalMultiShardSystem`, `ThreadedRuntime`,
   bridge metrics, terminal reports, and shutdown reports so failure does not
   break the thing reporting the failure.

   Bridge metrics must expose accepted, full, closed, timeout, cancelled,
   responded-late, and shutdown-retry truth where the bridge can observe them.
   Do not claim deterministic replay under Tokio.

   Bridge work in 045 is adapter regression proof only. Do not let it become
   the center of the runtime phase.

   Add direct public negative tests: after shard failure, `trace()` returns a
   partial snapshot naming missing shards, `complete_trace()` fails cleanly,
   and the terminal report still carries topology, resource, and error truth.

8. **Public Local Service Runner**
   Build the blessed local-service shape that users and porting tests will
   target. It must be public API, not a test-only helper.

   Shape:
   create `LocalSystem`, configure the resource budget manifest, register
   roots, start listener/service isolates, wait for runtime shutdown signal,
   drain, join, and return/inspect the terminal report.

   Minimum public operations: configure runtime budgets, register roots,
   start/run, request or observe shutdown, drain/join, and return a terminal
   report. Names should follow the existing code style; the operations must be
   present.

   This should feel like Tokio's split between `#[tokio::main]` and
   `runtime::Builder`: 045 builds an explicit public `LocalSystem`
   runner/builder path now. No attribute macro in this phase. Normal users
   must have a real way to run a Tina app without copying test scaffolding.

   The runner must not hide mailbox capacity, ingress capacity, lane capacity,
   timeout policy, shutdown policy, or terminal reports. It may remove
   ceremony; it may not hide overload, failure, cancellation, queueing, or
   shutdown truth.

   The runner must compose existing `LocalSystem` behavior. It must not add a
   second delivery engine, hidden worker pool, hidden queue, or special service
   path that bypasses ordinary Tina semantics.

   Tests must prove the public runner path directly from outside the crate and
   assert that it returns a terminal report. The service harness and non-toy
   example must use this public path unless a test is intentionally exercising
   lower-level runtime internals.

   Blast-radius proof: direct `LocalSystem`, `LocalMultiShardSystem`, and
   low-level `ThreadedRuntime`/current runtime construction must still work.
   Existing bridge tests must stay green. Add at least one new test that
   bypasses the runner and still exercises the old low-level path.

9. **Non-Toy Portable Service Example**
   Build a runnable example that uses the same harness shape but reads like an
   app, not a unit test. It should be small enough to understand and serious
   enough to resemble a port target: listener/session lifecycle, bounded
   resource config, timeout/retry, persistence checkpoint, graceful shutdown,
   and terminal report inspection.

   The example must be CI-safe: use ephemeral ports, temp directories,
   deterministic cleanup, and no external network dependency.

   This is not marketing polish. It is a compile/run artifact that proves the
   app/service shape is legible outside a test assertion wall.

   No-async-leakage proof: the example and public-runner e2e must compile and
   run without `tokio::spawn`, async handlers, or raw backend handles in isolate
   code. This may be proved by targeted source checks plus compile/run proof.

10. **Portable Service Harness**
   Build one reusable local service harness in
   `tina-runtime/tests/portable_service.rs` over the portable backend. It
   should use several real rails together without forcing every rail into one
   unreadable scenario: TCP or TLS ingress/loopback, DNS, file/path I/O,
   timer, process, persistence, cross-shard call, bounded queues, and graceful
   shutdown.

   The harness must model the normal listener/session lifecycle: listener
   isolate accepts, creates or notifies a per-connection/session isolate, hands
   over stream ownership, reads/writes with timeout, closes, and abandons
   cleanly on shutdown.

   Add one composed happy-path e2e. Add focused direct scary-edge e2e tests
   for:
   mailbox full, live ingress full, shard-pair full, resource lane full,
   timeout, cancellation, stale address, failed shard, corrupt persistence,
   slow peer, and shutdown while work is in flight.

   Focused tests must directly hit changed paths rather than relying on the
   composed happy path as surrogate proof.

   Prove supervision plus I/O composition: supervised listener/session/worker
   failure while runtime-owned I/O is pending; restart does not inherit stale
   resources; stale addresses reject visibly; shutdown still drains/reports.

   Prove a port-shaped request/reply boundary: local call, cross-shard call,
   timeout, requester closed, requester mailbox full, bridge timeout/cancel,
   and responded-late behavior in one user-shaped request path.

   Prove failure domains under service load: one shard/session fails, sibling
   shard/session keeps serving, topology names the failed domain, and partial
   trace survives.

   The process rail must use a deterministic portable command or a
   platform-gated scenario. No shell-specific or environment-specific command
   should be required for the core CI harness.

11. **DST Families For Weird Rocks**
   Add new DST families with saved seeds and deterministic replay:

   - `tina-sim`: timeout + late completion; persistence corruption + restart;
     trace retention + failure.
   - `tina-runtime`: deterministic scripted live-vs-sim projection for remote
     full + shard failure; requester stop + driver completion. Compare stable
     semantic facts only: accepted/full/closed/timeout/cancel/report shape.
     Do not compare wall-clock order, OS scheduling order, or raw trace byte
     equality with live backend noise.
   - `tina-tokio-bridge`: bridge ingress + service shutdown + retry/cancel.

   At least one new family must exercise deletion shrinking.

12. **Portable Runtime Cost Report**
   Add one stable report command, preferably `make portable-runtime-cost`, for
   the portable backend. It prints backend, platform, profile, operation row,
   configured capacities/preallocation, allocation count where probes exist,
   and rough timing where easy.

   Rows: local send, live ingress, cross-shard send, isolate call, timer, TCP
   loopback, TLS loopback, file read/write, journal append, bridge call.

   Use a tiny smoke mode for CI and a larger manual mode for humans. Pin the
   profile in the output. No thresholds. No external Tokio/Glommio baselines.
   No performance claims. Baobab owns comparison.

13. **Portable Runtime CI Gate**
    Add a named additive gate, preferably `make verify-portable-runtime`, and
    wire CI to run it on Linux and macOS. It should include the capability
    table, portable service harness, selected DST seeds, and cost report smoke
    run. It should not duplicate the full workspace `make verify`. Long DST
    stays behind a named env var such as `TINA_DST_LONG`.

    The gate must run the public service runner and portable service harness
    scary-edge tests. Tables and DST alone are not enough.

    Platform differences must be asserted through capability truth and
    platform-gated expectations, not silent skips.

14. **Baobab Handoff**
    Update `SYSTEM.md`, `CHANGELOG.md`, `ROADMAP.md`, and Phase 046 Baobab
    plan/review with only landed truth after proof. If 045 discovers a
    remaining non-claim, Baobab must compare that truth, not old hope.

## Required Proof

- `make verify` passes.
- `make verify-portable-runtime` exists and passes.
- The portable capability table is executable and current.
- Every `missing` matrix cell needed for portable runtime completeness is fixed.
- Every `deferred-nonclaim` cell is reflected in capability truth and Baobab.
- Changed rails have direct public-path proof. Unchanged rails have existing
  proof plus blast-radius proof. Unsupported or intentionally untouched rails
  are named `not-applicable(reason)` or non-claims.
- Public local service runner shape exists and is tested from the outside.
- Direct construction without the public runner still works for `LocalSystem`,
  `LocalMultiShardSystem`, and low-level `ThreadedRuntime`/current runtime.
- Non-toy runnable portable service example exists and uses the same app shape.
- The portable service harness has composed happy path plus focused scary-edge
  tests.
- Listener/session lifecycle, supervision plus I/O, request/reply boundary, and
  sibling progress under failure are directly proved.
- The target workload uses ordinary Tina effects only, with no async/raw backend
  leakage inside isolates.
- Existing bridge tests stay green, and bridge capability truth remains
  adapter-scoped.
- The process rail uses deterministic portable command proof or platform-gated
  proof.
- New DST families replay saved seeds; at least one new family shrinks.
- Live DST/projection compares semantic facts only, not wall-clock ordering.
- `make portable-runtime-cost` or equivalent runs and prints numbers without
  claims, including capacity/preallocation context, profile, CI smoke mode,
  and manual mode.
- CI names platform exclusions honestly.

## Done Means

- Baobab gets to judge a built portable runtime, not a TODO list.
- A future local-service porting experiment can target the current portable
  backend without immediately falling into known
  lifecycle/report/fairness holes.
- Tina remains honest: bounded work, visible overload, traceable failure,
  replayable races, no hidden queues, no fake speed story.
