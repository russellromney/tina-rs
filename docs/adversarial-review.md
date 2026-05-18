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

## Highest-risk modules reviewed

1. `tina-http/` — HTTP/1 keepalive (C1), HTTP/2 protocol surface (C2,
   C3, C5, H9, M1, M6, M7, M14), chunked decoder (H14, L13). Highest
   density of exploitable findings.
2. `tina-runtime/` — multi-shard relay (C4), shutdown (H6), supervisor
   (H7), TLS driver (H8), trace ring (H4, H5), call/lifecycle (M2, M3,
   M16, M25), entries leak (H11), signals (M23).
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
  prefix). Targets: padded DATA/HEADERS, oversized stream IDs,
  RST_STREAM storm, `:authority`-less request, illegal connection
  headers.
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

## Top 10 to fix first

1. C1 — HTTP/1 chunked keepalive smuggling.
2. C4 — Cross-shard reply drop on saturated reverse queue.
3. C2 — HTTP/2 PADDED / PRIORITY flags ignored.
4. C5 — HTTP/2 RST_STREAM flood (rapid reset).
5. H1 — Bridge `CancelGuard::drop` double-release.
6. H3 — `PoolLease` missing Drop hook.
7. H2 — Bridge timeout capacity semantics split across sqlx / AWS.
8. H6 — Drop hangs forever on wedged handler.
9. C3 + H9 — HTTP/2 SETTINGS ignored + HTTP/1 headers accepted (pair:
   conformance and smuggling).
10. H7 — Supervisor `RestartBudget` has no time window.
