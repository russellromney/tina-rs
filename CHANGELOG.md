# Changelog

This file records completed work.

## Unreleased

### Phase Wim Kok

- Added Tina local persistence helpers: `snapshot_commit`,
  `snapshot_load`, `journal_append`, and `journal_replay`.
- Added durable snapshot metadata with `last_journal_index`, framed journal
  records with monotonic `record_index`, and append-before-apply recovery
  semantics.
- Added explicit persistence trace events for snapshot commit, journal append,
  recovery start/finish/failure, and snapshot/journal failures.
- Added `LOCAL_PERSISTENCE_SUPPORT` so platform durability claims are visible
  instead of implied.
- Added `JournalReplayWarning::TruncatedTail` for valid-prefix replay and
  `CallError::CorruptRecord` for complete records with bad checksums or
  unreplayable record index order.
- Added `CallError::CommitUncertain` for snapshot commits where rename already
  happened but the final durability step could not be proven.
- Added simulator `DurableImage` capture/load support so durable recovery can
  be replayed from deterministic path-to-bytes state.
- Proved live `Runtime`, canonical `LocalApp`, deterministic `tina-sim`, and
  Tokio bridge recovery paths for stateful services.
- Added negative proof that failed journal append is visible and does not
  mutate user state.

### Phase Piet de Jong

- Added `tina_runtime::LocalApp` as the canonical live app owner for local
  Tina services, with single-shard and multi-shard builders.
- Added lifecycle shutdown/reporting types: `LocalAppState`,
  `LocalAppTerminalReport`, `LocalAppShutdown`, and
  `LocalMultiShardAppShutdown`.
- Added `BridgeHost::from_app(app)` so bridge-hosted services can start from
  the canonical `LocalApp` path.
- Added retry policy support with both per-attempt timeout and total policy
  deadline.
- Added direct lifecycle failure proof for worker-side failure surfaced through
  `LocalAppTerminalReport`.
- Added production-shaped local service proofs:
  `llama_http_bridge_service`, `llama_tcp_timer_service`,
  `llama_supervised_worker_service`, and `llama_sim_dst_parity_service`.
- Added a narrow performance/allocation envelope for the preferred app ingress
  path and kept broader performance claims explicit non-claims.

### Phase Jelle Zijlstra

- Added outbound TCP connect to the vendored Betelgeuse native and simulated
  I/O backends.
- Added Tina runtime-owned `tcp_connect(addr)` and trace vocabulary for
  client-side TCP streams.
- Added runtime-owned file I/O to `tina-runtime`: `FileId`,
  `FileOpenOptions`, `file_open`, `file_create`, `file_read`,
  `file_read_at`, `file_write`, `file_write_at`, `file_fsync`, `file_size`,
  `file_close`, and `mkdir`.
- Added deterministic `tina-sim` file behavior for config/snapshot/log-shaped
  workloads, including replay-visible failures for invalid file resources and
  unsupported open modes.
- Proved live and simulated outbound TCP client flows, live and simulated file
  read/write/fsync/close flows, LocalApp-hosted file services, and
  Tokio-bridge-hosted file services.
- Added future roadmap home for visible sequential workflow ergonomics without
  hiding runtime-owned suspension points or failure policy.

### Phase Dries van Agt

- Added `tina-tokio-bridge`, a narrow Tokio/Tower bridge that sends bounded
  `BridgeRequest` messages into a Betelgeuse-backed Tina runtime and waits for
  explicit typed responses with timeout.
- Added Axum integration proof plus bridge overload tests for worker-ingress
  `Full` and target-mailbox `Full`, so the bridge does not degrade bounded
  pressure into silent timeout.
- Added `TraceRetention::{Full, Bounded, Off}` to `tina-runtime`, wired it into
  `BetelgeuseBackedRuntimeConfig`, and proved bounded live trace retention.
- Renamed the live runner surface to backend-honest
  `BetelgeuseBackedRuntime` / `BetelgeuseBackedMultiShardRuntime`.
