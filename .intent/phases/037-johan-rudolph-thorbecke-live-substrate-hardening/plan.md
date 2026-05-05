# 037 Johan Rudolph Thorbecke Live Substrate Hardening Plan

## Purpose

Make Tina's local live substrate boring enough for experimental production-ish
use.

Tina now has the shape we wanted: bounded mailboxes, deterministic simulator,
runtime-owned TCP/time/file calls, local persistence, `LocalApp`, and a narrow
Tokio bridge. But the next bottleneck is not more convenience syntax. The next
rock is making the live runtime/driver/storage story harder to misuse:

> A user should be able to run a real local Tina service across multiple live
> shards, do runtime-owned I/O and persistence, overload it, shut it down, and
> see bounded, traceable behavior instead of hidden blocking, hidden work, or
> mystery queues.

This phase is core framework work. It deliberately combines runtime/substrate
hardening, I/O/storage production shape, and live thread-per-core proof because
the same rules decide all three: ownership, cancellation, backpressure,
shutdown, and what work is allowed to block a shard.

## Why This Comes Before Barend

Barend Biesheuvel flow ergonomics is useful, but it is syntax over the runtime
truth. Thorbecke must go first so the macro does not prettify weak semantics.

After this phase, Barend can compress ceremony around a substrate whose
blocking, cancellation, overload, and shutdown behavior is already named and
tested.

## Starting Baseline

Current Tina has:

- `LocalApp` as the preferred live owner;
- a Tina-owned driver boundary for time, TCP server/client operations, local
  file I/O, and local persistence helpers;
- native Betelgeuse-backed and simulated Betelgeuse driver adapters;
- bounded live ingress and bounded live cross-shard transport;
- same-resource lane ownership for TCP/file-style calls;
- snapshot/journal helpers with visible corrupt/truncated/commit-uncertain
  outcomes;
- `tina-sim` replay for timers, TCP, file I/O, persistence image state, spawn,
  supervision, perturbation, and multi-shard behavior;
- bridge-facing bounded admission, cancellation, retry policy, and metrics;
- CI/workspace verify hooks via `make verify`.

Current missing rock:

- persistence and local file work can still execute synchronous filesystem work
  in the runtime driver path;
- no single contract says which runtime-owned calls may block a shard and which
  must go through a bounded lane;
- live multi-shard/thread-per-core behavior exists in pieces, but the preferred
  production-shaped service proof is not yet a multi-shard live workload with
  storage, I/O, overload, shutdown, and recovery in one user-shaped package;
- storage overload and slow storage behavior are not yet as visible as mailbox
  overload;
- terminal runtime reports are useful but not yet the complete "nothing hidden
  remains" proof for a service using I/O plus persistence;
- performance/allocation numbers exist for narrow paths, but not for the new
  storage/persistence service shape.

## Phase Thesis

The live substrate should obey these rules:

1. **Shard turns stay short by default.**
   Handler turns are synchronous, but driver work that can be slow must not
   silently monopolize the shard worker.

2. **Every bounded lane has visible overload.**
   Mailbox, ingress, shard-pair transport, bridge, storage lane, and driver
   command queues must reject as typed outcomes when full. No hidden overflow
   queue.

3. **Cancellation means no later mutation unless explicitly documented and
   traced.**
   Timed-out, stopped, canceled, or shutdown-owned work cannot later apply
   user-visible state changes silently.

4. **Shutdown is a proof, not a hope.**
   Graceful and hard shutdown must report what completed, what was canceled,
   and what was abandoned.

5. **Live thread-per-core means shared-nothing application state.**
   Shards may coordinate through bounded queues, but user isolate state remains
   owned by one shard worker.

6. **Simulator remains the oracle for replay.**
   Live runs can prove real substrate behavior and boundedness; simulator tests
   prove deterministic interleavings and replay.

## Accepted Scope

### Rock 1: Driver Blocking Audit And Contract

Audit every runtime-owned call family:

- time;
- TCP accept/connect/read/write/close;
- file create/open/read/write/fsync/size/close;
- snapshot commit/load;
- journal append/replay;
- bridge ingress calls where they cross into Tina.

Classify each operation:

- **inline-safe:** small bounded runtime bookkeeping only;
- **driver-completion:** completion-shaped I/O where backend owns readiness;
- **storage-lane:** may block on filesystem/durable work and must not run on
  the shard worker unchecked;
- **forbidden-in-handler:** anything users might try to call directly that
  would bypass Tina's visibility.

Add a short contract comment/doc section where the code owns these categories.

### Rock 2: Bounded Storage Lane

Introduce a bounded storage lane for file and persistence operations that can
block on local filesystem work.

Expected shape:

