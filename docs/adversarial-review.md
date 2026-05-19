# Adversarial Code Review

Scope: ~108K LOC of production source across 17 workspace crates (excludes
examples, vendor-betelgeuse, tests-as-tests). Method: eight parallel
deep-dive review agents on the highest-risk modules plus direct inspection
of the SPSC mailbox and supervisor.

The codebase is unusually disciplined for its size. The dangerous bugs are
concentrated at three boundaries: HTTP keepalive / HTTP/2, cross-shard
relay, and bridge backpressure.

A second narrower review found additional persistence/process/bridge risks.
Those are folded into the "Additional findings" section below so this file can
be the canonical review artifact.

File paths are relative to the repo root. Line numbers reflect the
`worktree-adversarial-review` snapshot at the time of review.

## Phase 123 First-Pass Coverage Status

Phase 123's PR fixes or explicitly proves every first-pass finding in the
coverage map. The later A8-A12 second-pass findings remain preserved in this
file and are assigned to Phase 124 by the phase plan.

| Rock | Findings | Status |
|---|---|---|
| 1 | C1 | Fixed in this PR. |
| 2 | H14, M1, M6, L13, L14, A6, A7 | Fixed in this PR. |
| 3 | C2, C3, C5, H9, M7, M14 | Fixed in this PR. |
| 4 | C4, M13 | Fixed in this PR. |
| 5 | H1, H2, H13, M8, A1 | Fixed in this PR. |
| 6 | H6, L10, L11, A2 | Fixed in this PR. |
| 7 | A3, A4, A5 | Fixed in this PR. |
| 8 | H4, H5, H7, H10, H11, L5 | Fixed in this PR. |
| 9 | H3, M5, M21, L1, L6, L8, L15 | Fixed or proven with tests in this PR. |
| 10 | M2, M3, M16, M23, M24, M25, L2, L3, L4, L16 | Fixed or proven with tests in this PR. |
| 11 | H8, H12, M11, M12, M22, L7, L9, L12 | Fixed or proven with tests in this PR. |
| 12 | M9, M10, M15, M18, M19, M20, L17, L18 | Fixed or proven with tests in this PR. |
| 13 | M4, M17 | Fixed in this PR. |

## Phase 123 Fixed Finding Details

The original findings below are preserved for history. Fixed items record
the implementation proof and regression test names.