- Added a runnable `llama_bridge` example showing Tokio caller code crossing
  into Tina without async handlers or hidden unbounded queues.
- Added bridge production-shape helpers: `BridgeHost`, explicit close/health,
  metrics snapshots, bounded retry/reject policy helpers, caller-timeout late
  response accounting, and a preserved/weakened bridge capability table.
- Added bridge compile-fail guardrails for non-`Send` requests and wrong
  response shapes, plus tests for lifecycle, metrics, cancellation, timeout,
  overload, and clean host shutdown.
- Tightened bridge timeout/cancellation semantics: host-registered services
  skip cancelled queued requests before user state mutates, bridge handles can
  map requests into larger service message enums, and `BridgeHost::try_shutdown`
  can be retried after handles are dropped.
- Added `TINA_DRIVER_RUNTIME_CONTRACT` to name Tina's driver-runtime target:
  completion-shaped I/O, bounded commands, explicit cancellation, owned
  shutdown, explicit progress, deterministic simulation, no hidden executor
  tasks, and no claim of being a general async runtime.
- Rewrote the README into a shorter Tina-as-concurrency-primitive story with
  explicit inspiration links and current non-claims.

### Phase Sputnik

- Added the `tina` trait crate as the shared vocabulary layer.
- Added `Isolate`, `Effect`, `Mailbox`, `Shard`, `Context`, `Address`,
  `Outbound`, and `ChildDefinition`.
- Chose a closed `Effect` enum with per-isolate payload types.
- Added docs, compile-fail tests, and downstream-style integration tests for
  the trait surface.

### Phase Pioneer

- Added shared supervision policy types in `tina`, including restart policy,
  restart-budget accounting, and child restart classification.
- Added `tina-mailbox-spsc`, a bounded single-producer/single-consumer mailbox
  implementation.
- Proved mailbox FIFO order, boundedness, explicit `Full` and `Closed` errors,
  and no hidden overflow queue with black-box tests.
- Added Loom coverage for producer/consumer interleavings, close/send races,
  close/recv behavior, wraparound, and slot reuse.
- Added drop-accounting and allocation-accounting tests to keep mailbox claims
  narrow and evidence-backed.
- Documented the DST boundary and the runtime-enforced SPSC contract.

### Phase Mariner

- Added `tina-runtime`, a small in-progress runtime with a deterministic
  event trace and causal links.
- Added single-shard stepping and local same-shard `Send` dispatch in
  registration order.
- Added local same-shard `Spawn` dispatch with runtime-owned mailbox creation,
  deterministic child IDs, and later-round child execution.
- Added runtime-owned direct parent-child lineage for root registrations and
  spawned children, with crate-private proof support for restart-oriented
  follow-up slices.
- Added a typed runtime ingress API so external code can send to registered and
  spawned isolates without holding raw mailboxes.
- Added stop-and-abandon semantics: when an isolate stops, buffered messages are
  drained in FIFO order, dropped, and traced as `MessageAbandoned`.
- Added panic-capture semantics: an unwinding handler panic becomes
  `HandlerPanicked`, then `IsolateStopped`, and the runtime continues the rest
  of the round deterministically.
- Added runtime tests for trace-core behavior, local send dispatch, and
  stop-and-abandon determinism.
- Added runtime tests for panic capture, post-panic abandonment, preserved
  programmer-error panics, and same-round continuation after panic.
- Added runtime tests for spawn dispatch, typed ingress backpressure, cross-shard
  ingress panics, and zero-capacity spawn rejection.
- Added runtime unit tests for direct parent-child lineage, nested spawn edges,
  and lineage survival across stop/panic.
- Added address-liveness semantics: `Address<M>` now includes a generation,
  runtime send traces include target generation, and stale known generations
  fail visibly as `Closed` instead of targeting a current incarnation.
- Added restartable child records: `RestartableChildDefinition<I>` records a
  factory-backed restart recipe, and `Runtime` stores private child
  metadata for future `RestartChildren` execution.