- one storage lane per `LocalApp` or per live shard, whichever the audit proves
  simpler and safer;
- bounded command capacity with typed `Full` outcome;
- typed `Closed` outcome after shutdown begins;
- per-call timeout/cancellation tied to the requester;
- terminal accounting for completed, canceled, rejected, and still-owned work;
- no unbounded `mpsc` fallback and no hidden worker queue.

The storage lane may use a standard blocking thread internally. That is allowed
because the product is not "no threads"; the product is "shared-nothing Tina
state with visible bounded side effects." The lane must not become a general
task pool.

### Rock 3: Persistence Over Storage Lane

Move snapshot/journal helper execution onto the storage lane or prove a narrower
path is safe enough.

Required semantics:

- append-before-apply remains the preferred persistence discipline;
- journal monotonicity stays checked at append time;
- commit-uncertain remains explicit when durability cannot be proved after a
  visible rename/commit boundary;
- platform support table remains honest for directory fsync and rename replace;
- storage-lane full/closed/timeout/canceled outcomes are traceable as
  persistence failures, not swallowed as generic I/O.

### Rock 4: Live Multi-Shard Service Proof

Build one user-shaped live workload that exercises the real local substrate:

- external ingress through `LocalApp` or the Tokio bridge;
- at least two live shard workers;
- cross-shard bounded send to a worker/service isolate;
- runtime-owned TCP or file I/O;
- snapshot or journal persistence;
- reply back to the caller;
- graceful shutdown and restart/recovery.

This workload should be small enough to maintain, but real enough that a user
can recognize the shape of a service they might port from Tokio.

### Rock 5: Overload Proof Matrix

Force every relevant queue/lane to fill without sleep-as-proof:

- live ingress full;
- mailbox full;
- cross-shard transport full;
- storage lane full;
- bridge admission full;
- requester stopped while storage work is pending;
- shutdown while storage work is pending;
- timeout while storage work is pending.

Each case must assert a typed outcome and the expected trace event. The proof
must fail if an unbounded queue is inserted under the covers.

### Rock 6: Shutdown And Cancellation Proof

Pin both graceful and hard shutdown:

- graceful shutdown accepts no new work, drains already accepted work up to a
  bounded rule, then reports terminal state;
- hard shutdown cancels owned driver/storage work and reports what was
  abandoned;
- no pending completion slot, storage command, bridge request, or driver call
  remains unaccounted for;
- late backend completions after cancellation are swallowed or rejected with a
  named trace shape, never applied silently.

### Rock 7: Simulator Oracle Parity

Keep simulator meaning aligned:

- if live storage lane introduces new outcomes, simulator persistence/file
  oracle must model the same visible outcomes where meaningful;
- simulator remains deterministic and replayable;
- live-only scheduling variation is documented as live-only, not normalized into
  simulator semantics;
- one service-shaped scenario should have both simulator and live proof, even if
  live has additional OS scheduling facts.

### Rock 8: Cost And Pressure Numbers

Add narrow measurement tests or reports for:

- storage-lane command admission;
- snapshot commit round trip;
- journal append round trip;
- live multi-shard send under warmed runtime;
- live service request under no-overload path;
- overload rejection path.

No broad "faster than Tokio" claim. The goal is to know where allocations and
latency are currently paid, and to prevent accidental cliffs.

## Non-Goals

- No flow macro implementation.
- No remoting.
- No clustering.
- No durable mailboxes.
- No durable work queue.
- No exactly-once claim.
- No broad database abstraction.
- No Tower/Axum expansion except where existing bridge tests need to prove
  lifecycle/overload behavior.
- No DNS/TLS/UDP/process/signal implementation unless the audit discovers a
  tiny required runtime contract fix. Those remain deferred I/O breadth work.
- No performance marketing.
- No new general-purpose async runtime.

## Build Steps

1. Audit current driver/runtime call paths and write the operation category map
   into the plan's review or a small code-facing doc comment.
2. Add/adjust driver contract types so call results can distinguish storage
   lane `Full`, `Closed`, `Timeout`, and `CommitUncertain` where
   relevant.
3. Implement bounded storage-lane ownership in `tina-runtime`.
4. Route file/persistence operations that can block through the storage lane.
5. Preserve simulator file/persistence behavior and add simulator-visible
   outcomes where the live contract gains new visible states.
6. Add storage-lane unit tests for bounded admission, close, timeout,
   requester-stop cancellation, and shutdown cancellation.
7. Add persistence regression tests over the storage lane: append,
   duplicate/out-of-order rejection, snapshot commit, commit uncertain,
   truncated/corrupt replay, current-directory paths, restart recovery.
