# Phase 045: Portable Local Runtime Completion

## Goal

Build the missing user-shaped layer over the portable local runtime.

This phase starts from a code audit, not a roadmap guess. Tina already has
`LocalSystem`, `LocalMultiShardSystem`, runtime-owned I/O rails, topology,
capability reports, cross-shard calls, shutdown reports, and many direct tests.

The gap is simpler and sharper:

> A normal local service should have one blessed shape, one budget manifest,
> visible backpressure, fair progress under pressure, service-level DST, cost
> numbers, and a CI gate that proves the shape works.

At closeout:

> Tina's non-`io_uring` local runtime is coherent enough for serious local
> service experiments before Baobab judges production readiness.

## Code Audit Baseline

Already exists:

- `LocalSystem::single_shard` and `LocalSystem::multi_shard`.
- root registration, supervision, ingress send, observed send, trace,
  complete trace, topology, capabilities, drain/join shutdown.
- `LocalSystemConfig` with ingress, shard-pair, remote-drain, storage, DNS,
  TLS, process, signal, trace, preallocation, idle wait, and shutdown drain
  fields.
- live cross-shard isolate-call reply transport with reply/full/closed/timeout
  tests.
- runtime-owned time, TCP, UDP, DNS, TLS client/server, file/path,
  persistence, process, signal, and shutdown notification rails.
- topology reports for lane capacity, worker-held resources, pending driver
  calls, remote queue pressure, failed shards, partial traces, and terminal
  truth.
- allocation/cost probes for narrow hot paths.
- many service-like tests in `local_system.rs`, `application_surface.rs`,
  `tina-sim`, and the Tokio bridge.

Weak or missing:

- no single canonical public service/session harness that users would copy.
- service/router/placement patterns are scattered through tests.
- budget manifest is real, but builders lack some obvious budget knobs and no
  one test proves the whole manifest from the user path.
- backpressure handling is visible, but common service policies are not named.
- fairness proofs exist in slices, not as one portable service pressure story.
- composed I/O tests exist, but not one canonical pressure harness covering the
  service shape.
- DST exists, but service-level DST is not yet the default proof wall.
- no portable cost report command.
- no portable-runtime coverage inside the single `make verify` CI gate.
- truth docs are stale in places; `SYSTEM.md` still underclaims live
  cross-shard isolate-call transport.

## Non-Goals

- No `io_uring`, DPDK, kernel bypass, custom TCP stack, or hard OS pinning.
- No remoting, clustering, membership, durable mailbox, or durable work queue.
- No general Tower/Axum middleware inside Tina.
- No broad speed claim.
- No `flow!` macro.
- No hidden fallback queue.
- No async handlers or raw backend handles in isolate code.

## Rules

- Build code and tests. The audit is done before the phase; it is not the
  deliverable.
- If something can overload, user code or topology must see `Full`, `Busy`, or
  pressure.
- If something can fail, user code and trace must see typed failure.
- If something can race, DST or deterministic e2e must replay it.
- Blocking rails stay bounded and reported. Already-started blocking OS work is
  not preemption; Tina must tombstone, drain, and report honestly.
- `trace()` may be partial and must not break when the system is broken.
  `complete_trace()` is strict and may fail.
- `SYSTEM.md` changes only after direct proof.
- Tests may use wall-clock waits only as bounded timeouts around deterministic
  conditions. They must not use sleeps as the proof.
- New helpers must earn their place: repeated public-path ceremony at least
  three times, no hidden capacity, no hidden timeout, no hidden retry, no hidden
  route, no hidden failure policy.

## Build Rocks

1. **Canonical Service/Session Harness**

   Add one public-path harness in `tina-runtime/tests/portable_service.rs`.
   It must use `LocalSystem`/`LocalMultiShardSystem`, not low-level worker
   internals.

   The harness must be copyable by a user using public Tina APIs. If it needs
   private helpers or strange test-only scaffolding, fix the public shape
   instead of hiding the problem in tests.

   Shape:

   - configure runtime budgets;
   - register listener/service/session roots;
   - accept or start work;
   - route to worker/session isolates;
   - use runtime-owned calls for I/O/time/persistence;
   - drain/join;
   - assert terminal report.

   Include one composed happy path and focused scary paths.

