# 034 Piet de Jong Local Production Readiness Plan

## Purpose

Make Tina meaningfully closer to a production-ready local concurrency framework.

This phase does **not** try to launch Tina, polish the website, or prove a
demo. It finishes the next hard layer of the core framework: the substrate
story, the bridge story, hardening, performance evidence, and the missing
local-service APIs that make a normal Tokio-shaped service portable to Tina
without bespoke runtime glue.

Target end-state claim:

> Tina is production-shaped for local, bounded, shared-nothing services: a
> service can run behind a Tokio/Tower/Axum edge, keep Tina's synchronous
> isolate model internally, use runtime-owned time/TCP, observe overload and
> cancellation directly, shut down cleanly, and be tested against deterministic
> simulation/DST evidence.

Still not claimed at phase end:

- general Tokio replacement;
- distributed remoting/clustering/persistence;
- broad performance win over Tokio;
- arbitrary async ecosystem integration inside isolates;
- production maturity for every I/O family.

## Starting Baseline

Dries made the first version real:

- `tina-tokio-bridge` exists and supports a narrow Axum/Tower entry path;
- bridge cancellation, bounded ingress, retry naming, host shutdown retry, and
  real-service message enums are fixed;
- live runners have backend-honest names;
- `TINA_DRIVER_RUNTIME_CONTRACT` names the substrate direction;
- trace retention, bridge metrics, capability tables, and compile-fail guardrails
  exist in first form;
- `make verify` is green.

But grug still sees five big gaps before "Claude can port a Tokio local service
to Tina and it mostly works":

1. driver-runtime substrate maturity;
2. bridge/adapters are useful but still narrow;
3. production hardening is not yet boring;
4. performance envelope is honest but too thin;
5. local-service API surface is incomplete for normal apps.

This phase attacks those five directly.

## Pinned Intended Surface

Piet's preferred live app surface is `tina_runtime::LocalApp`.

Expected ownership shape:

- `LocalApp` lives in `tina-runtime` and is the canonical live owner for local
  Tina services.
- `BetelgeuseBackedRuntime` and `BetelgeuseBackedMultiShardRuntime` remain
  lower-level backend-honest runners.
- `tina_tokio_bridge::BridgeHost` becomes the bridge crate's wrapper around a
  `LocalApp`-owned runtime, not a competing app owner.
- Single-shard and multi-shard use sibling builders under one name:
  `LocalApp::single_shard(...)` and `LocalApp::multi_shard(...)`.
- Shutdown returns a terminal report with final lifecycle state, worker result,
  terminal trace/sink view, cancellation/drain counts, and any worker panic.

Intended single-shard shape:

```rust
let mut app = LocalApp::single_shard(AppShard, AppMailboxFactory)
    .ingress_capacity(1024)
    .trace_retention(TraceRetention::Bounded(4096))
    .build();

let users = app.register_root(UserService::new(), 256)?;
let result = app.try_send(users, UserMsg::Join("llama-7".into()))?;
assert!(result.is_accepted());

let terminal = app.shutdown().drain().join()?;
assert_eq!(terminal.state(), LocalAppState::Closed);
```

Intended multi-shard shape:

```rust
let mut app = LocalApp::multi_shard(AppMailboxFactory)
    .shard(AppShard::Ingress)
    .shard(AppShard::Workers)
    .ingress_capacity(1024)
    .shard_pair_capacity(256)
    .trace_retention(TraceRetention::Bounded(8192))
    .build();

let ingress = app.register_root_on(AppShard::Ingress, Ingress::new(), 256)?;
let worker = app.register_root_on(AppShard::Workers, Worker::new(), 256)?;
app.link("ingress-to-worker", ingress, worker)?;
```

Intended bridge shape:

```rust
let mut app = LocalApp::single_shard(AppShard, AppMailboxFactory).build();
let service = app.register_root(LlamaService::new(), 256)?;

let tower_service = BridgeHost::from_app(app)
    .register_tower_service(service, BridgeConfig::default())?;
```

