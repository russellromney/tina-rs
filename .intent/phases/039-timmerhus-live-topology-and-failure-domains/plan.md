# Phase 039: Timmerhus Live Topology And Failure Domains

## Goal

Make Tina's local live runtime feel boring under real thread-per-core pressure.

Timmerhus is the phase where the live multi-shard substrate stops being "we
can run several shard workers" and becomes a named local topology with visible
ownership, visible lifecycle, visible failure, bounded cross-shard behavior,
and native/live proof that lines up with the deterministic simulator.

This is a production-readiness phase, not a demo phase.

At closeout, Tina should be able to say:

> A local Tina app can run fixed shared-nothing shards on worker threads,
> explain which shard owns what, route bounded cross-shard traffic, surface
> overload and failure as typed outcomes/trace events, shut down without hidden
> pending work, and compare its live behavior against simulator projections.

## Why Now

Stuga gave Tina a reusable DST nucleus:

- histories as data;
- replay assertions;
- deletion shrinking;
- common invariants;
- simulator storage faults;
- live-vs-sim projection comparison.

Now we should use that machinery on the next real runtime gap. More DST without
new runtime surface would be a nice cave painting. More live runtime surface
without DST would be fire near dry grass. Timmerhus is both: build the next
local runtime capability and make DST/native proof follow every step.

## Current Baseline

Already landed before Timmerhus:

- `BetelgeuseBackedRuntime` for one live shard.
- `BetelgeuseBackedMultiShardRuntime` for a fixed local shard set.
- bounded command ingress into worker threads.
- bounded cross-shard send transport.
- live cross-shard send proofs and live TCP+persistence proofs.
- storage lane for blocking file/persistence work.
- `LocalApp` as canonical local app owner.
- `tina_sim::dst` for reusable deterministic histories and projections.

Known gaps:

- live topology is not yet a first-class report;
- shard lifecycle vocabulary is too thin;
- worker panic vs graceful stop vs closed shard are not sharp enough;
- queue pressure and ownership are visible in scattered ways, not one topology
  surface;
- local cross-shard isolate-call reply transport is still not claimed;
- live-vs-sim differential proof exists but is narrow;
- native/live stress is not yet a standard closeout bar.

## Non-Goals

These are important, but not Timmerhus:

- no remoting or clustering;
- no remote node membership;
- no durable mailbox;
- no DNS/TLS/UDP/process/signal implementation;
- no nonblocking storage reactor;
- no public release/Gemini story;
- no flow macro ergonomics;
- no claim that native OS scheduling is deterministic.

## What Goes To The Next Phase

Timmerhus may discover pressure around I/O/storage. If the answer requires new
runtime-owned resource types or a storage substrate redesign, that belongs in
**Funkishus storage and I/O maturity**, not Timmerhus.

Funkishus should explicitly own:

- nonblocking storage reactor decision;
- platform durability hardening beyond current local persistence support;
- DNS/TLS/UDP/process/signal rails;
- richer file/resource cancellation;
- native driver adapter policy beyond the current Betelgeuse-backed shape;
- any I/O feature needed by a named local production workload but not needed to
  settle shard topology/failure domains.

Jan Peter Balkenende still owns real remoting. Mark Rutte still owns
clustering. Timmerhus must not smuggle those in.

## Design Principles

1. **Topology is data.**
   A live runtime should be able to report its shard set, worker ownership,
   lifecycle state, queue pressure, and terminal state without scraping logs.

2. **Failure is typed and traceable.**
   Worker panic, shard closed, shard failed, ingress full, remote queue full,
   target closed, and target full must be visible as typed outcomes and/or
   trace events.

3. **Bounded means bounded under native pressure.**
   Native worker scheduling cannot be deterministic, but it must not create
   hidden unbounded queues or hidden waits.

4. **DST compares projections.**
   Simulator and live runtime do not have identical raw traces. Timmerhus
   compares semantic projections: accepted work, rejected work, terminal state,
   no hidden pending work, and durable/visible output.

5. **Graceful and failed shutdown differ.**
   A drained runtime and a failed runtime should not be collapsed into one
   vague "closed" state.

6. **Do not make every runtime internal thread-safe.**
   Each shard remains owned by one worker thread. Cross-thread interaction goes
   through bounded command queues and explicit reports.

## Expected User Shape

The app-level surface should feel like this:

```rust
let app = LocalApp::multi_shard([AppShard::Ingress, AppShard::State, AppShard::Store])
    .entry("llama-feed")
    .shard_pair_capacity(64)
    .storage_lane_capacity(8)
    .start()?;

let report = app.topology();
assert_eq!(report.shards().len(), 3);

let service = app.register_on(AppShard::Ingress, LlamaIngress::new(...), 128)?;
let outcome = app.try_send(service, LlamaMsg::Feed("hay".into()))?;

let terminal = app.shutdown()?;
let summary = terminal.summary();
```

Exact names may differ if the current code shape demands it, but the phase
must produce a readable canonical app-owner story. Users should not have to
reach into worker internals to answer "what shard is alive and what pressure is
visible?"

## Build Steps

### 1. Audit Live Topology And Lifecycle Surface

Inventory current public/private live types:

- `BetelgeuseBackedRuntime`;
- `BetelgeuseBackedMultiShardRuntime`;
- `LocalApp`;
- bridge host integration;
- worker handles;
- shutdown reports;
- terminal report summaries;
- cross-shard send/rejection trace events.

Write the findings in `review.md`, not a separate audit file.

Done means we know the smallest public surface to add instead of inventing a
parallel management API.

### 2. Add Topology Report Types

Add a small report surface:

- local app/runtime topology id;
- shard id;
- worker thread name/id if available without platform tricks;
- shard lifecycle state;
- ingress queue capacity/pressure;
- remote shard-pair queue capacity/pressure;
- storage lane capacity/pressure where available;
- trace retention mode and dropped count if available.

Keep it read-only. No control plane in this step.

Expected rough names:

- `LiveTopologyReport`;
- `LiveShardReport`;
- `LiveShardState`;
- `LiveQueueReport`.

### 3. Pin Shard Lifecycle Vocabulary

Add or clarify states:

- `Starting`;
- `Running`;
- `Draining`;
- `Stopped`;
- `Failed`.

If current implementation cannot observe `Starting` or `Draining` honestly,
name the narrower states and say why. Do not invent fake states.

The distinction between graceful shutdown and worker failure must be visible in
the final report.

### 4. Make Queue Pressure Queryable

Expose bounded pressure without racing into lies:

- ingress command queue capacity;
- current or last-known ingress depth if available;
- remote shard-pair capacity;
- current or last-known remote depth if available;
- storage lane capacity/current accepted pending where available.

If exact live depth cannot be reported safely without new synchronization,
report stable capacity plus counters for accepted/rejected/failed work. Do not
add locks that hurt the shard hot path unless proof says it is worth it.

### 5. Harden Worker Panic And Failed Shard Semantics

When a worker panics:

- the handle must surface typed failure;
- later external ingress must reject visibly;
- peer shards must not silently accept work for a failed shard;
- topology report must mark the shard failed or terminal;
- shutdown must remain retryable/observable, not hang.

Add direct tests that force a shard worker panic while other shards continue or
shut down cleanly.

### 6. Harden Graceful Drain And Hard Stop

Define and test:

- drain: stop accepting new ingress, finish accepted ready work as far as the
  contract allows;
- hard stop/failure: reject new ingress and cancel/abandon pending owned work
  visibly;
- shutdown report: no hidden pending runtime calls, storage work, timers, TCP
  work, or cross-shard queue entries remain unaccounted.

If the current API only supports one shutdown mode, pin that honestly and add a
future note for a second mode.

### 7. Cross-Shard Send Under Lifecycle Changes

Prove source-time and destination-time behavior when:

- target shard is running;
- target shard is draining/stopped;
- target shard worker failed;
- remote queue is full;
- target isolate is closed/full/stale;
- source shard is shutting down while remote queues contain work.

The event vocabulary must preserve the Galileo/Thorbecke rule:
source-time queue admission and destination-time mailbox delivery are separate
stages.

### 8. Decide Local Cross-Shard Isolate-Call Reply Transport

This is the biggest decision in the phase.

Current live cross-shard isolate calls reject. Timmerhus must either:

1. keep rejection as the local-runtime rule and make it sharper; or
2. implement bounded local cross-shard isolate-call reply transport.

Expected direction: implement only if the bounded reply path can stay simple:

- source sends call request through bounded shard-pair queue;
- destination delivers request to target;
- target reply returns through bounded reply transport;
- timeout remains mandatory;
- requester stop/timeout/full rejects late replies visibly;
- failed/draining shards surface typed outcome;
- no hidden unbounded pending map;
- simulator and live projections agree.

