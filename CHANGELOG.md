# Changelog

This file records completed work.

## Unreleased

### Phase Sputnik

- Added the `tina` trait crate as the shared vocabulary layer.
- Added `Isolate`, `Effect`, `Mailbox`, `Shard`, `Context`, `Address`,
  `SendMessage`, and `SpawnSpec`.
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

- Added `tina-runtime-current`, a small in-progress runtime with a deterministic
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
- Added restartable child records: `RestartableSpawnSpec<I>` records a
  factory-backed restart recipe, and `CurrentRuntime` stores private child
  metadata for future `RestartChildren` execution.
- Added `RestartChildren` execution for direct child records: restartable
  children are replaced with fresh isolate incarnations, non-restartable
  children are skipped visibly, and restart traces now support deterministic
  causal tree branching.
- Added `tina-supervisor` with `SupervisorConfig`.
- Added supervised panic restart in `tina-runtime-current`: configured parents
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