These sketches are rails, not final syntax. Implementation may tighten names if
code demands it, but it should preserve this ownership story: app owner first,
backend runner underneath, Tower bridge at the edge.

## Rock 1: Driver-Runtime Substrate Maturity

Turn the current driver-runtime contract into a boring local service substrate.

Implement or pin:

- one canonical live app runner path;
- explicit lifecycle state machine: `Starting`, `Accepting`, `Closing`,
  `Draining`, `Closed`, `Failed`;
- worker panic visibility and typed terminal result;
- graceful shutdown with a direct rule for queued commands, pending timers,
  pending TCP operations, bridge requests, and in-flight isolate calls;
- driver cancellation/drain proof that does not depend on backend accident;
- multi-shard live topology config that names shard ids, worker names, ingress
  capacity, shard-pair capacity, and shutdown order;
- no hidden executor tasks and no unbounded internal queue unless it is named as
  a non-Tina edge with a bounded wrapper above it;
- a small backend abstraction seam that keeps Betelgeuse-backed I/O as one
  implementation, not Tina's semantic source.

Expected direction:

- keep completion-shaped I/O;
- keep synchronous handlers returning effects;
- keep deterministic simulator/DST as oracle;
- keep Tokio/monoio/other drivers outside the isolate model unless mediated by
  explicit adapters;
- do not build a new general Rust async runtime in this phase.

Proof:

- live startup/shutdown tests for clean, closing, failed, and panic paths;
- pending timer/TCP/call cancellation tests when requester stops;
- shutdown torture: blocked worker command, queued command, pending TCP, pending
  timer, bridge request, and multi-shard in-flight transport;
- multi-shard live pressure test with bounded remote queues and deterministic
  accepted/full/closed outcomes;
- simulator/live parity tests for every lifecycle outcome that both surfaces can
  honestly share.

## Rock 2: Bridge And Adapter Breadth

Make the bridge a useful adoption layer, not a toy edge.

Pinned boundary:

- Tower `Service` is the canonical bridge boundary.
- Axum is the first proof adapter on top of Tower.
- Tina bridge code does not chase Axum-specific routing, extraction, or response
  features beyond what the Tower boundary can naturally support.
- `poll_ready` reports bridge health/closed state only; bounded queue admission
  remains a call-time fact unless Piet adds a real capacity probe.

Implement or pin:

- one canonical Tower service helper for "Tokio HTTP enters Tina";
- one Axum proof adapter that uses the Tower helper;
- request extraction and response mapping helpers that do not hide capacity or
  timeout;
- bridge support for services that use runtime-owned time/TCP/spawn/isolate-call
  effects internally;
- bridge cancellation semantics for all phases: before queue admission, queued
  before handler, running handler, waiting for response, after timeout;
- total-deadline option in addition to per-attempt timeout if retry math remains
  easy to misread;
- readiness/health integration for accepting/closing/closed/failed;
- metrics counters for accepted, full, closed, timeout, cancelled before run,
  cancelled after run, late response rejected, retry count, and successful
  response;
- a bounded bridge ingress policy table: reject, retry, total deadline, close.

Refusals:

- no async isolate handlers;
- no arbitrary future execution inside Tina state;
- no hidden bridge queue that can grow without bound;
- no claim that Tokio work-stealing becomes replayable;
- no Tower/Axum feature chase beyond the minimum that proves real app entry.

Proof:

- Axum integration test with multiple routes sharing one Tina service;
- overload test where Tokio caller sees `Full` without sleep-as-proof;
- cancellation tests for queued-before-handler and timeout-after-admission;
- retry tests that prove `max_retries`, per-attempt timeout, and total deadline;
- graceful shutdown test from HTTP edge through Tina runtime;
- compile-fail guardrails for async handler misuse, non-`Send` bridge messages,
  wrong response type, and missing timeout where the bridge can catch it.

Bridge cancellation truth table:

| State | Behavior |
|---|---|
| Before queue admission | Do not enqueue; caller observes cancel/timeout/closed. |
| Admitted to bridge queue but not runtime queue | Drop/skip before Tina sees it; no user state mutation. |
| In target mailbox but handler not started | `BridgeGuard` skips handler; no user state mutation. |
| Synchronous handler running | Not preempted. Handler turn completes because Tina handlers are synchronous. |
| Waiting for response after caller timeout/cancel | Late response is rejected/observed and metrics record it. |
| Response already delivered | Cancellation has no effect. |

The bridge must not claim stronger cancellation than this.

## Rock 3: Production Hardening

Make "does this pass the real gate?" boring.

Implement or pin:

- GitHub Actions CI for the intended supported gate;
- separate local targets for fast verify, full verify, stress, loom, miri, and
  doc/compile-fail tests if they are not already cleanly separated;
- platform matrix decision for the live substrate path;
- test classification: unit, integration, simulator/DST, live, loom, miri,
  compile-fail, stress;
- panic/abort policy note and tests where Rust behavior changes the guarantee;
- long-run quiescence tests for trace retention, bridge metrics, driver pending
  work, and shutdown;
- direct regression tests for every P1/P2 review finding from Dries/Piet.

Expected direction:

- required GitHub Actions job is `.github/workflows/verify.yml` running
  `make verify` on the supported host matrix;
- required local mirror is `make verify`;
- `make verify` must include the compile/doc/test gate that the repo treats as
  ordinary correctness;
- compile-fail/trybuild coverage is required if already part of `make verify`;
- stress, Loom, and Miri are separate manual or nightly-only gates unless this
  phase proves they are cheap enough for the default CI path;
- claimed live-substrate platforms are exactly the platforms in the required CI
  matrix; any OS not in that matrix is a non-claim;
- no "works on my machine" production claim.

Proof:

- CI config committed and documented in repo scripts;
- `make verify` and CI gate agree about what must be green;
- stress target can be run locally and reports deterministic seed/config;
- failing examples are not used as proof.

## Rock 4: Performance And Allocation Envelope

Replace vibe with numbers.

Measure and gate selected local-service paths:

- local send accepted/full/closed;
- same-shard isolate call replied/full/closed/timeout;
- bridge call success/full/timeout/cancelled;
- TCP echo read/write;
- runtime-owned timer;
- multi-shard local transport;
- trace retention modes;
- high-cardinality idle stepping;
- startup/shutdown with many isolates.

Implement or pin:

- allocation probes use the existing global-allocator pattern from
  `tina-runtime/tests/multishard_allocation.rs`;
- latency/throughput measurements use a custom release-mode harness first,
  unless implementation deliberately chooses Criterion and records why;
- memory-under-overload scenario comparing bounded Tina behavior to an
  intentionally constrained Tokio baseline if it helps expose the contract;
- deterministic allocation/no-growth claims may be test-gated;
- wall-clock measurements are recorded evidence first, not default CI
  thresholds, unless a path proves stable enough not to create flakes;
- performance-envelope results are summarized in this phase's `review.md`
  during implementation, with commands and raw numbers sufficient to reproduce;
- a cost table states what allocates and why.

Decision topics:

- whether completion-slot boxing can be slabbified safely;
- whether call translators or erased payloads need a typed fast path;
- whether bridge future boxing is acceptable for adoption edge only;
- whether zero-copy TCP payload paths are worth this phase or later.

Proof:

- reproducible benchmark/probe command;
- committed cost-envelope artifact or test output summary;
- tests pin no-regression allocation behavior for warmed hot paths where we
  claim it;
- no broad "faster than Tokio" claim unless numbers actually support it.

## Rock 5: Local-Service API Completeness

Make the core Tina app surface complete enough for normal local services.

Support table:

| Capability | Piet default |
|---|---|
| runtime-owned time | Supported. |
| runtime-owned TCP | Supported. |
| Tower bridge | Supported and canonical for HTTP-edge integration. |
| Axum adapter/proof | Supported as first adapter on Tower. |
| health/readiness/shutdown/metrics | Supported. |
| live test harness | Supported. |
| simulator/DST scenario harness | Supported where Tina semantics are simulated. |
| DNS/TLS/UDP/file/process/signal | Jelle Zijlstra. |
| durable state/persistence | Wim Kok. |