- Added `RestartChildren` execution for direct child records: restartable
  children are replaced with fresh isolate incarnations, non-restartable
  children are skipped visibly, and restart traces now support deterministic
  causal tree branching.
- Added `tina-supervisor` with `SupervisorConfig`.
- Added supervised panic restart in `tina-runtime`: configured parents
  apply `RestartPolicy` and runtime-lifetime `RestartBudget` state when direct
  children panic.
- Added generated-history runtime property tests for deterministic traces,
  causal-link validity, visible send outcomes, and no accidental handling after
  stop.
- Added an assertion-backed task-dispatcher proof package for the single-shard
  runtime, covering `OneForOne`, `OneForAll`, `RestForOne`, budget exhaustion,
  stale-address closure, and repeated-run determinism.
- Added a runnable `task_dispatcher` example that mirrors the tested workload:
  dispatcher-owned task ingress, registry-isolate address resolution, worker
  panic/restart, and later work continuing through replacement workers.
- Extended `runtime_properties.rs` with generated dispatcher workloads and a
  replay-style proof that reconstructs worker completions, panics, stops, and
  replacements from the runtime trace alone.
- Added focused Miri coverage for the SPSC mailbox unsafe slot paths and a
  `make miri` target.
- Added a runtime-owned call effect family at the `tina` boundary:
  `Isolate::Call` associated type and `Effect::Call(I::Call)` variant.
  Trait surface stays substrate-neutral; concrete request/result
  vocabulary lives in runtime crates.
- Added runtime-owned child bootstrap on `ChildDefinition` and
  `RestartableChildDefinition` via `with_initial_message`. The runtime delivers the
  bootstrap message to the new child immediately after spawn (and after
  each restart, for restartable specs), so a parent can hand a child its
  initial kick without test-harness trace introspection.
- Added `tina-runtime`'s first TCP call family on Betelgeuse
  (nightly Rust): `RuntimeCall<M>` carrying a translator from `CallOutput`
  back to `I::Message`, plus `CallInput` covering TCP listener bind,
  accept, stream read, stream write, listener close, and stream close.
  Resources are runtime-assigned opaque ids; raw sockets never escape
  into isolate state.
- Added a Betelgeuse-backed I/O backend in `tina-runtime`:
  caller-owned typed completion slots, synchronous Betelgeuse ops
  (bind / close) finish during dispatch, async ops (accept / recv / send)
  stay in a pending list until their slot has a result, all driven from
  `Runtime::step()` synchronously.
- Pinned tina-rs to nightly Rust via `rust-toolchain.toml` so the Betelgeuse
  substrate's `allocator_api` feature is available; the gate is scoped to
  `tina-runtime` via a crate-level `#![feature(allocator_api)]`.
- Added new runtime trace event kinds for call dispatch attempt, call
  completion, call failure, and rejected-on-stop completion delivery.
- Added focused tests for the call effect path covering invalid resource
  ids and call-id monotonicity, plus a "no call effect" compile-only smoke
  test that shows existing isolates remain ergonomic with
  `type Call = Infallible`.
- Added an assertion-backed live `tcp_echo` integration test: listener
  isolate supervises a restartable connection-handler child spawned via
  `RestartableChildDefinition::with_initial_message`; bytes round-trip end-to-end on
  `127.0.0.1:0` with the runtime reporting the actual bound address; trace evidence is asserted per
  call kind. Separate unit tests prove the connection isolate's
  partial-write retry logic and the `CallCompletionRejected{RequesterClosed}`
  path for a pending `TcpAccept`, plus accepted-stream `peer_addr` reporting.
- Added a runnable `tcp_echo` example mirroring the tested workload with
  inline assertions on echoed payloads.
- Added ordered `Effect::Batch(Vec<Effect<I>>)` at the `tina` boundary and
  runtime support in `tina-runtime` for deterministic left-to-right
  execution with `Stop` short-circuiting later batched effects.