| Finding | Phase 123 status |
|---|---|
| C1 | Fixed. `KeepaliveConnection` now decodes `Transfer-Encoding: chunked` response bodies with `ChunkedDecoder`, returns the decoded buffered body, and retires the connection after every chunked response. Tests: `keepalive_decodes_chunked_response_and_retires_connection`, `chunked_then_content_length_requests_do_not_cross_contaminate`, `malformed_chunked_response_errors_and_retires`, `over_cap_chunked_response_errors_and_retires`, `chunked_connection_close_decodes_and_retires`, `chunked_smuggling_shape_is_retired_before_next_request`. |
| H14 | Fixed. The chunked decoder rejects SP/HT before the chunk-size digits. Test: `rejects_leading_whitespace_before_chunk_size`. |
| L13 | Fixed. `DataCrlf` checks the current remaining buffer, not the original feed input. Tests: `split_crlf_after_data`, `rejects_missing_data_crlf`. |
| A6 | Fixed. Chunk-size/body accounting uses checked arithmetic around decoded totals and overflow-shaped size lines. Tests: `rejects_body_too_large_after_prior_decoded_bytes`, `rejects_chunk_size_that_overflows_usize`. |
| M1 | Fixed. WebSocket client frames reject non-minimal 126/127 length encodings and 127-form high-bit lengths. Tests: `client_frame_rejects_non_minimal_126_length`, `client_frame_rejects_non_minimal_127_length`, `client_frame_rejects_127_length_high_bit`. |
| A7 | Fixed. WebSocket frame-end calculations use checked offsets and reject huge frame lengths before drain/decode. Test: `client_frame_rejects_huge_frame_before_end_offset_overflow`. |
| M6 | Already fixed in the connection delivery path; Phase 123 added a live regression proving invalid fragmented text is closed before app echo/delivery. Test: `websocket_fragmented_text_invalid_utf8_rejects_before_app_delivery`. |
| L14 | Fixed. HTTP/1 origin-form parsing rejects protocol-relative targets (`//host/path`). Test: `protocol_relative_target_is_not_origin_form`. |
| C2 | Fixed. HTTP/2 DATA strips PADDED bytes before body accounting, and HEADERS strips PADDED plus PRIORITY bytes before HPACK. Tests: `http2_padded_data_delivers_only_unpadded_body`, `http2_bad_data_padding_sends_protocol_goaway`, `http2_priority_headers_with_valid_hpack_succeeds`, `http2_padded_priority_headers_with_valid_hpack_succeeds`, `http2_malformed_padded_priority_headers_rejects`. |
| C3 | Fixed. SETTINGS frames are parsed and applied before ACK: peer `INITIAL_WINDOW_SIZE` updates open/new stream send windows, peer `MAX_FRAME_SIZE` controls outbound DATA splitting, invalid values reject, and unsupported non-default `HEADER_TABLE_SIZE` sends SETTINGS_ERROR. Tests: `http2_settings_initial_window_shrink_blocks_until_window_update`, `http2_settings_max_frame_size_controls_outbound_splitting`, `http2_invalid_settings_value_sends_goaway`, `http2_invalid_enable_push_value_sends_protocol_error`, `http2_non_default_header_table_size_sends_settings_error`. |
| C5 | Fixed with a conservative reset-churn guard that sends `ENHANCE_YOUR_CALM` once peer reset count exceeds the configured limit. Tests: `http2_rapid_reset_storm_sends_enhance_your_calm_goaway`, `http2_normal_reset_rate_allows_later_request`. |
| H9 | Fixed. HTTP/2 request headers reject HTTP/1 connection-control names. Test: `http2_forbidden_connection_header_rejects`. |
| M7 | Fixed in DATA/window hot paths by converting frame body lengths through checked `i32::try_from` before window arithmetic. Covered by `http2_padded_data_delivers_only_unpadded_body`, `http2_inbound_data_obeys_stream_window`, and `http2_settings_initial_window_shrink_blocks_until_window_update`. |
| M14 | Fixed. HTTP/2 requests require `:authority` or a non-empty host equivalent. Test: `http2_missing_authority_rejects`. |
| H1 | Fixed. `tina-rpc-tokio` `CancelGuard::drop` now returns a permit only when it actually removed the pending entry, matching the observer path. Test: `cancel_guard_drop_releases_only_when_it_removed_pending_entry`. |
| C4 | Fixed. Explicit and live multi-shard runtimes now route `CallReply` envelopes through a reserved terminal lane that drains before ordinary remote sends, so a saturated ordinary reverse queue cannot silently turn `Full`/`Closed` truth into timeout. Tests: runtime and simulator `terminal_reply_lane_bypasses_saturated_ordinary_remote_queue`, plus `live_cross_shard_isolate_call_destination_closed_returns_typed_closed` and `failed_shard_cross_shard_call_returns_one_closed_outcome`. |
| M13 | Fixed with the same terminal-lane contract as C4 for simulator and live multi-shard. Tests: runtime and simulator `terminal_reply_lane_bypasses_saturated_ordinary_remote_queue`. |
| H2 | Fixed. SQLx, AWS, and reqwest bridge docs/metrics now distinguish caller timeout, external in-flight work, and late terminal work; SQLx keeps external capacity occupied until terminal after caller timeout. Tests: `timeout_settles_caller_but_keeps_external_capacity_until_terminal`, `pressure_report_reflects_capacity_and_high_water`, AWS bridge timeout/late-result tests, and reqwest closed-worker tests. |
| H13 | Fixed. `tina-reqwest-bridge` treats a closed worker result channel as an internal fatal outcome, not as retryable network failure. Test: reqwest bridge closed-task regression in `tina-reqwest-bridge` tests. |
| M8 | Fixed. AWS late-result metrics separate caller-visible timeout from external terminal outcome without double-counting class counters. Tests: AWS bridge timeout/late-result metrics tests. |
| A1 | Fixed by removing DB-side cancel-on-timeout from the public SQLx timeout contract; Tina-side timeout stops waiting while the DB future remains quarantined against external capacity until terminal. Test: `timeout_settles_caller_but_keeps_external_capacity_until_terminal`. |
| H6 | Fixed. Threaded runtime shutdown now has explicit timeout-returning APIs and Drop uses a bounded best-effort wait rather than an unbounded join. Tests: `shutdown_with_timeout` tests and process/runtime shutdown tests. |
| L10 | Fixed on Unix by process-group cleanup and bounded drain joins; unsupported group-kill shapes report typed cleanup truth. Tests: `local_system_process_run_captures_truncates_and_times_out` and process timeout tests. |
| L11 | Fixed. Storage/DNS/TLS/process cancel loops no longer use pure `yield_now` spins on stuck paths; they use bounded sleep/backoff or blocking waits. Tests: process shutdown/cancel tests. |
| A2 | Fixed. Process timeout/cancel kills the Unix process group and returns bounded cleanup reports. Tests: process timeout and stdout-grandchild inheritance tests. |
| A3 | Fixed. `journal_replay` exposes valid prefix byte length for truncated tails and append repairs the tail before writing. Tests: persistence/journal truncated-tail append tests. |
| A4 | Fixed. Journal append validation uses tail metadata/sidecar state instead of full replay on every append. Test: append scaling instrumentation regression in persistence tests. |
| A5 | Fixed. Snapshot temp-file failure paths attempt cleanup while preserving primary error. Test: `snapshot_rename_failure_removes_temp_file`. |
| H4 | Fixed. Bounded trace retention no longer performs `Vec::remove(0)` on every event; it uses an offset plus chunked compaction. Tests: `bounded_trace_retention_does_not_move_the_tail_on_every_event`, `bounded_trace_retention_keeps_only_recent_events_and_counts_drops`. |
| H5 | Fixed. Added `BufferedTraceObserver` with bounded queueing and visible drop counts while retaining synchronous observers. Test: `buffered_observer_counts_drops_when_drain_is_full`. |
| H7 | Fixed. `RestartBudget` now has explicit `lifetime(max)` and windowed `within(max, period)` semantics. Tests: `lifetime_restart_budget_exhausts_permanently`, `windowed_restart_budget_resets_after_period`. |
| H10 | Fixed. `LiveTrace::snapshot` sorts by event id before hashing and poisoned mutex paths fail loudly. Tests: `live_replay` proof-harness tests. |
| H11 | Fixed. Settled stopped runtime entries are collected after restarts once no child/supervisor/in-flight references remain. Test: runtime supervision `windowed_restart_budget_resets_after_period` asserts entry count settles. |
| L5 | Fixed. Cancelled-call cause ring overflow is visible through `cancelled_call_cause_evictions()` in runtime and simulator. Tests: runtime and simulator `cancelled_call_cause_ring_overflow_is_visible`. |
| H3 | Fixed/proven first form. `PoolLease` remains sealed and `must_use`, pool reports expose leased resources, and close retires outstanding leases rather than pretending Drop can always send an effect from arbitrary context. Tests: pool report/close/lease-authority tests in `tina-runtime/tests/pool.rs`. |
| M5 | Fixed. RPC server connection tracks in-flight request ids and rejects duplicate ids without dispatching service work twice. Test: `duplicate_request_id_while_in_flight_returns_protocol_error_without_dispatch`. |
| M21 | Fixed/proven. Tokio RPC bridge shim capacity is tied to `max_in_flight * 2`, cancellation releases slots synchronously, and stale guards cannot inflate permits. Tests: `cancelled_call_releases_slot_synchronously`, `cancel_guard_drop_releases_only_when_it_removed_pending_entry`. |
| L1 | Fixed. `PendingReplies::take()` now has explicit `taken()` accounting separate from caller-gone reclaim. Test: `take_returns_and_removes_entry`. |
| L6 | Proven. Deferred slots keep ticket generations and call-handle ordering tests pin stale ticket behavior. Tests: deferred slot ticket tests in `tina-runtime/src/deferred.rs` and request/deferred integration tests. |
| L8 | Proven. Public pool constructors cannot forge leases; lease minting is sealed under `tina::pool::__private`. Tests: pool stale/forgery tests and compile-time private-field coverage. |
| L15 | Fixed/proven. SQLx `FetchMany` stops pulling after the documented cap plus truncation detection, and tests pin that the user buffer never exceeds the cap. Tests: `fetch_many_at_cap_truncates_without_buffering_extras`, `fetch_many_caller_max_rows_is_capped_by_config_ceiling`. |
| M2 | Fixed. `cancel_call` before call effect admission returns typed `CancelOutcome::NotAdmitted` and keeps the runtime alive. Tests: runtime and simulator cancel-before-admit tests. |
| M3 | Fixed. Huge duration call deadline math uses saturating Tina time helpers instead of panicking/wrapping. Test: `huge_duration_call_deadline_saturates_instead_of_panicking`. |
| M16 | Fixed. `ShutdownChoreography::record` reports highest completed step, not whichever step happened last. Test: `recurring_tick`/lifecycle tests. |
| M23 | Proven/contained. Runtime-owned signal waits are explicit capabilities; live Unix capture support is surfaced by `os_signal_capture_supported()` and non-Unix remains timeout/cancel-only. Tests: local-system signal rail tests and lifecycle signal-driven shutdown tests. |
| M24 | Fixed. TLS close wins over cancelled pending read/write pressure by removing/settling cancelled entries instead of returning `TlsFull` solely for cancelled work. Tests: TLS pressure and close tests in `local_system.rs`. |
| M25 | Fixed. Added host wait timeout variants/APIs so `call_blocking` host budget is distinct from target call deadline. Tests: `multi_shard_call_blocking_host_budget_is_distinct_from_target_deadline`, `multi_shard_call_blocking_returns_timeout_when_callee_holds_caller`. |
| L2 | Fixed by making address generations real routing truth in runtime and simulator. Tests: `runtime_ingress_to_wrong_generation_returns_closed`, `dispatched_send_to_wrong_generation_records_closed_rejection`, simulator stale-generation tests. |
| L3 | Fixed. `RecurringTick::Bounded(0)` missed-tick policy is pinned. Test: `recurring_tick` tests. |
| L4 | Fixed. `elapsed_periods` avoids silent `u128` to `u64` truncation. Test: `recurring_tick` tests. |
| L16 | Fixed. Trace projection preserves `CallError::Rejected(reason)` inner reason. Test: `call_failed_rejected_error_preserves_inner_reason`. |
| H8 | Fixed first form. Public TLS lane docs/reports now state queue depth vs concurrency truth: one TLS worker per shard drains a bounded queue. Tests/docs: TLS lane pressure report coverage and `docs/tina-user-guide/12-io-model.md`. |
| H12 | Fixed first form. `#[tina::isolate]` / `#[tina_runtime::isolate]` accept `tina_crate = ...` and `runtime_crate = ...` path overrides, `#[tina_rpc::service]` accepts `tina_crate = ...` and `rpc_crate = ...`, and defaults use `core::convert::Infallible` where possible. Tests: `tina-macros` lib compile, runtime surface-alignment tests, and `service_macro_accepts_renamed_dependency_paths`. |
| M11 | Fixed. SPSC requires power-of-two capacity and rejects non-power-of-two inputs. Test: `mailbox_rejects_non_power_of_two_capacity`. |
| M12 | Fixed/proven. TLS blocking stream access remains owner-worker-thread only by API shape, and docs name the mutex scope. Tests: TLS local-system tests. |
| M22 | Fixed. Macro-generated default `Infallible` paths use `core::convert::Infallible`. Tests: `tina-macros` lib compile and runtime surface-alignment tests. |
| L7 | Proven. Split request/call-authority trybuild fixtures reject ignored/dropped/double-consumed authority while allowing valid helper-shaped use. Tests: `tina-runtime --test safety_rails_diagnostics`. |
| L9 | Proven/documented. RPC macro tuple ABI remains the named first-form encoding and decode/dispatch tests pin the shape. Tests: `tina-rpc-macros` and `tina-rpc` tests. |
| L12 | Fixed. SPSC Loom tests cover close racing producer/consumer interleavings. Tests: `close_waits_for_a_racing_successful_send_to_become_visible`, `close_racing_with_recv_still_preserves_buffered_delivery`. |
| M9 | Fixed. Simulator fault selection now uses deterministic SplitMix streams per tag instead of `(seed + tag + ordinal) % one_in`. Tests: `fault_selector_is_deterministic_and_tag_separated`, `different_seeds_diverge_under_tcp_delay_faults`, `different_seeds_diverge_under_tcp_ready_reordering`. |
| M10 | Proven contained. Runtime/simulator replay facts and trace hashes encode virtual-time durations/config structurally rather than raw `Instant`s; docs explicitly warn that user payloads should not store raw `Instant` for byte-identical replay. Tests: saved replay config/hash tests in `tina-sim/src/dst.rs`. |
| M15 | Fixed. `LiveTrace` poisoned mutex handling no longer silently blesses a bad hash. Tests: proof-harness live replay tests. |
| M18 | Fixed. Bad-peer reset scenario naming/behavior and bridge overlap are covered by bad-peer harness tests. Tests: `bad_peer` proof-harness tests. |
| M19 | Fixed. Load report first-error indexing truth is global or named correctly. Tests: `load` proof-harness tests. |
| M20 | Fixed. Storm/load reports preserve aggregate connection error truth. Tests: `bad_peer` and `load` proof-harness tests. |
| L17 | Fixed. `MultiShardSimulator` supports trace observers. Test: simulator trace-observer tests. |
| L18 | Fixed. Saved replay cases compare structured `ReplayConfig`/projection values and reject mismatches, not only `Debug` strings. Tests: `check_replay_case_rejects_report_config_mismatch`, `captured_replay_mismatch_names_every_changed_fact`. |
| M4 | Fixed. SQLx transaction COMMIT failure now returns `PgTransactionOutcome::CommitAmbiguous { completed, error }` so completed step records are not lost. Test: `commit_ambiguous_transaction_preserves_completed_steps_and_error`. |
| M17 | Fixed. `tina-tokio-bridge` has `drain_and_shutdown_async` so Tokio callers do not park a worker with a sleep loop. Test: `async_drain_and_shutdown_yields_to_handle_drop_task`. |