2. **Logical Service/Router Pattern**

   Make the common app shape legible:

   - listener isolate owns listener resource;
   - session isolate owns connection/resource state;
   - router/service isolate maps request keys to workers;
   - worker isolate owns domain state;
   - audit/persistence isolate records durable facts when useful.

   Add helpers only under the helper rule above. Prefer patterns first; add
   public API only when repeated public-path ceremony proves the need.

3. **Placement And Shard Ownership Pattern**

   Prove key-to-shard placement under `LocalMultiShardSystem`.

   Required proof:

   - stable shard ownership is visible in topology;
   - cross-shard call returns typed reply/full/closed/timeout;
   - stale addresses reject visibly after restart/stop;
   - one failed shard does not stop sibling shard service work;
   - partial trace names the missing shard.
   - unknown shard and wrong-shard/wrong-key placement reject visibly.

   If placement remains a pattern rather than a public API, the pattern's bad
   route must still fail visibly in tests.

4. **Resource Budget Manifest Proof**

   Make the user budget manifest boring.

   Add missing builder convenience knobs if needed for DNS, TLS, process,
   signal, and shutdown drain timeout. Keep `LocalSystemConfig` as the exact
   source of truth.

   Test from the public path:

   - every runtime-owned budget is configurable and visible in topology;
   - zero capacities reject before start;
   - isolate mailbox capacities remain explicit at registration/spawn;
   - no user has to drop to `ThreadedRuntimeConfig` for normal local services;
   - terminal topology/report preserves the configured budget shape after
     shutdown where terminal reports carry that field.

5. **Backpressure Policy Helpers**

   Add two tiny service patterns, not a broad framework:

   - immediate busy/reject reply;
   - explicit retry/backoff through Tina-owned timers.

   They must preserve visible `Full`/`Closed`/`Timeout`; no hidden retry loop,
   hidden queue, or hidden timeout.

   Expected outcomes:

   - reject path returns a typed busy/rejected message or outcome;
   - retry path schedules a named Tina-owned timer and retries explicitly;
   - exhausted retry returns a typed failure instead of silently dropping.

6. **Scheduling/Fairness Budgets**

   Prove cooperative progress under pressure.

   Required scenarios:

   - hot local self-sender does not starve ingress;
   - local ingress does not starve remote inbound;
   - hot mailbox does not starve driver completions;
   - unrelated mailbox pressure does not starve lane completions;
   - shutdown signal still reports while lane work is in flight.

   Add or adjust small budgets only when a test proves starvation.

   Use barriers, parked handlers, bounded queues, observed trace/events, and
   deterministic completion controls where possible.

7. **Composed I/O Under Pressure**

   Build one readable service harness that uses multiple rails in realistic
   composition:

   - TCP or TLS ingress/loopback;
   - timer timeout/backoff;
   - DNS or UDP where platform-safe;
   - file/path or persistence;
   - process rail with deterministic portable command or platform gate;
   - cross-shard call;
   - graceful shutdown.

   Add focused negative tests for mailbox full, ingress full, shard-pair full,
   lane full, timeout, cancellation, stale address, failed shard, corrupt
   persistence, slow peer, and shutdown while work is in flight.

   Do not make one giant all-rails/all-failures test. Use one readable happy
   path plus focused edge tests that share helper code.

   Process proof should use a deterministic portable command. Prefer a
   Rust-built helper/current test binary where practical. If platform gating is
   unavoidable, assert the capability truth instead of silently skipping.