- Added direct batch-semantics tests in `tina-runtime` proving
  left-to-right execution, spawn-plus-send sequencing, and `Stop`
  short-circuit behavior.
- Expanded the live `tcp_echo` proof and runnable example from a one-client
  demo into a small server-shaped workload: listener self-address capture,
  re-armed `TcpAccept`, sequential multi-client handling, bounded overlap,
  graceful listener close/stop, and retained one-client smoke coverage.
- Added a crate-local runtime proof that two accepted stream reads can be
  pending in `IoBackend` at the same time, so the bounded-overlap TCP claim
  is backed by direct runtime evidence rather than only by client-thread
  interleaving.
- Added the first runtime-owned time call verb: `CallInput::Sleep { after }`
  with `CallOutput::TimerFired`, plus `CallKind::Sleep` in the trace vocabulary.
  The runtime samples a monotonic clock once per `step()` and harvests due
  timers against that sampled instant. Equal-deadline timers wake in
  deterministic request order.
- Added a crate-private `ManualClock` seam so timer tests can drive time
  deterministically without brittle wall-clock sleeps, while production
  `Runtime` still uses a real monotonic clock.
- Added focused timer semantics unit tests: single timer wake, no early fire,
  fires exactly once, different-deadline ordering, equal-deadline request-order
  tie-break, and late-completion rejection after requester stop.
- Added a retry/backoff proof workload test: first attempt fails, a
  runtime-owned timer delays a real second attempt, later retry succeeds,
  and the trace proves the backoff `Sleep` completion occurred before the
  retried attempt.
- Added a public-path integration test for the same retry/backoff shape, using
  the shipped monotonic clock rather than the crate-private manual clock seam.

### Phase Voyager

- Added `tina-sim`, the first Voyager simulator crate.
- Added a single-shard virtual-time execution model with deterministic
  event recording against the shipped `tina-runtime` event
  vocabulary.
- Added simulator support for the shipped timer call family:
  `CallInput::Sleep { after }` and `CallOutput::TimerFired`.
- Added replay artifacts containing simulator config, final virtual time,
  and the reproducible event record for one run.
- Added timer-semantics proofs in `tina-sim` covering no-early-wake,
  one-shot wake, different-deadline ordering, equal-deadline request-order
  tie-break, stopped-requester completion rejection, and repeated
  same-config event-record reproduction.
- Added a simulator-backed retry/backoff proof workload and a replay test
  proving that rerunning from the saved config reproduces the same event
  record exactly.
- Made `SimulatorConfig.seed` semantically real for the first narrow seeded
  perturbation surface in `tina-sim`.
- Added `FaultConfig` / `FaultMode` for seeded perturbation over:
  - local-send delivery
  - timer-wake delivery
- Added a small checker surface in `tina-sim`:
  - `Checker`
  - `CheckerDecision`
  - `CheckerFailure`
- Extended replay artifacts to preserve optional checker failure information
  alongside config, final virtual time, and event record.
- Added a deliberate-bug public-path simulator workload proving that a seeded
  local-send perturbation can trip a checker, halt the run, and be reproduced
  exactly from the saved replay artifact config.
- Added a small structural checker proof over simulator event-id monotonicity.
- Fixed two simulator semantic bugs uncovered by the new proof surface:
  - delayed local sends now miss one additional delivery round instead of
    behaving identically to ordinary handler-emitted sends
  - `run_until_quiescent()` now continues while future-visible delayed local
    sends remain pending, instead of stopping early
- Tightened the timer-fault retry proof so its different-seed divergence claim
  is stated honestly: the timer-wake perturbation changes replay-visible
  virtual-time outcome, while the local-send perturbation changes the event
  record and checker outcome.
- Extended `tina-sim` with the shipped single-shard spawn and supervision
  surface:
  - `SpawnSpec`
  - `RestartableSpawnSpec`
  - direct parent-child lineage
  - restartable child records
  - direct-child `RestartChildren`
  - supervised panic restart through `SupervisorConfig`