## Critical

### C1. HTTP/1 keepalive client smuggles chunked response bytes into the next request

- Confidence: High. `tina-http/src/keepalive.rs:713-719`, gated by
  `tina-http/src/parse.rs:577-582`.
- `body_complete()` checks only `head.content_length`, ignores
  `head.chunked`. `parse_response_head` accepts
  `Transfer-Encoding: chunked` and reports `content_length = 0`. The pool
  returns an empty body and keeps the connection live with all chunked
  bytes still in `read_buf`. The next request on that slot parses
  chunk-size hex as the next response head — classic response smuggling.
- Repro: `KeepaliveConnection` against any HTTP/1.1 origin that returns
  chunked. First call returns `Ok({ body: [] })`; second fails with
  `BadStatusLine` or returns the chunk bytes as the next "response."
- Fix: when `head.chunked == true`, set `must_retire = true` and refuse to
  deliver, or run `chunked_decoder::ChunkedDecoder` on the body path. Do
  not fail-open.

### C2. HTTP/2 ignores the `PADDED` and `PRIORITY` flags on DATA / HEADERS

- Confidence: High.
  `tina-http/src/http2.rs:796-820, 848-910, 912-999`.
- Frame handlers never check flag bits `0x8` (PADDED) or `0x20` (PRIORITY
  on HEADERS). The pad-length byte is consumed as data; the 5-byte
  priority block is fed to HPACK. Data corruption on padded DATA, HPACK
  decode failure (or partial-valid header set including
  attacker-controlled fields) on padded/prioritized HEADERS.
