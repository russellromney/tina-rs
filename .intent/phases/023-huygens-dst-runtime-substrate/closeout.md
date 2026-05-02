# 023 Huygens Closeout

Session:

- D (closeout)

## Verdict

Huygens closes on its intended claim:

> tina-rs is a shared-nothing, shard-owned concurrency framework for Rust. Its
> core primitives are hammered by deterministic simulation/replay pressure, and
> the same isolate model can run on a real shard-owned runtime path for selected
> workloads today.

This is not a release claim and not a broad Tokio replacement claim. It is the
first honest framework claim: synchronous isolates, bounded queues, traceable
effects, simulator proof, explicit-step oracle, and a live worker-owned runtime
substrate all exist together.

## Workload Matrix

| Workload | Simulator/replay | Explicit-step oracle | Runtime substrate | Checker |
|---|---|---|---|---|
| TCP echo / request-response with partial I/O | `downstream_consumer_can_run_scripted_tcp_echo_end_to_end`, `scripted_tcp_echo_handles_overlap_and_partial_io`, `scripted_tcp_echo_handles_tangled_overlap_with_single_byte_drain`, `tcp_replay_preserves_peer_output` | `tcp_echo_round_trips_one_client_payload`, `tcp_echo_handles_two_overlapping_clients_and_rearms_listener`, `connection_retries_partial_write_before_reading_again` | `threaded_runtime_tcp_echo_round_trips_reference_workload` | `tcp_checker_failure_replays_under_ready_reordering` |
| Dispatcher / worker cross-shard request-reply and bad-address survival | `multishard_dispatcher_workload_preserves_request_reply_causality`, `multishard_dispatcher_workload_continues_after_bad_remote_address_on_same_shard`, `multishard_dispatcher_workload_replays_from_saved_config` | `dispatcher_worker_workload_preserves_cross_shard_request_reply_causality`, `dispatcher_worker_workload_continues_after_bad_remote_address_on_same_shard` | `threaded_multishard_dispatcher_round_trips_between_worker_threads`, `threaded_multishard_bad_remote_does_not_poison_good_remote_work` | `multishard_checker_accepts_address_local_remote_failure_then_good_traffic`, `multishard_checker_failure_replays_for_address_local_liveness_bug` |
| Retry/backoff and runtime-owned time | `retry_backoff_retries_after_virtual_timer_wake`, `same_seed_faulted_timer_run_reproduces_same_record`, `different_seeds_can_diverge_in_virtual_time_on_timer_wake_faults` | `retry_backoff_public_path_retries_after_timer_wake`, `retry_backoff_workload_uses_timer_path` | `threaded_runtime_timer_retry_runs_without_manual_stepping` | covered by replay/fault divergence on the simulator path |
| Supervision/restart under perturbation | `restart_workload_composes_with_seeded_local_send_delay`, `restart_sensitive_checker_failure_is_replayable`, `multishard_supervision_workload_composes_with_seeded_local_send_delay` | `task_dispatcher` supervision tests and `supervision::*` runtime tests | supervision API is available on `ThreadedRuntime` / `ThreadedMultiShardRuntime`; no separate live restart workload was required after the TCP and cross-shard live proofs | `dst_harness_records_replayable_checker_failure_on_composed_restart` |
| Bounded backpressure | `dst_harness_keeps_remote_full_pressure_visible_on_oracle`, multi-shard simulator `Full` tests | `dispatcher_worker_workload_surfaces_source_time_full_rejection`, runtime shard-pair `Full` tests | `threaded_runtime_try_send_surfaces_ingress_full_without_blocking_on_worker`, `threaded_runtime_local_mailbox_full_is_visible_in_trace`, `threaded_multishard_remote_queue_full_is_visible_at_source` | structural send-outcome property tests and composed checker replay |

## Runtime Substrate Claim

What exists:

- `ThreadedRuntime<S, F>` starts one OS worker thread for one shard runtime.
- `ThreadedMultiShardRuntime<S, F>` starts one OS worker thread per shard in a
  fixed shard set.
- Each worker constructs and owns its `Runtime<S, F>` on that worker thread.
- Handles communicate through bounded command queues.
- `try_send` is bounded handoff only. It does not wait for mailbox acceptance.
- `send_and_observe` is the synchronous control/test path for observing mailbox
  `Full` / `Closed`.
- Live cross-shard sends move `Send + 'static` payloads through bounded worker
  queues.
- The explicit-step runtime and simulator remain the semantic oracle.

What does not exist yet:

- broad production hardening
- dynamic shard membership
- peer quarantine / shard-down broadcast semantics
- cross-shard child ownership
- a Tokio bridge or Axum adapter
- broad allocation-free runtime claims
- a promise that every Tokio-shaped workload can move today

## Evidence Notes

The important Huygens proof is not one test. It is the stack:

- simulator/replay can compose time, TCP, supervision, faults, checkers, and
  multi-shard routing
- explicit-step runtime remains the precise semantic oracle
- live substrate runs user-shaped workloads without manual stepping
- bounded ingress and bounded cross-shard pressure are directly visible
- failures stay traceable rather than becoming logs or hidden task behavior

The live runtime tests use timeout guards only as failure guards. Success paths
exit through app-observed output, trace state, explicit shutdown, or bounded
queue observations.

## Verification

`make verify` passes on May 1, 2026.
