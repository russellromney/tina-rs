# 023 Huygens Audit

Session:

- C (implementation)

## Runtime / Simulator Seam Audit

Current seams:

- `Runtime<S, F>` is the semantic runtime core. It owns one shard, local
  mailboxes, runtime-owned timers/TCP calls, supervision state, and event trace.
- `MultiShardRuntime<S, F>` is still explicit-step. It composes shard-local
  runtimes under one coordinator and bounded shard-pair queues.
- `Simulator<S>` and `MultiShardSimulator<S>` mirror the runtime event model and
  add virtual time, replay, checkers, scripted TCP, and seeded perturbation.
- `Runtime` is intentionally not `Send`: it uses `Rc`, `Cell`, `RefCell`, and
  runtime-local erased mailboxes. Moving a built runtime between threads would
  fight the current model.

Decision:

- The smallest honest live substrate is a worker-owned runtime, not a moved
  runtime.
- Add `tina_runtime::ThreadedRuntime<S, F>` and
  `tina_runtime::ThreadedMultiShardRuntime<S, F>` as additive `tina-runtime`
  boundary types.
- `ThreadedRuntime` starts one OS worker thread for one shard runtime. The
  worker constructs and owns `Runtime<S, F>` on that thread.
- `ThreadedMultiShardRuntime` starts one OS worker thread per shard. Each
  worker constructs and owns its shard runtime on that thread.
- The handle communicates through a bounded command queue:
  registration, supervision, typed ingress, trace snapshot, in-flight query,
  and shutdown.
- Live cross-shard sends route `Send + 'static` payloads through bounded worker
  queues. `Runtime` itself remains one-thread-owned.
- The explicit-step runtime and simulator remain the semantic oracle.

First API shape:

- `ThreadedRuntime::new(shard, mailbox_factory)`
- `ThreadedRuntime::with_config(shard, mailbox_factory, ThreadedRuntimeConfig)`
- `register_with_capacity(isolate, mailbox_capacity)`
- `supervise(parent, config)`
- `try_send(address, message)`
- `send_and_observe(address, message)`
- `trace()`
- `has_in_flight_calls()`
- `shutdown()`
- `ThreadedMultiShardRuntime::new(shards, mailbox_factory)`
- `ThreadedMultiShardRuntime::with_config(shards, mailbox_factory, config)`
- `ThreadedMultiShardRuntime::register_with_capacity_on(shard, isolate, mailbox_capacity)`
- `ThreadedMultiShardRuntime::supervise(parent, config)`
- `ThreadedMultiShardRuntime::try_send(address, message)`
- `ThreadedMultiShardRuntime::trace()`
- `ThreadedMultiShardRuntime::trace_on(shard)`
- `ThreadedMultiShardRuntime::shutdown()`

Current claim boundary:

- The live substrate covers one worker-owned shard and fixed worker-per-shard
  sets.
- It proves the real "worker owns shard runtime" shape, bounded ingress, and
  bounded live cross-shard send transport for `Send + 'static` payloads.
- It does not claim peer quarantine, dynamic shard membership, or cross-shard
  child ownership. The explicit-step runtime and simulator remain the semantic
  oracle for full fault/replay exploration.
- `ThreadedRuntime::try_send` is bounded handoff only. Success means the
  worker accepted ownership of the command, not that the target mailbox has
  accepted the message.
- `ThreadedRuntime::send_and_observe` is the synchronous control/test path that
  waits for the worker to observe target mailbox `Full` / `Closed`.
- `ThreadedMultiShardRuntime::try_send` has the same bounded-handoff meaning
  for the worker that owns the address.

Evidence added:

- `threaded_runtime_tcp_echo_round_trips_reference_workload`
  - runs the TCP echo / request-response reference workload on a real worker
    thread
  - asserts echoed bytes and trace-level TCP bind/accept/read/write/close
    completions
  - compares final bytes and a shared TCP trace-count subset against the
    explicit-step runtime oracle
- `threaded_runtime_surfaces_closed_mailbox_after_stop`
  - proves live substrate ingress observes closed target after stop
- `threaded_runtime_try_send_surfaces_ingress_full_without_blocking_on_worker`
  - parks the worker in a handler, fills the bounded command queue, and proves
    `try_send` returns `IngressFull` without waiting for the worker to drain
    the first accepted command
- `threaded_runtime_timer_retry_runs_without_manual_stepping`
  - proves runtime-owned `Sleep` wakes and retries on the live worker without
    manual stepping
- `threaded_runtime_local_mailbox_full_is_visible_in_trace`
  - proves live local-send pressure surfaces as `SendRejected { Full }` in the
    runtime trace
- `threaded_multishard_dispatcher_round_trips_between_worker_threads`
  - runs a user-shaped coordinator/worker request-reply across two OS worker
    threads
  - proves shard 1 accepts a send to shard 2 and shard 2 accepts the reply back
    to shard 1
- `threaded_multishard_remote_queue_full_is_visible_at_source`
  - parks the destination worker, fills its bounded command queue without
    sleeps-as-proof, and proves the source records cross-shard
    `SendRejected { Full }`
- `threaded_multishard_bad_remote_does_not_poison_good_remote_work`
  - sends first to a stale remote address and then to a valid remote worker
  - proves the stale remote rejection does not poison later good cross-shard
    work
- `dst_harness_replays_supervision_timer_and_local_send_composition`
  - replays a composed simulator workload using supervision restart, timer
    wake, local send perturbation, and app observations
- `dst_harness_records_replayable_checker_failure_on_composed_restart`
  - captures a checker failure against a composed restart event and proves
    replay
- `dst_harness_keeps_remote_full_pressure_visible_on_oracle`
  - keeps cross-shard `Full` pressure visible through the explicit-step
    multi-shard simulator