- Added simulator proofs for spawn/restart parity: later-step child execution,
  same-step spawn ordering, bootstrap re-delivery after restart, repeated
  restart replay, all shipped restart policies, non-restartable skip events,
  stale-address send rejection as `Closed`, budget exhaustion, direct-child
  restart scope, and additive compatibility with existing `Spawn = Infallible`
  timer/fault/checker workloads.
- Extended `tina-sim` with scripted single-shard TCP simulation for the
  shipped call family:
  - `TcpBind`
  - `TcpAccept`
  - `TcpRead`
  - `TcpWrite`
  - `TcpListenerClose`
  - `TcpStreamClose`
- Added explicit simulator config for bounded scripted listeners, peers, and
  pending TCP completion capacity, plus `TcpCompletionFaultMode` for seeded
  delayed-completion and ready-batch reordering perturbation.
- Extended replay artifacts with captured peer-visible TCP output.

### Phase Galileo

- Added additive multi-shard coordinator shells:
  - `tina_runtime::MultiShardRuntime`
  - `tina_sim::MultiShardSimulator`
- Added root supervision routing on multi-shard runtime/simulator shells:
  `supervise(parent, config)` routes to the shard that owns the parent while
  child ownership remains shard-local.
- Added global explicit-step coordination in ascending shard-id order with:
  - global `try_send(addr, msg)` routed by `addr.shard()`
  - explicit root placement by shard
  - destination harvest before each destination shard's handler snapshot
  - next-step-only cross-shard visibility
- Added shared global event-id and call-id allocation across sibling shards.
- Added bounded shard-pair cross-shard transport with deterministic source-side
  `Full` rejection and no hidden overflow queue.
- Added deterministic cross-shard harvest rules:
  - ascending source-shard order per destination
  - FIFO within one shard-pair queue
  - drain-one-channel-to-empty before moving to the next source
- Added explicit source-time vs destination-time semantics for cross-shard
  delivery:
  - source-side `SendAccepted` / `SendRejected` describe transport admission
  - destination harvest records `MailboxAccepted` or destination-local
    `SendRejected` as an observability extension
- Added direct runtime and simulator proofs for:
  - global ingress routing
  - next-step-only remote visibility
  - shard-pair queue overflow
  - stopped/closed remote target rejection
  - unknown remote isolate rejection
  - destination mailbox full on harvest
  - FIFO from one source
  - deterministic multi-source harvest order
- Added a user-shaped two-shard dispatcher/worker workload on the preferred 021
  surface:
  - cross-shard request from coordinator to worker
  - cross-shard reply from worker back to coordinator
  - visible user-path `SendRejectedReason::Full`
  - deterministic repeated-run proof in the live runtime
- Added multi-shard simulator replay support:
  - `MultiShardSimulator::run_until_quiescent()`
  - `MultiShardSimulator::replay_artifact()`
  - `MultiShardReplayArtifact`
  - replay-style proof that rerunning from the saved configs reproduces the
    same multi-shard event record and workload output
- Added direct proof for per-isolate-pair FIFO across one shard pair with
  multiple source isolates and multiple target isolates, in both runtime and
  simulator tests.
- Added direct proof that multi-shard simulator replay works under non-default
  seeded timer/local-send fault config.
- Added direct proof that different non-default seeds can diverge in a
  faulted multi-shard simulator workload.
- Added direct proof that multi-shard scripted TCP echo composes with seeded
  TCP completion faults.
- Added direct proof that multi-shard supervision/restart composes with seeded
  local-send delay.
- Documented the current Galileo boundary honestly: full upstream-style
  peer-quarantine / shard-restarted semantics remain later work, not silently
  bundled into this first multi-shard slice.
- Added simulator proofs for TCP parity and replay: one-client echo,
  bounded-overlap echo, partial read/write drain behavior, invalid-resource
  failures, listener-close cancellation, stopped-requester rejection,
  mailbox-full completion rejection, same-config peer-output replay, both
  TCP fault-surface divergence modes, and checker-backed replay of seeded TCP
  accept reordering.