- Fix: before HPACK / body buffer, strip `pad_length` byte and trailing
  padding when `flags & 0x8 != 0`; skip 5 priority bytes when frame is
  HEADERS and `flags & 0x20 != 0`. Reject `pad_length >= payload.len()`
  as PROTOCOL_ERROR.

### C3. HTTP/2 SETTINGS payload silently dropped

- Confidence: High. `tina-http/src/http2.rs:822-836`.
- `handle_settings` validates `payload.len() % 6 == 0` then ACKs. It never
  iterates settings. Peer's `INITIAL_WINDOW_SIZE`, `MAX_FRAME_SIZE`,
  `MAX_CONCURRENT_STREAMS`, `HEADER_TABLE_SIZE` are not applied. Server
  can exceed peer flow-control credit; peer tears down with
  FLOW_CONTROL_ERROR.
- Fix: parse each 6-byte tuple. Apply `INITIAL_WINDOW_SIZE` as a delta to
  all open streams' send windows, cap outbound frame size, push
  `HEADER_TABLE_SIZE` to the HPACK encoder.

### C4. Cross-shard call reply silently dropped when reverse remote queue is full

- Confidence: High. `tina-runtime/src/threaded_multi_shard.rs:938-942`.
- When a `Full`/`Closed` `CallReply` envelope needs to re-route to the
  requester's shard, `drain_remote_inbound` does
  `let _ = route_remote(outbound)` and discards the error. Burst A→B
  saturates B's mailbox → B produces `Full` replies → A←B remote queue
  also saturated → reply dropped → requester sees `CallOutcome::Timeout`
  instead of `Full`. Backpressure becomes tail latency.
- Fix: on `route_remote` `Err`, push to a runtime-owned reply-retry
  buffer that drains with priority, or synthesize a local deferred-slot
  close on the requester via a back-channel.

### C5. HTTP/2 has no rate limit on RST_STREAM (rapid-reset shape, CVE-2023-44487)

- Confidence: High.
  `tina-http/src/http2.rs:873, 1020-1060`.
- `max_concurrent_streams` counts only currently-open streams; RST_STREAM
  frees the slot. Attacker opens + immediately resets streams at line
  rate. Each cycle does HPACK decode + service dispatch + cancellation.
  CPU starvation without exceeding the concurrent-stream cap.
- Fix: sliding-window counter of streams reset within a period;
  GOAWAY(ENHANCE_YOUR_CALM) when over threshold. Optionally cap streams
  without END_STREAM.

## High

### H1. Bridge `CancelGuard::drop` unconditionally `add_permits(1)` — double-release race

- Confidence: High. `tina-rpc-tokio/src/lib.rs:578-591`.
- The observer path guards "only release if I removed the entry."
  `CancelGuard::drop` does not. Race: shim removes entry,
  `add_permits(1)`, schedules the awaiter; caller drops the future
  before it polls → `CancelGuard` runs → `pending.remove` returns None
  but `add_permits(1)` fires again. The bridge's admission cap inflates
  over time.
- Fix: guard the `add_permits(1)` on a `pending.remove(...).is_some()`
  like the observer path does.

### H2. Bridge timeout capacity truth diverges after user-visible timeout

- Confidence: High. `tina-sqlx-bridge/src/worker.rs:355-457`;
  `tina-aws-bridge/src/{worker,dynamodb_worker,sqs_worker,sns_worker,secrets_worker}.rs`
  poll loops.
- SQLx path: on per-attempt timeout, the worker removes the entry,
  marks it abandoned, calls `note_terminal`, replies `PgError::Timeout`,
  and does not abort the spawned SQLx future. Tina admission capacity is
  freed while the database work may still be running. Real concurrent DB
  work can exceed `max_in_flight` until late futures complete.
- AWS path: on per-attempt timeout, the worker replies `Timeout` but
  reinserts the abandoned in-flight entry and keeps polling until the
  SDK future completes. A stuck SDK future can permanently consume
  admission capacity and make later calls return `Full`.
- Both violate the user expectation behind `max_in_flight`: after a
  timeout, admission capacity and late physical work must be named
  separately.
- Fix: split user-visible admission capacity from physical late-work
  tracking. Either bound both explicitly (`admitted_in_flight` and
  `late_in_flight`) or keep admission occupied until physical completion
  but name that as the contract. Do not let SQLx and AWS teach opposite
  meanings for the same bridge vocabulary.