8. **Service-Level DST**

   Add DST families that model whole service behavior, not only rail behavior.

   Required:

   - service request histories with send/call/full/closed/timeout;
   - persistence corruption plus restart;
   - trace retention plus shard failure;
   - bridge ingress plus shutdown/cancel;
   - live-vs-sim projection comparing stable semantic facts only.

   At least one new DST family must replay saved seeds and shrink by deletion.

   Homes:

   - simulator service DST lives in `tina-sim/tests/...`;
   - live projection lives in `tina-runtime/tests/portable_service.rs` or a
     sibling test file;
   - bridge model DST stays in `tina-tokio-bridge/tests/...`.

   Saved seeds must be constants in tests. Shrink proof must assert the smaller
   history still reproduces the same failure. Live-vs-sim projection must not
   compare event ids, wall-clock order, OS scheduling order, worker thread ids,
   raw timing, or platform-specific error strings.

9. **Portable Cost Report**

   Add `make portable-runtime-cost`.

   It prints:

   - backend/platform/profile;
   - configured capacities/preallocation;
   - operation row;
   - allocation count where probes exist;
   - rough timing where easy.

   Rows: local send, live ingress, cross-shard send, isolate call, timer, TCP
   loopback, TLS loopback, file read/write, journal append, bridge call.

   CI mode is a tiny smoke. Human mode can be larger. No thresholds and no
   performance claim. Output must label itself as "cost smoke / local machine /
   not benchmark." CI asserts the command runs and expected row names appear.

10. **CI Gate For Actual Service Harness**

    Fold the portable runtime harness into the one project gate: `make verify`.
    It must run:

    - capability/budget manifest tests;
    - portable service harness happy path;
    - selected scary-edge tests;
    - selected DST seeds;
    - portable cost smoke.

    Platform differences must be explicit capability truth, not silent skips.

11. **Truth Docs After Proof**

    Update `SYSTEM.md`, `CHANGELOG.md`, `ROADMAP.md`, and Baobab only after
    proof lands.

    Audit-cleanup truth docs may record already-proved facts. New 045 claims
    require new 045 proof. Baobab must be updated to consume exactly the 045
    service harness, cost smoke, CI gate, and any remaining non-claims.

## Required Proof

- `make verify` passes.
- `make verify` includes the portable runtime harness and passes.
- The portable service harness proves composed happy path and focused scary
  paths.
- User-shaped tests cover positive, negative, overload, timeout/cancel, late
  completion, shutdown report, trace, and DST where applicable.
- The budget manifest is executable truth.
- The public path is used; low-level runtime tests remain as blast-radius proof.
- New DST families replay; at least one shrinks.
- The cost report runs without making a speed claim.
- Truth docs match only what the tests prove.

## Done Means

- Baobab gets to judge a built portable runtime, not a TODO list.
- A local service can be written in one clear Tina shape without copying random
  test scaffolding.
- Tina remains Tina: bounded work, visible overload, traceable failure,
  replayable races, no hidden queue, no fake speed story.

## Closeout Notes

Implemented:

- `tina-runtime/tests/portable_service.rs` is the canonical public-path
  service harness. It uses `LocalMultiShardSystem`, budget builders, router and
  shard-owned workers, journal-before-reply, wrong-placement rejection,
  unknown-shard rejection, observed-send accepted-continuation, observed-send
  full before reply, explicit busy retry with Tina-owned timer, terminal report
  checks, and journal replay.
- Live runtime and simulator now preserve isolate-call context through
  runtime-owned call completions and observed-send completions. This was found
  by the harness: call -> journal append -> later reply previously became a
  timeout plus trace-only reply.
- Budget manifest proof now covers DNS/TLS/process/signal/shutdown-drain
  builder knobs and terminal topology/report truth.
- `tina-sim/tests/portable_service_dst.rs` adds saved-seed service DST with
  replay equality, common invariants, observed-send continuation, observed-send
  full before persistence, closed-worker outcomes, journal append, and deletion
  shrinking.
- `make portable-runtime-cost` prints labeled cost-smoke rows only. It is not a
  benchmark.
- `make verify` and CI run the portable service harness, budget manifest,
  service DST, bridge cancellation model, and cost smoke.

Remaining non-claims:

- The cost command has row coverage, not real performance numbers.
- `io_uring`, remoting, clustering, durable mailbox, hard OS pinning, and broad
  Tower/Axum-inside-Tina remain later work.