Decide and implement the minimum supported local service set:

- startup/app builder;
- typed root registration;
- capacity config;
- bridge host;
- health/readiness;
- graceful shutdown;
- service-local metrics;
- test harness for live and simulator runs;
- simulation/DST scenario builder;
- supervision policy/budget ergonomics;
- trace retention and sink config;
- runtime-owned time/TCP helpers;
- explicit deferral table for DNS, TLS, UDP, file, process, signal, and durable
  state. DNS/TLS/UDP/file/process/signal move to Jelle Zijlstra unless a
  cross-cutting Piet workload proves one is required now. Durable state moves
  to Wim Kok.

API discipline:

- one preferred path;
- macros may remove ceremony, but must not create a second mental model;
- helpers must keep timeout, capacity, address, and effect semantics visible;
- no old aliases unless needed by Rust coherence or macro expansion internals.

Proof:

- canonical app tests use only the preferred public path;
- examples compile under the same path;
- compile-fail tests catch the most likely user mistakes;
- README examples, if touched, match tested code rather than invented snippets.

## Cross-Cutting E2E Workloads

Build a small suite of real-ish local service workloads that exercise all five
rocks together:

- `llama_http_bridge_service`: Tower canonical service plus Axum adapter,
  overload, cancel/timeout, metrics, readiness, and graceful shutdown;
- `llama_tcp_timer_service`: runtime-owned TCP plus timer in a local Tina
  service, with cancellation and shutdown proof;
- `llama_supervised_worker_service`: child panic, supervisor restart,
  policy/budget, stale address rejection, and terminal trace;
- `llama_sim_dst_parity_service`: simulator/DST proof of the same service logic
  where the live behavior has a deterministic oracle.

These are not marketing demos. They are user-shaped regression tests.

## Pause Gates

Pause and update the plan if any of these happen:

- bridge design requires async isolate handlers;
- runtime substrate work turns into a general async runtime;
- adding DNS/TLS/file/process/signal expands beyond local-core needs;
- performance numbers show a structural cost problem rather than a tuning issue;
- completion-slot pooling/slabbing wants unsafe code;
- bridge adapters create hidden queues or hidden tasks;
- MPSC fallback becomes necessary for the local service claim;
- cross-shard live isolate calls become necessary for the bridge story;
- broader I/O starts expanding beyond time/TCP/bridge before the local core is
  boring; that work belongs in Jelle Zijlstra unless forced by a Piet workload.

## Done Means

- One local production runner path is canonical and tested.
- Runtime lifecycle, shutdown, cancellation, and worker failure surfaces have
  direct live tests.
- Axum/Tower bridge supports realistic local services, not just trivial request
  handlers.
- Bridge backpressure, cancellation, retry, health, and shutdown semantics are
  tested from a user perspective.
- CI exists and runs the intended workspace gate.
- Stress/loom/miri/doc/compile-fail testing posture is named and runnable.
- Performance/allocation envelope exists with numbers and narrow claims.
- Performance/allocation results are summarized in this phase's `review.md`.
- Local-service API completeness table says what is supported and deferred.
- `llama_http_bridge_service`, `llama_tcp_timer_service`,
  `llama_supervised_worker_service`, and `llama_sim_dst_parity_service` prove
  the preferred path.
- Remaining non-claims are narrower than at phase start and moved into the
  roadmap with exact homes.

## Non-Claims After This Phase

Even if Piet succeeds:

- Tina is still not a general-purpose Tokio replacement.
- Tina is still not remoting/clustering/persistence.
- Tina still does not run arbitrary async ecosystem code inside isolates.
- Tina still does not claim broad performance superiority.
- Gemini remains blocked unless this phase closes cleanly and the core feels
  ready enough to document without apologizing every page.