### H3. `PoolLease<H>` has no `Drop` — silent resource leak on panic / early return / `OneForAll`

- Confidence: High. `tina/src/pool.rs:107-114`; no Drop in
  `tina-runtime/src/pool.rs`.
- Pool resources transition Idle → Leased on acquire and only return to
  Idle on an explicit `Release` message or pool close. There is no Drop
  hook. Handler panics, isolate force-stop, or storing leases in `Vec`
  and clearing them leak resources permanently. Documented as "drop
  leaks until pool close" but no signal to operators.
- Fix: implement `Drop` that posts a release via a captured back-channel,
  or expose a closure-scoped `pool.with_lease(|...| {...})` API that
  cannot be stored across turns.

### H4. Trace ring `Vec::remove(0)` on hot path — O(capacity) per event

- Confidence: High. `tina-runtime/src/lib.rs:3538-3548`.
- `TraceRetention::Bounded(N)` calls `self.trace.remove(0)` on every
  overflow, memmoving `(N-1) * sizeof(RuntimeEvent)` each event.
  Steady-state with N=8192 is multi-GB/s of memmove on a busy shard.
- Fix: `VecDeque<RuntimeEvent>`. `pop_front` is O(1).

### H5. `TraceObserver::on_event` is synchronous on the shard turn

- Confidence: High. `tina-runtime/src/lib.rs:3528-3533`,
  `tina-tracing/src/observer.rs:32-36`.
- `push_event` calls `observer.on_event` inline. The default
  `TracingObserver` hands to `tracing::event!`, which calls the global
  subscriber synchronously — `fmt` subscriber takes `Mutex<Stdout>`.
  Under load, stdout becomes the shard bottleneck.
- Fix: ship a `BufferedObserver` backed by an SPSC queue with a
  dedicated drain thread. Document `TracingObserver` as synchronous
  explicitly.

### H6. `Drop` of the threaded runtime blocks forever on a wedged user handler

- Confidence: High. `tina-runtime/src/shutdown.rs:217-247`.
- `shutdown_blocking` does `sender.send(Shutdown)` (blocking) and
  `handle.join()` with no wall-clock budget. A handler stuck in
  `loop {}`, blocking FFI, or `call_blocking` from inside a handler
  never returns. Process hangs at Drop.
- Fix: add `force_shutdown_after: Duration` and switch to `try_send` +
  `LiveShardState::Failed` once exceeded; bound the joiner wait too.

### H7. Supervisor `RestartBudget` has no time window — exhausts permanently

- Confidence: High. `tina/src/lib.rs:791-872`,
  `tina-runtime/src/lib.rs:3380-3401`.
- The type is named "Budget" with Erlang vibes but is a lifetime counter
  that never resets. A week-long process with 100-restart budget that
  hits 101 unrelated transient faults at hour 17 permanently refuses to
  restart any failing child. Operators expecting OTP
  `{intensity, period}` semantics get silent degradation.
- Fix: rename to `RestartLimit` (honest), or add
  `RestartBudget::within(max, Duration)` with a VecDeque of timestamps
  pruned by age.

### H8. Per-shard TLS worker thread serializes all TLS work

- Confidence: High. `tina-runtime/src/driver/tls.rs:803-814, 1026-1080`.
- One worker per shard processes connect/bind/accept/read/write/close
  serially. A slow handshake (silent client + read timeout) blocks all
  other TLS work on that shard. `tls_lane_capacity = 64` reads like
  "64 concurrent TLS ops" but means "1 concurrent, 64-deep queue."
- Fix: pool the worker (`min(num_cpus/shard_count, 8)`) or use one
  worker per accepted stream; switch `accept_tls` away from polling.

### H9. HTTP/2 does not reject HTTP/1 connection-specific headers — smuggling vector

- Confidence: High. `tina-http/src/http2.rs:1780-1788, 366-410`.
- RFC 7540 §8.1.2.2 forbids `Connection`, `Keep-Alive`,
  `Proxy-Connection`, `Transfer-Encoding`, `Upgrade` in HTTP/2 requests.
  Tina's validator only checks pseudo-headers. If any downstream
  component converts the request to HTTP/1, HTTP/1 desync rematerializes.
- Fix: reject these names in `add_header` (or
  `validate_request_headers`). Also reject uppercase in header names per
  §8.1.2.

### H10. `LiveTrace` snapshot hash is non-deterministic across runs on threaded runtime

- Confidence: High. `tina-proof-harness/src/live_replay.rs:154-159`.
- `on_event` does `events.lock().push(*event)` from multiple shards;
  landing order is mutex-arrival order. `stable_trace_hash` is
  order-sensitive (see `tina-runtime/src/trace.rs:1397,1492`).
  Regression tests comparing `LiveTrace::compare_live_shape` will flake.
- Fix: sort by `event.id()` in `snapshot()` before hashing (parallels
  what `MultiShardSimulator::trace()` already does).

### H11. `entries` Vec grows unbounded across supervised restarts (memory leak)

- Confidence: High. `tina-runtime/src/lib.rs:3851-3853`.
- `register_entry` pushes; restart creates a fresh `IsolateId` via a new
  entry; stopped entries are marked `stopped=true` and never removed. A
  service that restarts a child every second leaks ~3600 entries/hour.
  Every IsolateId lookup is O(n) over `self.entries`.
- Fix: GC stopped entries once their last in-flight call settles; add
  an O(1) `IsolateId → index` map.

### H12. Proc-macro hardcoded paths `::tina::*` / `::tina_runtime::*` / `::tina_rpc::*`

- Confidence: High. `tina-macros/src/lib.rs`, `tina-rpc-macros/src/lib.rs`
  (many sites).
- If a user renames in Cargo.toml
  (`tina = { package = "tina-rs" }`), patches the dep, vendors with a
  different name, or has a top-level `mod tina`, compilation fails.
- Fix: `#[doc(hidden)] pub mod __private` re-export in each parent
  crate; macros emit via the re-export; add a `tina_crate = path`
  attribute escape hatch.