- Fixed two simulator driver/scheduler bugs uncovered by the TCP proof
  surface:
  - `run_until_quiescent()` and checked replay runs now continue while pending
    TCP calls remain in flight, instead of stopping early when no timers or
    visible messages exist yet
  - seeded TCP delay perturbation now preserves per-resource FIFO by never
    allowing later completions on the same listener/stream to overtake earlier
    ones

### Phase 021 Devex and Call Ergonomics

- Renamed the user-facing runtime crate directory and package surface from the
  transitional `tina-runtime-current` shape to `tina-runtime`.
- Reworked the preferred authoring vocabulary around `Runtime`,
  `RuntimeCall`, `CallInput`, `CallOutput`, `CallError`, `Outbound`,
  `ChildDefinition`, and `RestartableChildDefinition`.
- Added the preferred prelude and isolate authoring macros so common isolates
  do not need the old wall of associated-type boilerplate.
- Added typed runtime-call helpers such as `sleep(...)`, `tcp_read(...)`, and
  `tcp_write(...)` with `reply(...)` as the single public completion combinator.
- Removed the old compatibility-alias plan before public use; Tina kept one
  preferred surface instead of silent equal-peer names.
- Reworked README and tests toward the new syntax and proved the renamed
  surface through runtime and simulator consumer tests.

### Phase Kepler

- Sealed the current explicit-step multi-shard liveness boundary:
  address-local remote failures remain address-local, and there is still no
  shard-down / peer-down / restarted-peer event vocabulary.
- Sealed multi-shard supervision as shard-local: root supervision routes to the
  parent shard, spawned children stay on the parent shard, and supervised
  restarts stay on that shard.
- Added runtime proofs for the sealed rules:
  - `cross_shard_unknown_isolate_does_not_poison_destination_shard`
  - `dispatcher_worker_workload_continues_after_bad_remote_address_on_same_shard`
  - `multishard_supervision_keeps_children_on_parent_shard`
- Added simulator proofs for the same sealed rules:
  - `cross_shard_simulation_unknown_isolate_does_not_poison_destination_shard`
  - `multishard_dispatcher_workload_continues_after_bad_remote_address_on_same_shard`
  - `multishard_simulation_supervision_keeps_children_on_parent_shard`
- Added multi-shard checker support:
  - `MultiShardSimulator::run_until_quiescent_checked()`
  - `MultiShardReplayArtifact::checker_failure()`
- Added checker/replay proofs for the liveness boundary:
  - `multishard_checker_accepts_address_local_remote_failure_then_good_traffic`
  - `multishard_checker_failure_replays_for_address_local_liveness_bug`
- Added a focused allocation probe,
  `multishard_runtime_path_still_has_allocations_so_the_claim_stays_narrow`,
  and narrowed the runtime allocation claim instead of pretending the whole
  multi-shard runtime path is allocation-free.

### Phase Huygens

- Added the first live shard-owned runtime substrate in `tina-runtime`:
  - `ThreadedRuntime<S, F>` for one worker-owned shard runtime
  - `ThreadedMultiShardRuntime<S, F>` for a fixed worker-per-shard runtime set
  - `ThreadedRuntimeConfig` for bounded command ingress and idle wait tuning
  - `ThreadedTrySendError`, `ThreadedSendObservedError`, and
    `ThreadedControlError`
- Defined `ThreadedRuntime::try_send` as bounded handoff only, so it does not
  block after admission waiting for the worker to observe mailbox state.
- Added `ThreadedRuntime::send_and_observe` as the explicit synchronous control
  path for tests/setup that need mailbox `Full` / `Closed` outcomes.
- Added live-threaded TCP echo proof:
  `threaded_runtime_tcp_echo_round_trips_reference_workload`.
- Added live bounded-ingress proof:
  `threaded_runtime_try_send_surfaces_ingress_full_without_blocking_on_worker`.
