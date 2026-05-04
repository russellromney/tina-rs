# 026 Review

## Implementation Review 1

Verdict: first driver-contract slice is on-shape.

What landed:

- `tina-runtime` now has a Tina-owned `RuntimeDriver` boundary in
  `tina-runtime/src/driver.rs`.
- `Runtime` owns `Box<dyn RuntimeDriver>` and no longer owns timer state or a
  concrete Betelgeuse I/O module directly.
- `BetelgeuseDriver` owns both timer completions and Betelgeuse TCP state, so
  sleep and TCP calls use the same driver path.
- The old `io_backend.rs` file was renamed to `driver.rs`; old backend naming
  was removed from runtime control flow.
- Bounded ingress, mailboxes, cross-shard queues, supervision, trace events,
  and call outcomes remain in Tina runtime code, outside the driver.

Direct proof added:

- `runtime_timer_path_can_use_non_betelgeuse_driver` proves a runtime-owned
  sleep call can submit to and complete through a fake non-Betelgeuse driver.
- `runtime_shutdown_cancels_non_betelgeuse_driver_pending_call` proves runtime
  shutdown calls the driver cancellation hook and emits requester-facing
  `CallCompletionRejected { RequesterClosed }`.
- `driver_timer_hot_path_allocation_count_is_pinned_after_warmup` pins the
  warmed timer-through-driver path at 10 allocations and 1 reallocation in the
  existing debug-profile allocation probe.

Regression proof run:

- `cargo +nightly test -p tina-runtime --lib`
- `cargo +nightly test -p tina-runtime --test tcp_echo`
- `cargo +nightly test -p tina-runtime --test betelgeuse_substrate`
- `cargo +nightly test -p tina-runtime --test multishard_allocation`
- `make verify`

Simulated-driver decision:

- No extra fake simulated protocol layer was added. Existing explicit-runtime
  and threaded-runtime simulated Betelgeuse TCP tests now pass through
  `BetelgeuseDriver::with_io_loop(...)`, so they already prove simulated TCP
  uses the same Tina driver path after this refactor.

Remaining 026 work:

- Continue with any additional driver-contract polish found during review.

## Implementation Review 2

Verdict: stopped-requester cancellation gap closed.

What changed:

- `RuntimeDriver` now has per-call `cancel(call_id)` in addition to whole-driver
  shutdown cancellation.
- `Runtime::stop_entry_with_precollected(...)` cancels all in-flight driver
  calls owned by the stopped requester and emits
  `CallCompletionRejected { RequesterClosed }` immediately.
- Betelgeuse TCP cancellation closes the listener/stream for the pending op and
  keeps the completion slot as a tombstone until Betelgeuse reports readiness.
  This avoids dropping a completion box while the substrate may still hold its
  pointer.
- Canceled TCP tombstones do not keep `Runtime::has_in_flight_calls()` true and
  do not deliver later user-visible completions.

Direct proof added:

- `pending_accept_completion_is_rejected_when_requester_stops_first` now proves
  pending accept cancellation without any peer connection completing the accept.
- `pending_read_is_cancelled_when_requester_stops_without_peer_input` proves a
  silent peer cannot keep the runtime in-flight after the reader stops.
- `pending_write_is_cancelled_when_requester_stops_before_simulated_completion`
  proves delayed simulated writes cancel before completion and do not reach the
  peer.

Regression proof run:

- `cargo +nightly test -p tina-runtime --test call_dispatch -- --nocapture`
- `make verify`

## Implementation Review 3

Verdict: per-call cancellation is now honest for 026.

What changed:

- `CallError::ResourceBusy` was added for the live TCP driver path.
- `BetelgeuseTcp` now enforces one active pending operation per TCP resource
  (`ListenerId` or `StreamId`). A second pending accept/read/write on the same
  resource fails immediately with `CallOutput::Failed(CallError::ResourceBusy)`.
- Listener/stream close also respects the same rule. Closing a resource with an
  active pending operation fails with `ResourceBusy` instead of becoming hidden
  cancellation from a second isolate.
- `tina-sim` now uses the same one-active-pending-operation rule, so the oracle
  and live driver no longer teach different TCP resource semantics.
- Per-call cancel may still close the underlying listener/stream, but that is
  now safe inside 026's contract because no unrelated live pending operation can
  exist on that same resource.
- Canceled TCP operations stay as tombstones until Betelgeuse produces the late
  completion, then the tombstone is swallowed without user-visible completion.

Direct proof added:

- `second_pending_operation_on_same_stream_fails_resource_busy` proves a live
  pending read prevents a second write on the same stream, leaves the original
  read in flight, and records `CallFailed { TcpWrite, ResourceBusy }`.
- `stream_close_while_read_pending_fails_resource_busy` proves an explicit live
  close cannot invalidate another isolate's pending read.
- `pending_write_is_cancelled_when_requester_stops_before_simulated_completion`
  now steps beyond the simulated delayed write completion and reasserts that no
  peer bytes, translated messages, or `TcpWrite` completion appear.
- `listener_close_while_accept_pending_fails_resource_busy`,
  `same_resource_second_read_fails_resource_busy_under_tcp_delay_faults`, and
  the stream-close simulation tests pin the same rule on the oracle side.

Regression proof run:

- `cargo +nightly test -p tina-runtime --test call_dispatch -- --nocapture`
- `cargo +nightly test -p tina-sim --test io_simulation -- --nocapture`
- `cargo +nightly test -p tina-runtime --lib`
- `cargo +nightly test -p tina-runtime --test tcp_echo`
- `cargo +nightly test -p tina-runtime --test multishard_allocation`
- `cargo +nightly test -p tina-sim --test tokio_vs_tina_examples`
- `make verify`

## Closeout Verdict

026 is done.

Tina now has a runtime-owned TCP/time driver contract in `tina-runtime`.
Betelgeuse native TCP, Betelgeuse simulated TCP, and timer calls all pass
through that boundary. Tina still owns isolate scheduling, bounded mailboxes,
trace events, supervision, call outcomes, and cancellation semantics.

The hard cancellation cases are pinned directly:

- stopped requester cancels pending accept/read/write;
- canceled writes are stepped past late substrate completion and swallowed;
- same-resource concurrent TCP operations fail typed `ResourceBusy`;
- explicit close cannot invalidate another isolate's pending operation;
- simulator and live driver use the same resource-busy rule.

Final proof: `make verify` passes.