8. Build the live multi-shard service proof.
9. Add the overload proof matrix.
10. Add graceful/hard shutdown proofs with terminal reports.
11. Add narrow cost/pressure tests or report fixtures.
12. Run `make verify`.
13. Update `SYSTEM.md`, `ROADMAP.md`, and `CHANGELOG.md` only for landed
    committed constraints.

## Required Tests

- `cargo test -p tina-runtime --test storage_lane` or equivalent:
  - bounded lane full without sleep-as-proof;
  - lane closed rejects new work;
  - requester stop cancels pending storage work;
  - timeout cancels pending storage work;
  - hard shutdown accounts for pending storage work;
  - graceful shutdown terminal report accounts for accepted storage work.
- `cargo test -p tina-runtime --test persistence` remains green and adds:
  - persistence helpers execute through the bounded storage path;
  - storage overload becomes visible persistence failure;
  - late storage completion after requester cancellation does not mutate state.
- `cargo test -p tina-runtime --test live_multishard_service` or equivalent:
  - two or more live shard workers;
  - bounded cross-shard send;
  - runtime-owned I/O;
  - persistence;
  - reply;
  - graceful shutdown;
  - restart/recovery.
- `cargo test -p tina-runtime --test local_app_end_to_end_service` or
  equivalent must be the named whole-path proof:
  - start `LocalApp` with two or more live shards;
  - register a frontend isolate and a worker/persistence isolate;
  - accept user-shaped requests through the public app API or bridge;
  - route work across shards;
  - perform a runtime-owned file or TCP call;
  - append journal before applying state;
  - return typed success/failure to the caller;
  - overload each bounded point that belongs to the path and assert visible
    `Full`, `Closed`, or `Timeout`;
  - shut down while work is pending and assert the terminal report;
  - restart a fresh app and recover from snapshot/journal;
  - compare simulator oracle shape where meaningful.
- `cargo test -p tina-sim --test persistence_simulation` or equivalent:
  - simulator image/replay remains deterministic under new outcomes.
- `cargo test -p tina-sim --test multishard_dispatcher` must include one
  service-shaped DST proof:
  - scripted TCP enters one simulated shard;
  - the request crosses to a second shard;
  - the second shard appends the journal;
  - the durable ack crosses back;
  - peer-visible TCP output happens only after `JournalAppended`;
  - same-seed perturbation reruns to the exact same multi-shard replay
    artifact;
  - a checker fails the run if TCP write is observed before durable append.
- It must also include a harder seed-sweep workload that shakes normal and
  weird pressure together:
  - multiple overlapping scripted clients;
  - small read chunks;
  - partial TCP writes;
  - many cross-shard persistence requests;
  - monotonic journal indexes assigned by the storage isolate;
  - exact replay artifact equality across repeated runs for several seeds.
- It should include randomized deterministic-history DST tests where useful:
  - single-shard generated histories with delayed local sends, delayed timers,
    stop, panic, mailbox pressure, and stale sends;
  - multi-shard generated histories with bounded remote queues, burst pressure,
    closed/unknown remote targets, and reply traffic;
  - every generated history reruns with the same seed and requires identical
    replay artifacts plus trace-causality invariants.
- Bridge tests:
  - bridge overload while storage is busy remains visible;
  - bridge timeout/cancel does not produce hidden later mutation unless a named
    semantic says it can and tests assert that trace.
- `make verify` must pass.

## Review Traps

- A storage lane backed by an unbounded channel.
- A helper named `try_*` that can block after admission.
- A timeout that only stops waiting while accepted work still mutates state
  later without a named trace.
- A shutdown path that drops completion slots or storage work without terminal
  accounting.
- A persistence helper that reports success before durable/uncertain semantics
  are pinned.
- A live multi-shard proof that only uses explicit-step runtime or simulator.
- A simulator proof used as a substitute for live boundedness.
- A benchmark-like number without a semantic assertion.
- Any macro or helper that hides `Full`, `Closed`, `Timeout`, or
  `CommitUncertain`.

## Done Means

- Tina has a named blocking/driver/storage contract.
- File and persistence work that can block no longer silently stalls the shard
  worker in the preferred live path.
- Storage overload is as visible as mailbox overload.
- Shutdown and cancellation leave no hidden accepted work behind.
- One live multi-shard service proves ingress, cross-shard work, runtime-owned
  I/O, persistence, overload, reply, shutdown, and recovery in a user-shaped
  path.
- Simulator parity remains honest and replayable.
- Cost/pressure evidence covers the new storage/live-service paths.
- `make verify` passes.

## Non-Claims After This Phase

Even if Thorbecke succeeds:

- Tina is still not a general Tokio replacement.
- Tina still does not provide remoting, clustering, distributed consensus, or
  durable mailboxes.
- Tina still does not claim broad throughput superiority.
- Tina still does not support every server I/O family.
- Barend flow ergonomics remains future work.
- Gemini release story remains blocked until Tina's core feels boring under
  real app pressure.

## Implementation Notes

First Thorbecke implementation slice landed the core storage/live-service
rocks:

- persistence helpers route through a bounded storage lane instead of running
  synchronously inside the shard worker on the preferred live path;
- storage capacity is configurable through `BetelgeuseBackedRuntimeConfig` and
  `LocalApp` builders;
- storage admission and lifecycle failures have named `CallError` variants:
  `StorageFull` and `StorageClosed`;
- direct explicit-step runtime storage remains inline, while live single-shard
  and multi-shard worker paths use the bounded storage lane;
- storage-lane tests prove full rejection without sleep-as-proof, cancellation
  swallowing late completions, and shutdown skipping buffered work that never
  started;
- `local_app_end_to_end_service` proves public app ingress, cross-shard routing,
  journal append before state apply, shutdown, fresh-app recovery, and recovery
  trace visibility.
- `local_app_tcp_service_journals_before_replying_to_client` begins the next
  slice by composing real TCP, live storage-lane persistence, shutdown, and
  journal replay in one user-shaped `LocalApp` service.
- `LocalAppTerminalReport::summary()` adds trace-derived terminal accounting
  for completed, failed, rejected, abandoned, journaled, and recovered work;
  the composed live proofs assert the summary instead of leaving shutdown
  evidence as raw trace archaeology only.
- `terminal_summary_scans_trace_without_allocating` pins the summary helper as
  zero-allocation over retained trace.
- Storage-lane capacity now counts total accepted pending work, not only
  buffered channel slots, and
  `local_app_storage_lane_full_is_user_visible_without_sleep_as_proof` proves
  one accepted journal append plus one deterministic `StorageFull` from a
  normal `LocalApp` service.
- `local_app_cross_shard_tcp_request_persists_before_client_reply` is the
  full live thread-per-core proof: TCP ingress on shard A, cross-shard durable
  journal append on shard B, ack back to shard A, client reply after
  persistence, shutdown, and journal replay.
- `multishard_tcp_persistence_service_replays_under_seeded_dst_faults` is the
  simulator partner proof: scripted TCP ingress, cross-shard persistence,
  checker-enforced persist-before-reply ordering, and bytewise replay under
  seeded TCP/local-send perturbation.
- `multishard_tcp_persistence_service_handles_overlap_partial_io_and_seed_sweep`
  makes that simulator proof meaner: three overlapping clients, chopped reads,
  partial writes, multiple journal records, cross-shard request/reply counts,
  monotonic replayable indexes, and exact same artifact on repeated runs for
  several seeds.
- `dst_randomized.rs` adds non-scenario DST pressure: generated single-shard
  and multi-shard histories under seeded perturbation, exact replay, visible
  send outcomes, causal trace checks, and stopped-isolate liveness checks.
- `simulator_random_persistence_matrix_replays_and_keeps_journal_recoverable`
  adds a persistence fault matrix: generated mutations, bad append indexes,
  snapshots, recovery calls, stepping, exact replay, and fresh recovery from
  the resulting durable image.
- `simulator_supervision_persistence_recovers_after_durable_append_then_panic`
  composes supervision and persistence: durable append happens, the child
  panics, supervised restart runs recovery, and state returns from the journal.
- `seeded_tcp_cancellation_matrix_replays_and_tombstones_late_completions`
  shakes pending accept/read/write cancellation under seeded TCP delay and
  proves requester-stop tombstones do not leave in-flight work behind.
- `send_stop_workload_matches_oracle_simulator_and_betelgeuse_runner` extends
  live-vs-sim differential proof for send/stop/closed-rejection semantics
  across explicit runtime, simulator, and Betelgeuse-backed runner.
- `bridge_ingress_model_dst_keeps_timeout_from_mutating_service_state` models
  bridge ingress as a bounded queue with timeout cancellation and proves
  timed-out queued work is skipped instead of mutating service state.
- `dst_history_shrinker_keeps_replayable_failure_but_removes_noise` adds a
  small minimization proof so future generated DST failures can become smaller
  repro rocks instead of huge histories.

Remaining Thorbecke pressure, if future review wants the whole phase closed in
one more pass:

- broader storage/file operation classification beyond snapshot/journal;
- explicit graceful-vs-hard terminal accounting counters for storage work;
- storage-lane cost/pressure numbers beyond semantic full/cancel proofs;
- a larger live service proof that combines TCP and persistence in one path.
