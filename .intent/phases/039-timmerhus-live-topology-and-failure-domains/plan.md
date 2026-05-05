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
   A gracefully stopped runtime and a failed runtime should not be collapsed
   into one vague "closed" state.

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

Queue report contract:

- `capacity` is required for every bounded queue the report names;
- accepted/rejected/closed/full counters are required where those outcomes are
  already observable by the runtime;
- exact depth is optional and must be shaped as `Option<usize>` or a clearly
  named sampled/last-known field;
- no field may imply exact live depth unless exact by construction;
- reports must be snapshots, not a promise that the value is still true after
  the call returns.

### 3. Pin Shard Lifecycle Vocabulary

Add or clarify states with observable transitions:

- `Running`;
- `Stopped`;
- `Failed`.

Expected 039 public vocabulary:

- after successful construction/start, shards report `Running`;
- after graceful shutdown completes, shards report `Stopped`;
- after worker panic or unrecovered worker failure, the shard reports `Failed`;
- if a drain API lands in this phase, `Draining` may be added only while the
  runtime has actually stopped accepting new work and is finishing accepted
  work;
- if construction has a separately observable pre-running phase, `Starting`
  may be added, but do not add it only because it sounds complete.

Default: 039 should ship `Running`, `Stopped`, and `Failed`. `Starting` and
`Draining` are deferred unless implementation makes them honest and directly
tested.

The distinction between graceful shutdown and worker failure must be visible in
the final report.

### 4. Make Queue Pressure Queryable

Expose bounded pressure without racing into lies:

- ingress command queue capacity;
- exact or sampled ingress depth only if shaped honestly;
- remote shard-pair capacity;
- exact or sampled remote depth only if shaped honestly;
- storage lane capacity/current accepted pending where available;
- accepted count;
- rejected count by reason where available;
- closed/full count where available.

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

- graceful shutdown: stop accepting new ingress, finish accepted ready work as
  far as the contract allows;
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

Current live cross-shard isolate calls reject.

Default 039 direction: keep rejection as the local-runtime rule and make it
sharper. The rejection must be typed, traced, and documented as "local
cross-shard isolate-call reply transport is not implemented in Timmerhus."

Only switch to implementation if the initial audit proves a same-process local
workload already needs cross-shard request/reply and the implementation can stay
simple:

- source sends call request through bounded shard-pair queue;
- destination delivers request to target;
- target reply returns through bounded reply transport;
- timeout remains mandatory;
- requester stop/timeout/full rejects late replies visibly;
- failed/draining shards surface typed outcome;
- no hidden unbounded pending map;
- simulator and live projections agree.

Pause before implementation if choosing transport. Amend this plan first with
exact queue, timeout, late-reply, failure, shutdown, topology-report, and
projection rules. Do not half-claim it.

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

Minimum histories:

- `topology_failure_history`: generated simulator history that starts shards,
  routes work, fills at least one bounded queue, stops one shard, fails one
  shard, and verifies visible terminal topology;
- `live_topology_failure_history`: matching live/native history over
  `LocalApp` or `BetelgeuseBackedMultiShardRuntime` that compares the
  projection with simulator semantics;
- `composed_service_history`: one user-shaped service with TCP ingress,
  cross-shard state, storage persistence, timeout, overload, and shutdown;
- `worker_panic_history`: worker panic on one shard while another shard either
  continues processing or shuts down cleanly;
- `shrinkable_topology_history`: deletion-shrink proof for a topology/failure
  model, with the reduced history printed on failure.

Projection must include:

- accepted user-visible values, not only event counts;
- rejected counts grouped by reason;
- terminal shard state per shard;
- queue pressure summary at terminal report;
- pending-work-zero at shutdown;
- durable image/journal result for persistence workloads;
- absence of mutation after rejection, timeout, cancellation, stopped shard, or
  failed shard.

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

Required e2e tests:

- `local_app_topology_report_before_and_after_shutdown`: report is useful while
  running and after terminal state;
- `live_ingress_pressure_reports_capacity_and_full_counter`: bounded ingress is
  forced full without sleeps and the topology/report shows the pressure;
- `remote_queue_pressure_reports_capacity_and_full_counter`: bounded
  shard-pair queue is forced full and rejection remains visible;