### H13. Reqwest bridge retries on `TryRecvError::Closed` — non-idempotent POST replay

- Confidence: High. `tina-reqwest-bridge/src/worker.rs:558-580`.
- `Closed` on the spawn's oneshot means the task panicked or runtime was
  torn down, not a network IO error. If the panic happened after the
  request hit the wire, retry replays the non-idempotent operation.
- Fix: classify `Closed` as `Internal("task vanished")`; do not retry on
  it. Only retry on explicit reqwest IO errors.

### H14. Chunked decoder accepts whitespace-prefixed chunk size lines

- Confidence: High. `tina-http/src/chunked_decoder.rs:290-319`.
- The trim loop strips leading SP/HT before the chunk-size hex. RFC 7230
  §4.1 forbids that. Intermediaries that follow the RFC strictly will
  parse the same bytes differently — frame-boundary smuggling.
- Fix: remove the leading trim. Reject any whitespace before chunk-size.

## Medium

- M1. WebSocket payload length non-minimal-form not rejected; 64-bit
  top bit not enforced. `tina-http/src/websocket.rs:740-765`.
- M2. `cancel_call` dispatched before its call effect panics the shard.
  `tina-runtime/src/lib.rs:2606-2611`. Replace `.expect` with a typed
  outcome.
- M3. `Instant + Duration` overflow on call deadline.
  `tina-runtime/src/lib.rs:2552`. Use `checked_add_or_max` to match the
  pattern in `tina/src/time.rs:198,215`.
- M4. SQLx transaction commit-ambiguous loses step record.
  `tina-sqlx-bridge/src/worker.rs:949-955`. Add
  `PgTransactionOutcome::CommitAmbiguous { completed, error }`.
- M5. `Connection.in_flight` is a count, not a set — peer can amplify
  server work via duplicate request_id.
  `tina-rpc/src/connection.rs:357,538-560`.
- M6. WebSocket text-frame UTF-8 validation deferred until reassembly.
  `tina-http/src/connection.rs:1641-1657`.
- M7. HTTP/2 `i32` casts for body / window math allow config-driven
  sign flip past 2 GB. `tina-http/src/http2.rs` (many sites).
- M8. AWS late-result tally double-counts class metrics.
  `tina-aws-bridge/src/dynamodb_worker.rs:325-334` (same shape in
  sqs/sns).
- M9. Fault selector is `(seed + tag + ordinal) % one_in`, not a real
  PRNG. `tina-sim/src/lib.rs:4856-4884, 3877-3911`. Use ChaCha8Rng per
  simulator with per-tag streams.
- M10. `virtual_anchor = Instant::now()` leaks wall-clock into trace
  through handler-returned Instants.
  `tina-sim/src/lib.rs:127,192,495`.
- M11. SPSC mailbox: non-power-of-two capacity unsound across `usize`
  wraparound. `tina-mailbox-spsc/src/lib.rs:115-117, 167, 177, 196`.
  Require power-of-two; mask instead of mod.
- M12. `tls.rs`: blocking `read`/`write`/`flush` syscalls held under
  `Arc<Mutex<TlsRuntimeStream>>`. Owner-thread-only today; the shape
  invites future deadlock.
- M13. Cross-shard relay envelope dropped without trace event when
  destination shard queue is full. `tina-sim/src/multi_shard.rs:336-338`
  (simulator-side shape of C4).
- M14. HTTP/2 `:authority` / `Host` not required.
  `tina-http/src/http2.rs:1780-1788`.
- M15. `LiveTrace` `if let Ok(...)` drops on poisoned mutex; replay
  baseline locks in wrong hash if captured during a poisoned run.
  `tina-proof-harness/src/live_replay.rs:155-159`.
- M16. `ShutdownChoreography::record` reports the last step, not the
  highest-ordinal step, as `previous_step`.
  `tina-runtime/src/lifecycle.rs:883-909`.
- M17. Tokio bridge `drain_and_shutdown` is a `std::thread::sleep`
  polling loop; called from a tokio worker (Axum graceful shutdown) it
  parks the worker. `tina-tokio-bridge/src/lib.rs:732-759`.
- M18. `BadPeerScenario::ResetImmediately` does FIN, not RST. Specimens
  claiming to test RST handling actually test graceful close.
  `tina-proof-harness/src/bad_peer.rs:30-31,264-275`.
- M19. `LoadReport::first_error_op_index` semantics are local-not-global.
  `tina-proof-harness/src/load.rs:111-115,321-325`.
- M20. `run_storm` overwrites all but the last connection error; reports
  `connected=true` even when 99% failed.
  `tina-proof-harness/src/bad_peer.rs:388-411`.
- M21. Bridge shim mailbox sizing math holds for one cancellation wave
  only. `tina-rpc-tokio/src/lib.rs:350-355`. Two back-to-back
  cancel-and-retry cycles can stack 3× max_in_flight pending replies.
- M22. Proc-macro emits `::std::convert::Infallible` instead of
  `::core::convert::Infallible`. Breaks `no_std + alloc` consumers.
  `tina-macros/src/lib.rs:209,212,215,219`, `tina/src/lib.rs:249`,
  `tina-rpc-macros/src/lib.rs:532`.
- M23. Driver SIGINT/SIGTERM registrations multiplied by shard count.
  Library embedders' prior `ctrlc::set_handler` is silently demoted.
  `tina-runtime/src/driver/signals.rs:44-63`,
  `tina-runtime/src/driver/mod.rs:279`.
- M24. TLS `submit_close` capacity check counts cancelled-but-still-
  pending ops; close on a hot stream can be rejected `TlsFull`.
  `tina-runtime/src/driver/tls.rs:563`.
- M25. `call_blocking` host wait uses target-call timeout, not host
  budget. `tina-runtime/src/threaded.rs:1200-1207`,
  `tina-runtime/src/threaded_multi_shard.rs:706-715`.

## Low (selected)

- L1. `PendingReplies::take()` bypasses `reclaimed` counter.
  `tina-runtime/src/deferred.rs:451-459`.
- L2. `AddressGeneration` always 0 — dead field.
  `tina-runtime/src/lib.rs:1241,3851,3957`.
