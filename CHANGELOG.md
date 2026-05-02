# Changelog

This file records completed work.

## Unreleased

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
  - `ChildDefinition`
  - `RestartableChildDefinition`
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