- Added live single-shard substrate proofs for stopped-target observation,
  runtime-owned timer retry, and local mailbox `Full` trace visibility.
- Added live fixed-shard cross-shard substrate proofs for:
  - request/reply across two OS worker threads
  - remote destination worker queue `Full` observed at the source
  - stale remote address rejection without poisoning later good remote work
- Added sendable erasure for live cross-shard payload transport while keeping
  the explicit-step runtime one-thread-owned.
- Changed shared runtime/simulator event and call id allocation to use a
  cloneable atomic id source so sibling worker runtimes can preserve global
  monotonic ids.
- Added composed Huygens DST harness tests covering supervision + timer +
  local-send perturbation, replayable checker failure, and remote `Full`
  pressure on the explicit-step oracle.
- Documented the Huygens claim boundary: live substrate exists for selected
  workloads, while production hardening, peer quarantine, dynamic shard
  membership, cross-shard child ownership, Tokio bridge work, and broad
  allocation-free runtime claims remain future work.

### Phase Mercury

- Added user-visible observed send outcomes so application code can branch on
  `Accepted`, `Full`, and `Closed` instead of only inspecting trace after the
  fact.
- Added same-shard isolate-to-isolate call with mandatory timeout and typed
  outcomes for reply, target full, target closed, timeout, and requester-stop
  completion rejection.
- Added focused runtime and simulator tests for reply delivery, full/closed
  targets, timeout, late replies after timeout, requester stopped, requester
  mailbox full at completion, and replay determinism.
- Added macro/devex cleanup and a runnable Tokio-vs-Tina semantic comparison
  suite to pressure ergonomics without making Tokio the substrate story.
- Recorded cross-shard call reply transport as not yet claimed; cross-shard
  call rejects deterministically in this slice.

### Phase Betelgeuse

- Added the first live substrate surface as `BetelgeuseRuntime` /
  `BetelgeuseMultiShardRuntime`.
- Kept explicit-step runtime and `tina-sim` as the semantic oracle while
  proving selected workloads on live Betelgeuse-backed runners.
- Added bounded ingress proof, live time/TCP completion semantics, live
  multi-shard bounded send, typed live cross-shard call rejection, and
  oracle/sim/live parity tests.
- Added a narrow Betelgeuse simulated TCP backend with seeded completion delay
  and partial-write pressure.
- Pinned allocation and cost probes for the touched substrate paths and kept
  Tokio as comparison/later bridge rather than the main runtime story.

### Phase Tina TCP Driver Contract

- Moved runtime-owned time/TCP behind a small Tina-owned driver boundary for
  timers, TCP operations, completions, cancellation, shutdown, and wakeups.
- Added native Betelgeuse and simulated Betelgeuse driver adapters under the
  same runtime semantics.
- Added same-resource `ResourceBusy` semantics, bounded pending-operation
  admission, and direct cancellation/late-completion proofs.
- Proved user-shaped workloads on explicit runtime, native Betelgeuse-backed
  runtime, and simulated-driver runtime without adding futures, wakers, async
  handlers, or arbitrary task spawning.

### Phase Parallel Substrate Support

- Polished the simulated Betelgeuse I/O surface as generic substrate support
  rather than Tina-specific magic.
- Added narrow allocation/performance probes for current hot paths.
- Expanded runnable Tokio-vs-Tina comparisons around constrained capacity,
  backpressure, timeout, shutdown, and overload behavior.
- Added only small helper/macro polish that preserved one preferred Tina
  surface.
- Recorded external review prompts and substrate research notes for Tokio
  current-thread, Monoio, Glommio, and Compio.

### Phase Ranger

- Documented the driver capability contract for time/TCP progress,
  cancellation, shutdown, bounded pending work, and deterministic simulator
  compatibility.
- Moved TCP pending ownership to listener/read/write lanes and allowed
  full-duplex same-stream read/write while keeping close and duplicate-lane
  `ResourceBusy` honest.