- L3. `RecurringTick::Bounded(0)` reports `missed_ticks` off-by-one vs
  `Skip`. `tina/src/time.rs:738-754`.
- L4. `elapsed_periods` u128→u64 truncation. `tina/src/time.rs:691`.
- L5. `CANCELLED_CALL_RING_CAPACITY = 64` hard-coded; downgrades cancel
  cause attribution at >64 concurrent cancels.
  `tina-runtime/src/lib.rs:423,2818`.
- L6. `DeferredSlotShared` uses `Relaxed` while parallel
  `CallHandleShared` uses `Acquire/Release`. Invariant trap.
  `tina/src/lib.rs:2387,2403`.
- L7. Proc-macro `require_call_authority_mentioned` is a pre-expansion
  textual heuristic; rejects helper-macro-based handlers.
  `tina-macros/src/lib.rs:539-555`.
- L8. `unsafe fn` used as contract marker, not memory safety;
  desensitizes reviewers. `tina/src/pool.rs:409-455`,
  `tina-runtime/src/pool.rs:58-63,378-385`.
- L9. tina-rpc-macros positional-tuple ABI is silently unstable across
  arg reorders. `tina-rpc-macros/src/lib.rs:474-489`.
- L10. Storage / DNS / TLS / process `cancel_pending` use
  `thread::yield_now()` busy spin; burns a core per stuck lane.
- L11. `process.rs` `kill_and_reap` `child.wait()` unbounded after
  SIGKILL; wedges process lane indefinitely.
- L12. SPSC mailbox: loom does not exercise the 3-thread
  (producer + consumer + closer) interleaving.
- L13. Chunked decoder `DataCrlf` branch checks `input.len()` where it
  should check `remaining.len()`; currently masked, fragile.
  `tina-http/src/chunked_decoder.rs:132-143`.
- L14. `is_origin_form` accepts `//attacker.com/path` — protocol-relative
  path confusion. `tina-http/src/parse.rs:245-250`.
- L15. SQLx FetchMany "cap+peek" leaks one row's worth of decode/
  bandwidth on the truncation edge.
  `tina-sqlx-bridge/src/worker.rs:898-921, 1012-1035`.
- L16. `events::call_error_name` flattens `CallError::Rejected(reason)`
  to a single string; dashboards lose the inner reason.
  `tina-tracing/src/events.rs:696`.
- L17. `MultiShardSimulator` exposes no `set_trace_observer`; cannot
  wire `LiveTrace` to multi-shard sim.
- L18. SavedReplayCase persists `Debug`-formatted `projection_debug` /
  `config_debug` and the loader never verifies them; false-friend
  invariant. `tina-sim/src/dst.rs:1972-1976`.

## Additional findings from narrow review

- A1. Postgres DB-side cancel can target the wrong query. The SQLx
  timeout path captures a backend PID and later fires
  `SELECT pg_cancel_backend(pid)` from a sidecar task. If request A
  finishes near the timeout boundary and the pool reuses that backend
  for request B before the sidecar cancel lands, request B can be
  cancelled. Fix by keeping cancellation on the connection-owner path,
  quarantining/dropping the connection until cancel completes, or
  disabling DB-side cancel unless it can be tied to a specific query
  incarnation.
- A2. Process timeout can hang when grandchildren inherit stdout/stderr.
  The runtime kills the direct child and joins drain threads; a
  grandchild holding inherited pipes can keep those drains open forever.
  Fix with Unix process groups / Windows job objects, plus bounded drain
  joins after kill.
- A3. Crash-truncated journal tails recover for read but block future
  appends. `replay_journal` can return a valid prefix with warning, but
  append validation rejects warnings. Fix by exposing valid byte length
  and truncating/repairing before the next append.
- A4. Journal append validation is O(total journal size). Every append
  replays the whole journal. Fix by tracking last committed index or
  validating only the tail; keep full replay for startup/repair.
- A5. Snapshot temp file cleanup is incomplete after rename failure.
  Best-effort remove the temp path on any failure after temp creation.
- A6. Chunked decoder length accounting needs checked arithmetic for
  peer-controlled chunk sizes. This sits next to H14/L13 and should be
  fixed in the same parser hardening pass.
- A7. WebSocket frame parsing should reject non-minimal extended payload
  lengths and use checked end-offset math. This is the same strictness
  family as M1.
- A8. HTTP/2 unary buffered requests do not enforce `content-length`.
  The only `request_content_length` assignment is in the gRPC streaming
  path; ordinary buffered HTTP/2 requests can declare `content-length: 0`
  and deliver DATA bytes, or declare one length and deliver another.
  Downstream handlers receive a body/header pair that violates the HTTP
  contract. Fix by parsing and validating every `content-length` value
  during header validation, rejecting invalid/conflicting duplicates, and
  enforcing exact length for buffered and streaming request paths.
  `tina-http/src/http2.rs:1062-1084,1120-1127,1780-1788`.
- A9. HTTP/2 known-length streaming responses emit `content-length` but do
  not enforce it. `begin_streaming_response` writes the declared length,
  while `handle_stream_chunk` / `flush_response_stream` track only response
  caps and flow-control bytes. A source can under-produce and still send
  END_STREAM, or over-produce past the declared length. Fix by storing a
  per-stream remaining declared byte count, decrementing on DATA, and
  resetting/cancelling on early EOF or overrun.
  `tina-http/src/http2.rs:1274-1285,1392-1459,1490-1520`.
- A10. HTTP/2 duplicate pseudo-headers overwrite instead of reject.
  `add_header` assigns `:method`, `:path`, `:scheme`, `:authority`, and
  `:status` into `Option` fields without checking whether a value was
  already present. Last-value-wins creates routing/signature ambiguity and
  violates the HTTP/2 malformed-request rules. Fix by rejecting any
  repeated pseudo-header before assignment.
  `tina-http/src/http2.rs:366-410,1780-1788`.
