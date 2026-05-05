# Phase 039: Timmerhus Plan Review

Verdict: ready to hand off to implementation, with the usual grug caution
around cross-shard call scope.

The hostile review originally found four load-bearing gaps. The plan now pins
all four:

1. Queue pressure cannot fake precision. `LiveQueueReport` must expose stable
   capacity, observable counters, and optional/sampled depth only when named
   honestly.
2. Lifecycle states are tied to observable transitions. The default 039 public
   vocabulary is `Running`, `Stopped`, and `Failed`; `Starting` and `Draining`
   are deferred unless real and directly tested.
3. Cross-shard isolate-call reply transport has a default direction. 039 keeps
   sharpened typed rejection unless implementation pauses and amends the plan
   with exact bounded transport semantics before coding.
4. Native/live DST and e2e proof are concrete. The plan now requires named
   histories, projections, shrink proof, and user-shaped e2e tests covering TCP
   ingress, cross-shard state, storage persistence, overload, timeout,
   shutdown, worker panic, queue pressure, and mutation-after-rejection absence.

What looks strong:

- The phase is still local-runtime completion, not remoting/clustering creep.
- It uses Stuga as first-class proof machinery instead of one-off loops.
- It keeps native scheduling honest: projection equality, not raw trace
  determinism.
- It preserves Tina's core shape: bounded queues, visible failure, replayable
  races, shared-nothing shard ownership.

Implementation watch points:

- Do not add hot-path locks just to make queue depth look nicer.
- Do not let cross-shard isolate-call transport sneak in without the required
  pause/amend step.
- Do not use sleeps as proof in live stress tests.
- Do not let topology reports become a control plane.
- Keep DNS/TLS/UDP/process/signal and broader storage runtime work in
  Funkishus unless Timmerhus discovers a true prerequisite.

## Implementation Review

Verdict: implemented and ready to close.

What landed:

- `LocalApp::topology()` and `LocalMultiShardApp::topology()` report the live
  shard set from the canonical app path.
- `LocalAppTerminalReport::topology()` preserves the terminal topology after
  shutdown consumes the app owner.
- `LiveShardState` is intentionally small: `Running`, `Stopped`, `Failed`.
  No fake `Starting` or `Draining` state landed.
- `LiveQueueReport` exposes stable capacity, optional depth, and optional
  counters. Unknown counters are `None`, not fake zero. Exact depth remains
  `None` because the live bounded queues do not expose exact depth without
  adding hot-path synchronization.
- Betelgeuse-backed workers are named `tina-shard-{id}` and report per-shard
  lifecycle. Multi-shard shutdown preserves a failed shard as `Failed` while a
  healthy joined shard reports `Stopped`.
- Live multi-shard remote transport now has source/target queue-pressure
  counters for accepted, full, and closed outcomes.
- Cross-shard isolate-call transport stayed rejected; the existing typed
  `TargetClosed` proof remains the 039 contract.
- Timmerhus DST adds a true live-vs-simulator projection over the same
  topology/failure history and deletion-shrinks the failure model.

Tests added or strengthened:

- `local_app_topology_report_before_and_after_shutdown`
- `live_ingress_pressure_reports_capacity_and_full_counter`
- `remote_queue_pressure_reports_capacity_and_full_counter`
- `failed_worker_marks_one_shard_failed_and_rejects_later_work`
- `direct_send_after_stop_is_known_negative_contract`
- `remote_full_burst_is_known_edge_contract_and_replays`
- `remote_full_burst_history_shrinks`
- `live_sim_projection_matches_topology_failure_history`
- `topology_failure_history_shrinks`
- `seeded_random_single_shard_histories_replay_and_keep_trace_invariants`
- `seeded_random_multishard_histories_replay_and_keep_remote_pressure_visible`

Test posture:

- normal tests now pin the known ugly rocks directly: stopped target, closed
  ingress, bounded remote full, worker failure, shutdown, and topology reports;
- DST is used on top of that to compose the same semantics into weird histories,
  replay them, and shrink failures. It is not used as a substitute for direct
  negative-path tests.

Existing user-shaped tests still cover composed TCP ingress, cross-shard state,
storage persistence, storage overload, timers, shutdown accounting, and
cross-shard isolate-call rejection.

Remaining non-claims:

- No exact live queue depth.
- No observable drain state.
- No local cross-shard isolate-call reply transport.
- No remoting, clustering, DNS/TLS/UDP/process/signal, or nonblocking storage
  reactor.

Verification:

- `cargo +nightly test -p tina-runtime --test local_app -p tina-sim --test timmerhus_dst`
- `cargo +nightly test -p tina-sim --test dst_randomized`
- `cargo +nightly test -p tina-runtime -p tina-sim`
- `make verify`

## Three-Part Closeout Review

Verdict: no blocking findings.

### Positive Review

- Timmerhus adds the right thing at the right layer: topology lives on the
  canonical local app/runtime owner, not in a sidecar debug API.
- The public vocabulary is small and honest. `Running`, `Stopped`, and
  `Failed` are all observable; fake `Starting`, `Draining`, or exact queue
  depth did not land.
- Queue reports preserve Tina's taste: bounded capacity is visible, pressure is
  counted where measured, and unmeasured fields are `None` instead of fake
  certainty.
- Worker names and terminal topology snapshots make the local thread-per-core
  story much easier to inspect after shutdown or failure.
- The tests now follow the right layering: direct normal tests for known bad
  rocks, then DST for weird combinations and shrinking.

### Blast Radius Review

- Public API changes are additive: new report types and `topology()` accessors.
  Existing runtime, simulator, bridge, mailbox, call, TCP, storage, and
  supervision surfaces are not renamed or behaviorally reshaped.
- Metrics use atomics and snapshots. No hot-path locks were added to chase exact
  queue depth.
- Existing shutdown behavior remains: graceful shutdown returns retained trace;
  failed shutdown returns a failed report. Timmerhus adds terminal topology,
  but does not invent a drain control plane.
- Multi-shard live transport still uses the bounded target-worker command queue
  as the live shard-pair pressure point. The plan and tests name this honestly;
  no hidden dedicated remote queue was implied.
- Cross-shard isolate-call reply transport remains rejected. Timmerhus did not
  smuggle in remoting-shaped request/reply semantics.

### Hostile Red Review

- Tried to find false precision in queue reports. Result: exact depth remains
  `None`, storage-lane counters are `None`, and measured ingress/remote counters
  are explicit.
- Tried to find hidden happy-path-only DST. Result: randomized DST sweeps now
  fail if they stop hitting `Full`, `Closed`, panic, or timer rocks; known
  `Closed` and remote `Full` contracts are pinned directly.
- Tried to find worker-failure poisoning. Result: a failed shard is marked
  `Failed`, healthy sibling shard continues, and terminal topology preserves
  both states.
- Tried to find live-vs-sim overclaim. Result: Timmerhus compares semantic
  projections, not raw native traces.
- Honest limitation: topology is handle-observed state, not an active health
  probe. If a worker dies and no command/shutdown observes it yet, topology does
  not claim clairvoyance. That is acceptable for 039 and should remain explicit
  until a later health-monitoring phase exists.