- `failed_worker_marks_one_shard_failed_and_rejects_later_work`: panic one
  worker, prove later ingress rejects, prove topology changes, prove another
  shard does not hang;
- `graceful_shutdown_accounts_for_timers_tcp_storage_and_remote_queues`: user
  workload starts all pending-work classes, then shutdown proves no hidden work;
- `composed_tcp_state_storage_overload_live_matches_sim_projection`: user-style
  service exercises TCP ingress, state shard, storage shard, overload, timeout,
  and shutdown against simulator projection;
- `cross_shard_isolate_call_rejects_with_typed_contract`: if rejection remains
  the 039 rule, prove the exact outcome/trace from user-facing call syntax;
- `topology_failure_history_shrinks`: Stuga deletion shrinker reduces a
  failing topology/failure predicate to a smaller history.

### 11. Stuga Long-Run Rails

Use Stuga instead of bespoke loops:

- add `TINA_DST_LONG=1` coverage for Timmerhus histories if they are not too
  slow;
- make failure output include seed/history/projection mismatch;
- add deletion shrink proof for at least one topology/failure model.

### 12. Documentation And Project Rules

There is no repo-local `SYSTEM.md` in this worktree. Do not invent one.
Record landed project truths in the existing project artifacts:

- live topology report meaning;
- shard lifecycle vocabulary;
- graceful vs failed shutdown semantics;
- whether local cross-shard isolate calls are implemented or rejected;
- native/live differential testing is projection-based, not raw-trace based.

Update `CHANGELOG.md` and `ROADMAP.md` at closeout.

## Proof Set

Minimum proof before closeout:

Testing rule:

- normal tests must cover known positive, negative, edge, and regression rocks
  directly; DST does not excuse happy-path-only tests;
- DST then composes those known semantics into weird orderings and shrinks the
  failures it finds.

- `cargo test -p tina-runtime --test betelgeuse_substrate`
- `cargo test -p tina-runtime --test local_app`
- `cargo test -p tina-sim --test betelgeuse_parity`
- new live topology/failure tests listed below;
- new native/live differential tests listed below;
- `TINA_DST_LONG=1 cargo test -p tina-sim dst_long` if the long suite remains
  reasonable;
- `make verify`.

Expected new/updated tests:

- `local_app_topology_report_before_and_after_shutdown`;
- `live_ingress_pressure_reports_capacity_and_full_counter`;
- `remote_queue_pressure_reports_capacity_and_full_counter`;
- `failed_worker_marks_one_shard_failed_and_rejects_later_work`;
- `direct_send_after_stop_is_known_negative_contract`;
- `remote_full_burst_is_known_edge_contract_and_replays`;
- `remote_full_burst_history_shrinks`;
- `seeded_random_single_shard_histories_replay_and_keep_trace_invariants`;
- `seeded_random_multishard_histories_replay_and_keep_remote_pressure_visible`;
- `cross_shard_send_to_failed_or_stopped_shard_is_visible`;
- `graceful_shutdown_accounts_for_timers_tcp_storage_and_remote_queues`;
- `composed_tcp_state_storage_overload_live_matches_sim_projection`;
- `live_sim_projection_matches_topology_failure_history`;
- `native_stress_keeps_bounded_pressure_visible`;
- `cross_shard_isolate_call_rejects_with_typed_contract`;
- `topology_failure_history_shrinks`.

## Done Means

- Live topology and shard lifecycle are reportable from the canonical app path.
- Queue reports expose stable capacities and honest counters/depth fields
  without false precision.
- Worker panic and graceful shutdown produce distinct visible terminal states.
- Cross-shard send behavior under running/stopped/failed/full targets is
  directly tested.
- Local cross-shard isolate-call reply transport is explicitly rejected with a
  sharper typed/tested contract, unless the plan was paused/amended before
  implementing bounded transport.
- Native/live stress tests cover bounded ingress, remote queue pressure, worker
  failure, pending work, shutdown, TCP, storage, and timers.
- Stuga histories/projections cover topology failure, composed service behavior,
  worker panic, shrinking, terminal pressure, and mutation-after-rejection
  absence.
- No hidden unbounded queues are introduced.
- `make verify` passes.

## Pause Gates

Pause and discuss if:

- cross-shard isolate-call reply transport starts looking like remoting;
- the initial audit argues to implement cross-shard isolate-call transport
  instead of the default sharpened rejection;
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