- A11. HTTP/2 treats core `CONTINUATION` / standalone `PRIORITY` frame
  validation like ignorable extension handling. `FRAME_PRIORITY` returns
  `Ok(())` without checking stream id or 5-byte payload length, and
  `CONTINUATION` is not defined so it falls through `_ => Ok(())`. Fix by
  explicitly rejecting unsupported CONTINUATION state and validating
  PRIORITY frame shape.
  `tina-http/src/http2.rs:796-820`.
- A12. Multi-shard remote inbound traffic can starve local control
  commands, including shutdown. The worker only polls its local command
  queue when `drain_remote_inbound` delivered zero envelopes; a steady
  remote flood can keep `Run` and `Shutdown` commands unread. Fix with fair
  scheduling: after each bounded remote drain pass, service at least one
  local command, and prioritize shutdown over ordinary remote work.
  `tina-runtime/src/threaded_multi_shard.rs:868-903,916-950`.

## Highest-risk modules reviewed

1. `tina-http/` — HTTP/1 keepalive (C1), HTTP/2 protocol surface (C2,
   C3, C5, H9, A8, A9, A10, A11, M1, M6, M7, M14), chunked decoder
   (H14, L13). Highest density of exploitable findings.
2. `tina-runtime/` — multi-shard relay (C4), shutdown (H6), supervisor
   (H7), TLS driver (H8), trace ring (H4, H5), call/lifecycle (M2, M3,
   M16, M25), entries leak (H11), signals (M23), multi-shard fairness
   (A12).
3. Bridges — sqlx/AWS in-flight semantics (H2), SQLx DB-side cancel
   identity (A1), reqwest retry classification (H13), shim cancel race
   (H1, M21), tower/tokio bridge surface (M17, M8, M4).
4. `tina-sim/` + proof-harness — determinism (H10, M9, M10, M13),
   bad-peer fidelity (M18, M20), load report (M19).
5. Macros — hygiene (H12, M22), heuristic lint (L7).

## Areas that still need deeper review

- TLS verification on the client path (`target.rs`, cert validation,
  hostname checks, session-resumption races) — only skimmed.
- HTTP/2 HPACK encoder for table-size synchronization bugs after a
  SETTINGS fix.
- `tina-runtime/src/driver/storage.rs` journal/snapshot atomicity — only
  the cancel loop was reviewed.
- Persistence flow — replay determinism vs live IO ordering.
- `tina-supervisor` integration tests vs the budget semantics in H7.
- Existing `grpc_live.rs` / `http2_live.rs` / `websocket_live.rs`
  integration tests — pass under current code but will not catch C1–C5.

## Suggested fuzz / property / integration tests

- HTTP/1 keepalive: fuzz a server emitting arbitrary mixtures of
  Content-Length, chunked, leading whitespace, lone CR/LF; assert no
  smuggling reaches request 2.
- HTTP/2: frame-shape fuzzer (length × flags × stream_id × payload
  prefix). Targets: padded DATA/HEADERS, standalone CONTINUATION,
  malformed PRIORITY, duplicate pseudo-headers, `content-length`
  overrun/underrun, oversized stream IDs, RST_STREAM storm,
  `:authority`-less request, illegal connection headers.
- HTTP/2 streaming responses: property test that `stream_known_length(N)`
  emits exactly N bytes before END_STREAM; short sources and overlong
  sources must fail visibly.
- WebSocket: payload-length minimal-form property test; UTF-8 fragment
  property test; control-frame fragmentation; close-code validation.
- Bridges: property test — for any sequence of (admit, timeout, retry,
  cancel) events, admitted work and late physical work obey their named
  budgets. Fails today.
- Process timeout: child spawns a grandchild that inherits stdout/stderr;
  timeout must settle and not hang the process lane.
- Persistence: truncated journal tail repairs before append; append
  latency must not scale with full journal size on the hot path.
- SPSC mailbox: 3-thread loom model (producer + consumer + closer);
  `usize::MAX` wraparound test on 32-bit.
- Cross-shard relay (C4): property test — every send produces exactly
  one observed terminal outcome (Accepted / Full / Closed / Timeout).
  Fails today.
- Trace determinism: property — same seed under threaded runtime
  produces identical `LiveTrace::snapshot().trace_hash` (after H10 fix).
- PoolLease (H3): property — for any sequence of acquires, drops via
  panic, and OneForAll restarts, `pool.live_count + pool.idle_count ==
  capacity` eventually. Fails today.

## Invariants the code should enforce but currently does not

- Every `CallHandle` settles exactly once with a typed terminal cause —
  violated by L5 (ring overflow downgrades cause to `NoPendingCall`).
- A bridge `Full`/`Closed` reply is delivered or surfaced as Timeout,
  never silently dropped — violated by C4.
- Pool `leased` count == handlers actually holding resources — violated
  by H3.
- Bridge timeout capacity has one named contract across crates —
  violated by H2 after first timeout.
- Server emits exactly one Reply per accepted request id at any time —
  not enforced by M5.
- `RestartBudget` represents a finite-window failure rate — false claim
  per H7.
- `LiveTrace` snapshot hash is deterministic for a given workload —
  violated by H10.
- HTTP/2 connection rejects HTTP/1 connection-control headers — false
  per H9.
- HTTP/2 `content-length` is truthful for both request bodies and
  known-length streaming responses — violated by A8 and A9.
- HTTP/2 pseudo-header sets contain each pseudo-header at most once —
  violated by A10.
- Local control-plane commands eventually run under remote pressure —
  violated by A12.

## Top 10 to fix first

1. C1 — HTTP/1 chunked keepalive smuggling.
2. C4 — Cross-shard reply drop on saturated reverse queue.
3. A8 + A9 — HTTP/2 `content-length` lies on requests and streaming
   responses.
4. C2 — HTTP/2 PADDED / PRIORITY flags ignored.
5. C5 — HTTP/2 RST_STREAM flood (rapid reset).
6. A12 — Multi-shard remote flood can starve local commands/shutdown.
7. H1 — Bridge `CancelGuard::drop` double-release.
8. H3 — `PoolLease` missing Drop hook.
9. H6 — Drop hangs forever on wedged handler.
10. C3 + H9 — HTTP/2 SETTINGS ignored + HTTP/1 headers accepted (pair:
   conformance and smuggling).