- Made per-call cancel tombstone the selected call without silently closing
  unrelated resource lanes.
- Added live and simulated proofs for stopped-requester cancellation, explicit
  close, runtime shutdown, late completion swallowing, requester mailbox full
  at completion, and live worker TCP shutdown.
- Pinned TCP read/write allocation counts and recorded Betelgeuse as the
  near-term substrate direction.

### Phase Surveyor

- Treated Tina's live substrate as a Tina-owned implementation over Betelgeuse
  instead of waiting for upstream Betelgeuse to provide Tina-specific
  guarantees.
- Hardened completion-slot ownership so shutdown and cancellation no longer
  depend on dropping slots while a backend may still hold pending completion
  state.
- Added no-leak shutdown/cancel-drain proofs across native and simulated
  Betelgeuse-backed runtime paths.
- Preserved the explicit-step runtime and `tina-sim` as the semantic oracle;
  Surveyor changed live-substrate ownership, not Tina's isolate model.

### Phase Willem Drees

- Added a composed local-production workload in `tina-runtime` with listener
  isolate, connection isolates, bounded worker, supervisor restart,
  runtime-owned TCP, runtime-owned time, and shutdown pressure.
- Proved the workload on live Betelgeuse TCP with real loopback clients,
  observing typed `CallOutcome::{Replied, Full, Timeout}` and trace events.
- Proved the same server shape through Betelgeuse simulated I/O with delayed
  completions and partial writes, driven through the threaded runtime loop.
- Added a `tina-sim` oracle version that replays the bounded TCP flow
  byte-for-byte for observations, peer output, and event kinds.
- Added composed shutdown proof for pending accept, read, write, sleep, and
  isolate-call work.
- Added server-shaped backpressure guards: explicit mailbox/ingress
  capacities, forced worker `Full`, and exact-sized scripted peer output
  buffers so hidden writes cannot be masked.

### Phase Ruud Lubbers

- Added a narrow numerical runtime cost model with allocation probes for
  multi-shard send, isolate call, timer, TCP read/write, two-send batch,
  spawn, restart, repeated trace pressure, live ingress handoff, and
  high-cardinality idle stepping.
- Kept the SPSC mailbox no-allocation proof intact while improving runtime,
  simulator, driver, and coordinator allocation behavior.
- Reused runtime and simulator round-message scratch storage, driver
  completion scratch storage, and preallocated common runtime/simulator
  bookkeeping vectors.
- Changed runtime-created mailboxes to store erased message boxes directly,
  avoiding an extra box/downcast/box cycle for runtime-created mailboxes while
  preserving user-provided typed mailbox registration.
- Reworked multi-shard runtime and simulator coordinator queues into prebuilt
  indexed double buffers, reducing the multi-shard send hot path to
  `1 alloc / 0 realloc` while preserving next-global-step remote visibility.
- Added regression proof for more-than-initial-capacity round-message scratch
  reuse, including a 12-isolate idle-step allocation test.
- Recorded medium follow-up rocks in the roadmap: batch small path, live worker
  command boxing, sizing knobs, trace retention policy, typed fast paths, and
  completion-slot pooling/slabbing.

### Phase Joop den Uyl

- Migrated the composed local-production workload into canonical
  `application_surface` test artifacts for `tina-runtime` and `tina-sim`.
- Added a named local service-capacity pattern in the canonical harness so
  listener, connection, worker, command, backlog, and pending-completion
  capacities are explicit instead of scattered magic numbers.
- Added test-local trace assertion helpers for event existence/counts,
  stopped-and-idle service checks, and terminal send/call outcome invariants.
- Added direct application-surface proofs across live Betelgeuse loopback,
  threaded simulated I/O, explicit-step runtime with simulated I/O, and
  deterministic `tina-sim` replay.
- Added non-TCP porting proofs for bounded worker/router pressure and a
  stateful session/control-plane shape with local audit send.
- Kept helper surface test-local for now; no public application builder,
  router, registry, or macro was added.