Pause before implementation if this wants remoting-shaped complexity. Do not
half-claim it.

### 9. Native/Live DST Differential Harness

Add a "DST native" proof mode:

- generate or enumerate histories with Stuga `History`;
- run them against `tina-sim`;
- run corresponding workloads against explicit runtime where relevant;
- run corresponding workloads against live `LocalApp` /
  `BetelgeuseBackedMultiShardRuntime`;
- compare semantic projections with `tina_sim::dst::assert_projection_eq`.

This is not deterministic native scheduling. It is native/live differential
checking against deterministic simulator semantics.

Minimum projections:

- accepted values;
- rejected outcomes by reason;
- terminal shard states;
- pending-work count is zero at shutdown;
- durable journal/recovery result when persistence is in the workload;
- no mutation after rejected/cancelled work.

### 10. Native Stress Suite

Add live stress tests with short deterministic scripts:

- many concurrent external senders into bounded ingress;
- cross-shard remote queue pressure;
- worker panic while other shards send;
- shutdown while timers/TCP/storage/cross-shard sends are pending;
- repeated startup/shutdown cycles;
- at least one composed service with TCP ingress, state shard, storage shard,
  timeout, and overload.

No sleeps-as-proof. Use barriers/channels/explicit readiness where possible.
Wall-clock deadlines are allowed only as test failsafes.

### 11. Stuga Long-Run Rails

Use Stuga instead of bespoke loops:

- add `TINA_DST_LONG=1` coverage for Timmerhus histories if they are not too
  slow;
- make failure output include seed/history/projection mismatch;
- add deletion shrink proof for at least one topology/failure model.

### 12. Documentation And System Rules

Update `SYSTEM.md` only for landed rules:

- live topology report meaning;
- shard lifecycle vocabulary;
- graceful vs failed shutdown semantics;
- whether local cross-shard isolate calls are implemented or rejected;
- native/live differential testing is projection-based, not raw-trace based.

Update `CHANGELOG.md` and `ROADMAP.md` at closeout.

## Proof Set

Minimum proof before closeout:

- `cargo test -p tina-runtime --test betelgeuse_substrate`
- `cargo test -p tina-runtime --test local_app`
- `cargo test -p tina-sim --test betelgeuse_parity`
- new live topology/failure tests;
- new native/live differential tests;
- `TINA_DST_LONG=1 cargo test -p tina-sim dst_long` if the long suite remains
  reasonable;
- `make verify`.

Expected new/updated tests:

- `local_app_reports_live_topology_and_queue_pressure`;
- `failed_shard_rejects_later_ingress_and_marks_topology`;
- `cross_shard_send_to_failed_or_stopped_shard_is_visible`;
- `shutdown_accounts_for_pending_cross_shard_timer_tcp_and_storage_work`;
- `live_sim_projection_matches_topology_failure_history`;
- `native_stress_keeps_bounded_pressure_visible`;
- `cross_shard_isolate_call_transport_is_bounded_or_rejected_by_contract`.

## Done Means

- Live topology and shard lifecycle are reportable from the canonical app path.
- Worker panic and graceful shutdown produce distinct visible terminal states.
- Cross-shard send behavior under running/stopped/failed/full targets is
  directly tested.
- Local cross-shard isolate-call reply transport is either implemented and
  proved, or explicitly rejected with a sharper typed/tested contract.
- Native/live stress tests cover bounded ingress, remote queue pressure, worker
  failure, pending work, shutdown, TCP, storage, and timers.
- Stuga histories/projections cover the new semantics.
- No hidden unbounded queues are introduced.
- `make verify` passes.

## Pause Gates

Pause and discuss if:

- cross-shard isolate-call reply transport starts looking like remoting;
- topology reporting wants locks on hot paths;
- queue depth cannot be reported without lying;
- graceful drain requires a new public lifecycle API;
- native stress starts depending on sleeps for correctness;
- live-vs-sim projection hides a real semantic mismatch;
- this phase wants DNS/TLS/UDP/process/signal or nonblocking storage.

## Non-Claims After This Phase

Even if Timmerhus succeeds:

- Tina still does not have network remoting or clustering.
- Tina still does not claim deterministic native OS scheduling.
- Tina still does not have durable mailboxes.
- Tina still does not claim all Tokio ecosystem adapters.
- Tina still does not claim broad performance superiority.
- Native I/O/storage breadth beyond current TCP/file/persistence stays in
  Funkishus unless Timmerhus proves an immediate prerequisite.
