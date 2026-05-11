# Changelog

This file records completed work.

## Unreleased

### Phase 070 Small Sharded Ergonomics

- Added `ShardBatch<T>` and `GroupByOwnerError::CapExceeded` to
  `tina_runtime::sharded`.
- Added `ShardPlacement::group_by_owner_bytes` and
  `ShardPlacement::group_by_owner_str` for bounded grouping of keyed items by
  owner shard. Output follows `placement.shards()` order; empty shards are
  omitted; too many items returns a typed cap error.
- Added integration tests proving placement-order preservation,
  non-contiguous shard ids, empty input, cap exceeded, duplicate-key grouping,
  and byte-identical live/sim mapping.
- Added `hot_key_pressure_first_attempt_only_no_retry_loop` test to
  `sharded_primitives.rs`, making explicit the first-attempt-only pressure
  report shape when the caller does not retry.
- Updated `docs/tina-user-guide/10-service-patterns.md` with group-by-owner
  snippet, hot-key pressure paragraph, and caller-owned retry paragraph.

### Phase 078 Host-Side Ergonomics

- Expanded docs for `ThreadedRuntime::call_blocking` with full example,
  usage guidance, and a warning that it is host/test only and must never
  be called from inside an isolate handler.
- Added trace query helpers to `RuntimeTraceExt`:
  `count_matching` / `any_matching`, `count_spawned` / `any_spawned`,
  `count_call_dispatched` / `any_call_dispatched`.
- Migrated `tina-http/tests/client_bad_input.rs` and
  `tina-http/tests/client_against_native.rs` from fake host Driver
  isolates to direct `call_blocking` calls, removing ~130 lines of
  ceremony while preserving `CallOutcome` visibility.

### Response body source cancel (079 PR 1)

`ResponseChunkMsg::Cancel` is a new typed variant sent to chunk sources
when the HTTP connection abandons the wire mid-stream. Sources can
release files, downstream calls, and pending slots. `IterBodySource`
handles it with `stop()`.

The connection isolate sends `Cancel` on every wire-death path:
`Read(Err)`, `Wrote(Err)`, `handle_wrote(0)`,
`handle_stream_chunk(Timeout|Full|Closed)`, peer EOF, and header
deadline. It also defensively cancels in `begin_close()`. Duplicate
cancels are harmless — the source either already stopped or drops the
late message.

`body_io_error_count` still increments on truncation; cancel is an
additional typed signal, not a replacement for the metric. Integration
tests prove both known-length and chunked paths.

Added a cancellation truth table to
`docs/tina-user-guide/14-lifecycle-and-shutdown.md` that names what
cancel means on every surface Tina exposes.

### SQLx/Postgres bridge

- Added `PgPressureReport` and `PgMetricsHandle::pressure_report` so
  callers can observe the bridge as a pool: `capacity`, `leased`,
  `available`, `waiters`, `full_count`, `timeout_count`, `high_water`.
- Added admission tests proving distinct outcomes at each boundary:
  `PgError::Full` (Tina admission), `PgError::PoolAcquireTimeout`
  (SQLx pool pressure), `PgError::Timeout` (bridge deadline), and
  SQL errors (lane stays held until completion).

Added `tina-sqlx-bridge`, a bounded Postgres bridge around
`sqlx::PgPool`. The bridge owns the Tokio/SQLx side; Tina callers see
typed `Full`, `Closed`, per-attempt timeout, pool-acquire timeout,
SQLx/decode errors, and worker-terminal metrics.

The public first form now includes:
- `PgWorker`, `PgConfig`, `PgPoolConfig`, `InstalledPgBridge`,
  `PgCloser`, `PgMsg`, `PgRequest`, `PgResponse`, `PgValue`, `PgRow`,
  `PgError`, `PgMetricsHandle`.
- `Execute`, `FetchOne`, `FetchMany`, and `Transaction` request shapes
  with bounded row caps.
- typed helper calls (`execute_call`, `fetch_one_call`,
  `fetch_many_call`, `transaction_call`) and `PgOutcomeExt::classify`.
- opt-in DB-side cancel-on-timeout via a sidecar pool and
  `pg_cancel_backend`, documented as best-effort rather than guaranteed
  query death.
- wider value support for bool, integers, floats, text, bytea, UUID,
  JSON/JSONB, NUMERIC, DATE, TIMESTAMP, and TIMESTAMPTZ, including
  typed NULL helpers so NULL does not silently infer the wrong type.

The Postgres counter specimen now drives the bridge with host
`ThreadedRuntime::call_blocking`, keeping the example as a SQL script
instead of a fake Driver isolate.

### SQLite bridge

Added `tina-sqlite-bridge`, a serial SQLite bridge around one
`rusqlite::Connection` on one blocking std thread. The first form is
deliberately small: one connection, `max_in_flight = 1`,
`pending_reply_capacity = 1`, autocommit only, explicit pragmas and
busy timeout, bounded buffered rows, late-result truth, metrics, and
typed errors for admission, timeout, response cap, busy/constraint/I/O,
SQLite, and internal faults.

Polish added `execute_call` / `query_call`, row/value accessors, common
Rust value conversions without silent `u64` wrapping, and a classifier
for caller-owned retry. `specimen_sqlite_counter` now uses the bridge
and the host `call_blocking` path; demo modes cover constraint,
timeout, closed, invalid, and retry surfaces.

- Added `SqlitePressureReport` and `SqliteMetricsHandle::pressure_report`
  so callers can observe the bridge as a pool: `capacity`, `leased`,
  `available`, `waiters`, `full_count`, `busy_count`, `high_water`.
- Added admission test proving the serial pool shape:
  `pressure_report_reflects_serial_pool_shape`.

### HTTP body streaming and backpressure

Native HTTP/1 now has production-shaped server response streaming:
`BodyMetrics`, `BodyPressureReport`, `IterBodySource`, typed
`HttpResponse::stream_known_length`, typed
`HttpResponse::stream_chunked`, and response-side chunked
transfer-encoding. The connection isolate pulls chunks on demand and
tracks high-water / body I/O error counters instead of buffering the
whole response.

The body-streaming specimen demonstrates the blessed shape:
`IterBodySource` plus `stream_known_length` for fixed length, and a
chunked route for unknown length. Deferred work is now explicit:
request-side chunked bodies, client-side chunked response decoding,
source cancellation on abandoned wire, and periodic metric emission.

### Native HTTPS first form

Native HTTP can now run over rustls-backed TLS through Tina-owned rails:
`HttpsListener`, typed startup (`HttpsReady` /
`HttpsStartupError`), `HttpTarget::Https`, `TlsTrustRoots`,
`HttpHostPolicy`, and transport-shaped client errors that preserve TLS
name/cert/handshake/I/O truth. Tests cover listener startup failures,
client Host/SNI/default-host behavior, untrusted roots, pool-over-HTTPS,
and simulator TLS replay. `specimen_native_https` compares a Tina HTTPS
listener with a Tokio + tokio-rustls version over the same scripted
client.

### Capacity modeling first form

Added Tina capacity vocabulary:
`tina::capacity::{CapacityMode, CapacityPolicy, CapacitySurfaceReport}`
and `tina_runtime::capacity::{CapacitySummary, SurfaceAssertion,
format_discovery_line}`. Worker-pool waiters and pending replies can
emit capacity reports with high-water and full counts, and docs now
show the tuning loop: unknown -> measured -> fixed. This is count-first
and policy-first; weighted capacity, shared shard-local budgets, and
explicit unbounded-for-now modes remain future work.

### Deadline and PendingCallSet

Added `Deadline` with explicit runtime/simulator clock truth:
`Context::now()`, `Context::deadline_after(after)`,
`Deadline::from_instant(now, after)`, and
`remaining_or_zero(ctx.now())`. There is no ambient
`Deadline::after()` shortcut, so simulator/replay code does not inherit
hidden live-clock behavior.

Added bounded `PendingCallSet<K, R>` for caller-owned `CallHandle`
storage. It uses fixed-capacity storage, rejects duplicate keys, and
keeps cleanup explicit (`remove`, `drain`, `sweep_terminal`) so it does
not hide ABA bugs. Cancellation, pool-cancel, and backpressure specimens
now use the shape.

### Bounded pool vocabulary

Added the first Tina pool primitive and vocabulary: `WorkerPool`,
`PoolLease`, `AcquireOutcome`, `ReleaseOutcome`, `CloseMode::{Drain,
Force}`, `ReleaseDisposition::{Reuse, Retire}`, pool pressure reports,
FIFO waiters, stale lease detection, and typed acquire/release helper
effects. Force close retires outstanding leases; late release is typed,
not silent. Follow-on work turned HTTP keepalive into the first real
consumer.

### Service bootstrap and fanout ergonomics

Round-4 specimen follow-up shipped the boring helper wins and recorded
design notes for the dangerous ones. Landed pieces include
self-address-at-registration for single-shard runtimes, bounded
pending-reply drain helpers, visible-input scatter/gather conventions,
and specimen cleanups using `stop_with(report)` / `observe_result`
instead of host side-channels. The design notes deliberately left
pipeline sugar, generic scatter/gather, flat reqwest continuation
sugar, work-settled helpers, and cross-isolate paired registration
unshipped until a real caller proves the shape.

### Specimen round 2 ergonomics

Round-2 specimen work produced and applied the low-risk helpers:
`ThreadedMultiShardRuntime::observe_result`, `HostBurstOutcomes` and
`try_send_outcome`, `send_observed_until`, `SingleCallGate`, and
`ReqwestOutcomeExt::classify`. Sharded, rate-limited, and retrying
outbound HTTP specimens were migrated so the helpers are the copied
shape. Self-address multi-shard parity, generic scatter/gather, and
reqwest flat continuation sugar remain evidence-gated.

### Server-side HTTP/1.1 keep-alive

`tina_http::HttpListener` / `HttpConnection` can now serve multiple
sequential requests on one TCP/TLS stream. Opt in with
`HttpLimits::keepalive_idle_timeout = Some(d)`; `None` keeps the
legacy one-request-per-connection behavior.

What stays explicit:
- HTTP/1.0 default close, explicit `Connection: close`, parse errors,
  service-call errors, and short known-length streaming responses all
  force close after the response.
- Between requests, the connection waits up to
  `keepalive_idle_timeout` for the next request head. Stale head
  deadlines are generation-tagged and ignored.
- No pipelining: per-request reset drops read-ahead bytes between
  iterations.

Nine integration tests in `tina-http/tests/server_keepalive.rs` cover
sequential reuse, close intent, idle timeout, default one-shot behavior,
slow-loris timeout on a later request, per-request body caps, and mixed
GET/POST traffic on one socket. The final test drives the native
listener through `tina_http::build_keepalive_pool` and asserts one
server `TcpAccept` across the whole script. `specimen_outbound_http`
now uses the same pooled keepalive client shape against a
keepalive-enabled Tina listener.

### HTTP/1.1 keepalive pool

`tina-http` now ships a real keepalive pool consumer of the
`WorkerPool` vocabulary. One TCP (or TLS) connection serves many
sequential requests; the pool exposes acquire / release / retire /
close and a pressure report.

What got shorter:
- An integration-test client that issues three requests against the
  same origin shares one TCP accept on the server side. (The
  user-facing wins of keepalive — fewer handshakes per workload —
  apply when there is a keepalive-capable server to talk to. A
  user-facing specimen is gated on the planned server-side keepalive
  work.)

What stayed explicit:
- Origin keying. `OriginKey` carries scheme + `SocketAddr` + (HTTPS:
  SNI server name + the DER trust roots stored verbatim — no
  fingerprint hash, no collision risk). A pool's connections are
  pre-bound to one `HttpTarget` at registration so cross-origin
  reuse is impossible at the connection-isolate level: the only
  way to reach a different origin is to register a different pool.
- Caps. `PoolConfig::new(capacity, max_waiters)` sizes resources
  and parked callers; the pool isolate's mailbox size is a separate
  bound (size to `>= max_waiters + burst`). The connection isolate's
  mailbox sizes only its own continuations.
- Reuse vs retire. The connection's `KeepaliveOutcome::Request`
  carries `must_retire`. Set when the server returned `Connection:
  close`, the peer EOF'd, or any connect/write/read error fired.
  **Recommended consumer pattern: always release `Reuse`.** The
  connection isolate self-heals — it has already dropped the bad
  transport and will reconnect on the next request. Releasing
  `Retire` permanently removes the slot and drains pool capacity if
  done on every must_retire; reserve it for the rare "the
  connection isolate itself is suspect" case (panic, observed
  protocol violation).
- Deadlines. `KeepaliveConnectionMsg::Request { request_timeout }`
  is the wall-clock budget enforced inside the connection via a
  generation-tagged `sleep`. Pass `Deadline::remaining_or_zero(now)`
  from a caller-owned deadline to propagate budgets honestly across
  hops. No hidden retry, no hidden queue.
- Close modes. `CloseMode::Drain` blocks new acquires and lets
  outstanding leases return normally; `Force` retires outstanding
  leases — late releases get `PoolClosed`.
- Shutdown. `WorkerPool` does not own connection-isolate lifetime.
  `build_keepalive_pool` returns a `KeepalivePoolHandles` bundle
  with the pool address *and* the connection addresses; send
  `KeepaliveConnectionMsg::Stop` to each connection after pool
  close so the underlying transports are closed and the isolates
  exit. Without this, transports leak past pool close.

New surface in `tina_http`:
- `OriginKey`, `KeepaliveConnection`, `KeepaliveConnectionMsg`,
  `KeepaliveOutcome`, `KeepaliveConnAddr`, `KeepalivePoolHandles`,
  `build_keepalive_pool`.
- `encode_keepalive_request` — sibling of the unchanged
  `encode_request` for the keepalive caller (omits the
  `Connection: close` header). The original `encode_request`
  signature is unchanged; this is a pure addition.

Sixteen integration tests live in `tina-http/tests/keepalive_pool.rs`,
plus an HTTPS-keepalive smoke (`keepalive_tls_smoke.rs`) and an
isolate-level DST replay (`dst_keepalive.rs`):

- sequential reuse counted via TCP accept count
- cross-origin isolation (two pools, two accept counts)
- stale server-close → `must_retire = true`, reconnect on next call
- acquire `Full`, acquire-call-timeout & acquire-cancel reclaim of
  waiter capacity
- request timeout marks must_retire; **always-Reuse** keeps pool
  capacity stable; an explicit-Retire test shows the pool removes
  the slot when asked
- Drain blocks new acquires; Drain-with-parked-waiters settles
  waiters as Closed; Force marks outstanding leases stale; Stop
  after Force closes the underlying transport
- HTTPS keepalive shares one TLS handshake across three sequential
  requests
- multi-slot pool (capacity 2) serves two concurrent acquires with
  two TCP accepts; `available` returns to 2 after release
- `OriginKey` distinguishes SNI / trust-root identity
- `request_timeout` correlates with wall-clock duration (proves the
  variable controls the failure)
- `PoolPressureReport` shape across capacity / `Full` / closed
  transitions
- DST replay: two passes against the same scripted TCP config
  produce identical trace fingerprints

The legacy capacity-1 `HttpConnectionPool` is kept as the first
form for one-request-per-connection admission; it remains useful
for callers that do not need keepalive.

### 066A hostile-review fixes

Round of correctness, vocabulary, and proof fixes on top of 066A
following an independent review. The first-form `CallHandle` /
`cancel_call` API surface is unchanged for the common path; the
deltas below tighten classification, close half-work, and add the
plan-mandated proof tests.

**Cross-shard cancel — typed rejection, not silent wrong-result:**
- `CallHandleShared` now stamps `shard_id` on dispatch alongside
  `call_id`. `dispatch_cancel_call` checks the stamp and returns
  the new `CancelOutcome::WrongShard` if the cancel runs on a
  different shard than the originating one.
- `CancelOutcome::NotDispatched` removed — the only reachable path
  to it (cancel batched ahead of its own `call_with_handle` effect
  in one isolate handler) was both rare and a wrong-result hazard
  (cancel returned `NotDispatched` while the call still ran). The
  cancel-dispatch path now `expect`s the `call_id` stamp; user code
  that batches cancel before call panics loudly instead.

**Cause-aware late-reply classification:**
- The recently-cancelled ring now stores `(CallId, CancelCause)`,
  not just `CallId`. Late callee replies for cancelled / timed-out /
  owner-stopped / runtime-stopped calls surface with the matching
  cause-specific rejection reason instead of the generic
  `NoPendingCall` / `CallerClosed`.
- Two new helper enums of variants: `CallReplyRejectedReason` and
  `DeferredReplyRejectedReason` each gain `CallerTimedOut`,
  `OwnerStopped`, `RuntimeStopped`. Stable hashing tags and
  `tina-tracing` string names extended.
- Timeout, owner-stop, and shutdown all record into the ring with
  their respective cause; previously only explicit `cancel_call`
  did, leaving the timeout path classifying late replies as
  generic `NoPendingCall`.

**Runtime shutdown wires `RuntimeStopped`:**
- `cancel_in_flight_calls_for_shutdown` now emits
  `CallCancelled { RuntimeStopped }` for pending isolate calls and
  transitions any caller-held `CallHandle` to `Cancelled` state. A
  user holding a handle while the runtime shuts down sees
  `state() == Cancelled`, not a forever-`Pending` lie.
- `CancelCause::CallerTimedOut` and `CancelCause::RuntimeStopped`
  were dead variants in 066A; now both are emitted by the runtime.

**Owner-stop late-reply distinct from explicit cancel:**
- Owner-stop cleanup now closes captured deferred slots with
  `DeferredReplyRejectedReason::OwnerStopped`. Previously it
  conflated owner-stop with `CallerCancelled`, which broke the plan's
  "trace facts must distinguish" rule (owner-stop and explicit
  cancel must stay distinct in the trace).

**`PressureSummary` extension is additive:**
- New counters `reply_rejected_caller_timed_out`,
  `reply_rejected_owner_stopped`, `reply_rejected_runtime_stopped`.
- `Display` format now appends the new keys after the pre-066
  `path_full=` / `shard_closed=` entries — pre-066 positional
  matchers continue to find their fields at the same offsets.

**API surface tightening:**
- `CallHandleShared::set_call_id`: reverted to a loud
  `assert!`-on-double-stamp instead of the 066A first-stamp-wins
  + `eprintln!`. The two `set_call_id` sites in the runtime are
  mutually exclusive; the path is dead defense and should panic.
- `CallHandleShared::set_shard_id` (new) follows the same shape.
- `CallHandle` `must_use` message clarifies that drop is a no-op
  and the call runs to completion.

**Tests added (Plan Rocks 2/3/5 proof items):**
- `late_reply_after_cancel_surfaces_specifically_as_caller_cancelled`
  (runtime + sim) — asserts the *specific* `CallerCancelled` reason
  and asserts the absence of the generic `CallerClosed` /
  `NoPendingCall`. Catches a regression that removes the cause-aware
  ring without breaking the test (the previous assertion accepted
  either, so the ring could have been silently removed).
- `double_cancel_returns_already_cancelled` (sim) — drives the
  `AlreadyCancelled` outcome through dual handles wrapping the same
  shared cell via `runtime_internal`. User code can't reach this
  with a `!Clone` move-only handle; the runtime can (e.g. owner-stop
  then a queued cancel effect dispatches).
- `late_reply_after_timeout_surfaces_as_caller_timed_out` (sim) —
  pins the new `CallerTimedOut` rejection reason and asserts the
  trace does NOT bleed `CallerCancelled` into a timed-out scenario.
- `owner_stop_cancels_pending_handles_and_classifies_late_replies`
  (sim) — full Rock 5 proof: 3 pending calls, owner stops, each
  handle transitions to `Cancelled`, trace records 3 ×
  `CallCancelled { OwnerStopped }` plus 3 ×
  `DeferredReplyRejected { OwnerStopped }`, no late reply reaches
  the owner's translator.
- `cancel_admit_cycle_does_not_leak_capacity` (sim) — Rock 3 proof:
  32 cancel/admit cycles produce 32 dispatches and 32
  cancellations, no `Full` rejection appears.
- `cross_shard_cancel_is_rejected_with_wrong_shard` (sim) — H1
  regression test: a handle stamped with a foreign shard id returns
  `CancelOutcome::WrongShard`, not silent `AlreadyCompleted`.

**Existing tests rewritten:**
- The 066A `late_reply_after_cancel_is_visibly_rejected` test
  accepted either `NoPendingCall` *or* `CallerCancelled`, which let
  the cause-aware ring be silently removed. Replaced with a
  specific-reason assertion.
- The 066A `double_cancel_returns_already_cancelled` test punted on
  exercising the `AlreadyCancelled` outcome with a comment about
  move-only handles. Now actually tests it.
- Three `tests/deferred.rs` and one `tests/consumer_api.rs` test
  updated to assert the new `CallerTimedOut` rejection reason
  instead of the old `CallerClosed`.

**Tokio side of `specimen_cancellation_chain` invariant relaxed:**
- 066A asserted `replies_after_cancel == 0` strictly; that holds for
  the current worker shape (`sleep().await; i`) but tripwires any
  refactor that adds an await between sleep and the reply path.
  Now asserts the loose `replies_before + after <= FANOUT`.
- Tina side of the example now polls the trace until rejection
  count converges before snapshotting, with a 2s safety budget.
  Removes the slow-CI flake where workers' late replies hadn't
  arrived by the time `result.wait` returned.

### Cancellation: first-form `CallHandle` and `cancel_call`

**Breaking trace surface changes** (downstream consumers — lint
your matches):
- `PressureSummary::Display` format adds a `cancelled=N` field. Log
  scrapers matching the old `reply[no_pending=N path_full=N
  shard_closed=N]` shape need updating.
- Stopping an isolate with pending calls used to settle as
  `CallCompletionRejected { RequesterClosed }` when the worker's late
  reply arrived. It now emits `CallCancelled { OwnerStopped }` at the
  moment of stop, and the worker's late reply hits
  `CallReplyRejected { NoPendingCall }` (or
  `DeferredReplyRejected { CallerCancelled }` for captured slots).
  Trace assertions on the old shape fail loudly.

API additions:

- `tina::CallHandle<R>` (move-only, `!Clone`, `#[must_use]`) plus
  `tina_runtime::call_with_handle(addr, msg, t).reply(...)` returning
  `(Effect, CallHandle<R>)`.
- `tina_runtime::cancel_call(handle).reply(...)` closes the wait,
  reclaims caller-side capacity, and emits
  `RuntimeEventKind::CallCancelled { call_id, cause }`. Late callee
  replies surface as `CallReplyRejected { NoPendingCall }` or
  `DeferredReplyRejected { CallerCancelled }` — visible truth, not
  silent loss.
- `CancelOutcome` (`Cancelled` / `AlreadyCompleted` / `AlreadyCancelled`
  / `NotDispatched`) is `#[must_use]`. `CancelCause` distinguishes
  `CallerCancelled` / `CallerTimedOut` / `OwnerStopped` /
  `RuntimeStopped`.
- Stopping an isolate with pending calls now proactively cancels them
  with `CallCancelled { OwnerStopped }` instead of waiting for late
  replies to bounce as `CallCompletionRejected { RequesterClosed }`.
- New trace shapes: `CallKind::CancelCall`,
  `CallReplyRejectedReason::CallerCancelled`,
  `DeferredReplyRejectedReason::CallerCancelled`. Stable hashing,
  `tina-tracing` event names, and `PressureSummary`
  (`reply_rejected_caller_cancelled`) all extended.
- `PressureSummary::Display` format gained a `cancelled=N` field —
  log scrapers that match the old shape need updating.
- `examples/specimen_cancellation_chain` rewritten to use the new
  shape; the host now counts rejected late replies from the trace
  and the `Report` reflects that truth instead of hardcoding zero.
- Simulator parity: `tina-sim` mirrors the dispatch and owner-stop
  paths; `tina-sim/tests/cancel_call.rs` pins the public behavior
  deterministically.
- Bounded `PendingCallSet` (Rock 4) and `Deadline` value (Rock 1)
  deferred to follow-up phases; design notes recorded in
  `.intent/phases/066-cancellation-and-deadline-model/`.

### DST As A First-Class Dev Mode

- Added a "bug in a box" replay-case shape in `tina_sim::dst`:
  `ReplayCase`, `ReplayReport`, `ReplayConfig`, plus `assert_replay_case`
  / `check_replay_case` / `ReplayMismatch`. Cases pin name, seed, full
  `SimulatorConfig`, declared mailbox capacities, scenario, history,
  expected event count, and `stable_trace_hash`. Failures name the
  next decision, not just numbers, and include the case history so a
  coding agent reading only the panic can see what the case did.
- `ReplayConfig` carries the full `SimulatorConfig` (seed overridden by
  `case.seed` at run time) and a `BTreeMap<&'static str, usize>` of
  per-isolate mailbox capacities. `ReplayConfig::mailbox(role)` panics
  loudly on a missing role so the runner cannot quietly inherit a
  literal. `check_replay_case` debug-asserts that `case.name`/
  `case.seed` match `case.history` so the two source-of-truth fields
  cannot drift.
- Added `sweep_seeds` for hand-cranked deterministic seed search.
  Failures return a `SweepFailure` whose `failing_case` has refreshed
  expected count/hash and is ready for `assert_replay_case`. All
  helpers accept `FnMut` runners so stateful runners are allowed.
- Added `shrink_replay_case` plus `ShrinkReport`. The shrunk case
  preserves name/seed/config/scenario/invariant, refreshes its
  expected count/hash, and prints a pasteable Display form with a
  review step.
- Rewrote `docs/tina-user-guide/08-simulation-and-dst.md` around the
  workflow (history → run twice → sweep → save bad seed → shrink →
  commit) with a copyable test skeleton, bug-report shape, and an
  explicit note that `Display` output is pasteable as readable lines
  for bug reports while the `case()` function itself is what to copy
  for code.
- Upgraded `examples/specimen_replay_dst` to be the copyable specimen:
  one saved `ReplayCase` with `Tick`/`Drain` ops where every op is
  load-bearing (deleting one changes the trace hash), declared
  mailbox capacities, runner, sweep demo, shrink demo, and
  same-seed/different-seed regression tests.
- Added `tina-sim/tests/saved_replay_cases.rs` with one
  service-shaped saved case (`burst overflow under local-send delay`)
  that pins a real `SendRejected{ reason: Full }` mailbox-pressure
  fact with exact `full_rejections` / `accepted_sends` counts
  alongside its event count and trace hash.
- Migrated the existing `remote_full_burst_is_known_edge_contract_and_replays`
  case in `tina-sim/tests/timmerhus_dst.rs` from `assert_replays` +
  bare `History` to `assert_replay_case` + `ReplayCase` so the new
  shape is the way for new DST tests, not just an alternative
  parallel API. Followed up by migrating
  `live_sim_projection_matches_topology_failure_history` and the four
  saved-seed cases in `tina-sim/tests/portable_service_dst.rs`
  (`portable_service_dst_replays_whole_service_history`,
  `portable_service_dst_replays_observed_send_full_before_persistence`,
  `baobab_dst_replays_observed_send_persistence_and_requester_stop`,
  `baobab_dst_replays_pressure_shard_failure_and_topology_truth`),
  each now pinning event count + `stable_trace_hash` alongside the
  service-projection invariants. The shrink tests still use
  `delete_shrink` + bare `History` because they exercise shrink
  itself, not pin a saved trace shape. Random-history tests in
  `dst_randomized.rs` and harness-exercising tests in
  `dst_harness.rs` stay on `assert_replays` for the same reason.
- Added ergonomic helpers driven by real-use friction:
  `ReplayCase::new(...).expecting(count, hash)` builds the case and
  the bundled `History` from one set of name+seed inputs;
  `ReplayCase::simulator_config()` returns a `SimulatorConfig` with
  `case.seed` already set so the runner is one line;
  `ReplayConfig::with_faults(...)` and `with_mailbox(role, cap)`
  replace nested struct literals; `observe_replay_case` plus
  `ReplayReport::pinned_constants()` are the blessed discovery path
  for first constants; `discover_constants` runs a batch of cases
  sharing the same `Op`/runner and prints one pasteable
  comment-headed block per case so adding or perturbing several
  related cases at once is a single `cargo test --ignored` away;
  the user guide gains a "Pick Your Op Alphabet" section that names
  the mental move.

### Phase 065 Observability First Form

- Added `tina-tracing`: an adapter crate that turns `RuntimeEvent`s and
  `LiveTopologyReport`s into structured `tracing::Event`s under stable
  targets (`tina_runtime::trace`, `tina_runtime::live`) without flattening
  typed reasons (`Full`, `Closed`, `CallerClosed`, `ReplyPathFull`,
  `MailboxFull`, `BudgetExceeded`, …).
- Added a live `TraceObserver` hook on `tina-runtime` and `tina-sim`: one
  synchronous in-line callback fired before retention, wired through
  `ThreadedRuntime`, `ThreadedMultiShardRuntime`, `LocalSystem*Builder`,
  and the `Simulator` setter. `ThreadedRuntimeConfig` stays pure data
  (`Copy + Eq`); the observer is a separate constructor parameter.
- Added byte-identical-trace property tests on both runtime and simulator
  proving a noop observer leaves the deterministic event record
  unchanged. `TraceRetention::Off` plus an observer is the new
  stream-only mode.
- Added new bridge tracing emission for `tina-sqlite-bridge`,
  `tina-tokio-bridge`, `tina-tower-bridge`, and `tina-reqwest-bridge`
  behind each crate's optional `tracing` Cargo feature. Targets follow
  the `tina_<bridge>.bridge[.call]` shape; shared `reason` strings
  (`Full`, `Closed`, `Timeout`) reuse runtime vocabulary where the
  concept matches; bridge-specific fields stay bridge-shaped
  (`request_kind`, `method`, `status`, `outcome`, `rows_changed`,
  `row_count`, `elapsed_ms`, `scope`, `detail`).
- `tina-rpc-tokio`'s pre-existing spans (`tina_rpc.bridge.call` with
  `service`, `method`, `correlator`, `result_kind`) were left
  untouched in this phase and remain on the previous shape.
  Documented in `19-tracing.md` as the one bridge whose vocabulary
  has not been harmonised yet.
- Added `docs/tina-user-guide/19-tracing.md` with the field/level/reason
  vocabulary table for runtime and bridge events; one runnable example
  (`specimen_tracing_demo`) wires the live observer end-to-end.
- Left OpenTelemetry / Prometheus mappers, span timing for runtime
  calls, cross-bridge correlator alignment, and `tina-rpc-tokio`
  vocabulary harmonisation as future observability work.

### Phase 055 Codebase Module Split

- Split the giant runtime, driver, simulator, and support files into smaller
  modules without changing public behavior, trace vocabulary, or API shape.
- Preserved the existing test/verify surface while making future feature work
  easier to review file-by-file instead of re-reading one massive module.
- Kept the split intentionally boring: module extraction only, no semantic
  cleanup mixed into the move.

### Phase 051 Ecosystem Bridge Adapters

- Completed the first bridge tranche around Tina's bounded core:
  `tina-rpc-tokio`, `tina-tower-bridge`, and `tina-reqwest-bridge`, with
  `tina-tokio-bridge` kept as the generic host/lifecycle foundation.
- Added bridge docs and specimens that name the two-runtime shape,
  preserve `Full`/`Closed`/`Timeout` outcomes, keep deadlines and ingress caps
  explicit, and document weakened DST/replay guarantees at the Tokio boundary.
- Added bridge ergonomics polish: friendlier Tower service aliases, Tower
  `Service` re-export, reqwest `install`, `send_request`, layered
  `ReqwestCallOutcome`, opt-in `flatten_outcome`, and
  `ReqwestOutcomeExt::classify` for caller-owned retry loops.
- Left SQLx/Postgres, AWS SDK, smol, common bridge setup extraction, and bridge
  crate folder layout as future bridge/database work.

### Phase 061 Bounded Deferred Replies and Service Fanout

- Added typed deferred reply capture so a service can accept a call, hold the
  caller reply slot across later messages, and answer after fanout or worker
  completion without hiding the pending capacity.
- Added bounded pending-reply accounting, explicit full/closed/rejected trace
  outcomes, and runtime/simulator proofs for slot capture, reply, drop,
  caller-close, requester-shard close, stop cleanup, and panic cleanup.
- Tightened the public API so deferred reply capture derives its reply type
  from the running isolate context, with a runtime type guard retained at the
  erased boundary.
- Added Specimen scatter/fanout examples that use deferred replies as ordinary
  Tina state instead of side-channel host polling.

### Phase 059 Specimen Ergonomics Harvest

- Added typed isolate result waiters through `stop_with(...)` and
  `observe_result(...)`, retiring several `Arc<Mutex<_>>`/atomic host
  side-channel patterns in specimens.
- Added per-call reply aliases and first-form TCP loop helpers so common TCP
  continuation enums read closer to the runtime call that produced them.
- Added capacity diagnostics and pressure-report conventions for examples and
  tests, including reusable mailbox budget naming.
- Added small HTTP router ergonomics, stateful-router support, and bridge
  specimen structure cleanup so examples show the Tina-shaped code first.

### Phase 058 Tina RPC Usability Layer

- Added a typed Tina RPC service layer on top of the framed-call seed:
  service-handler topology notes, generated service dispatch, typed client
  stubs, and a `#[tina_rpc::service]` authoring surface.
- Added `tina-rpc-tokio` so Tokio callers can await Tina RPC calls through a
  bounded bridge without pretending cancellation, full, or timeout disappear.
- Added RPC usability tests and Specimen typed-RPC notes that keep capacity,
  serialization limits, local-vs-wire outcomes, and retry policy explicit.

### Phase 053 Sharded Service Primitives

- Added sharded placement primitives with stable key ownership over an
  explicit ordered shard list, visible placement reports, and owner-side
  wrong-shard validation.
- Added first-form sharded table/counter patterns, service-table helpers,
  reply adapters, bounded scatter/gather vocabulary, partial aggregate
  outcomes, and hot-key pressure reporting.
- Added live and simulator/DST proofs for placement determinism, wrong-shard
  rejection, closed targets, aggregate timeout, and bounded collector pressure.

### Phase 052 Tina Framed Calls First Form

- Added a Tina-native framed request/reply probe with length-prefixed TCP
  frames, service/method names, request ids, bounded in-flight calls, typed
  full/closed/timeout/error outcomes, client state machine, and registry.
- Added simulator and live proofs for framed-call behavior, including visible
  overload and close/cancel semantics.
- Added Specimen RPC comparison coverage so the first-form RPC surface is tested
  as code people actually read, not only as crate internals.

### Phase 048 Native HTTP Service Stack

- Added Tina-owned HTTP/1.1 first-form support: parser/framing, request and
  response types, connection/listener isolates, bounded limits, visible
  overload, graceful close paths, and small routing helpers.
- Added native HTTP client and bounded pool first forms, plus examples showing
  when Tina can own HTTP directly instead of using Tokio/Axum as the edge.
- Added parser-level DST and documented the remaining larger slices:
  production streaming bodies and full listener/connection simulator replay.

### Phase 047 Specimen Ergonomics Harvest

- Added the first Specimen comparison suite discipline and harvested its obvious
  Tina papercuts into primitives instead of leaving them as example folklore.
- Added bounded observation handles, stable trace/fingerprint support, easier
  single-shard defaults, mailbox/reply-slot sizing guidance, sequenced-call/TCP
  helper docs, bridge lifecycle cleanup, and runtime surface alignment.
- Updated Specimen findings/READMEs so resolved pain moved out of the active
  complaint list and remaining pain became future work.

### Runtime: TCP/UDP close cancels pending lanes instead of failing with `ResourceBusy`

- `tcp_close_stream`, `tcp_close_listener`, and `udp_close_socket` no
  longer fail with `CallError::ResourceBusy` when a read/write/accept/
  recv is pending. Close cancels the pending op and closes the
  resource. The pending caller's continuation never fires (silent
  cancel — same shape as isolate-stop with pending calls).
- New `CallCompletionRejectedReason::ResourceClosed` trace variant
  keeps each silent cancellation observable.
- Live driver pushes cancelled call ids onto `cancelled_by_close`;
  the runtime layer drains them via the new
  `RuntimeDriver::take_cancelled_by_close` hook and drops matching
  `in_flight_calls` plus translators. Without this the worker would
  spin on ghost calls.
- Simulator gets a matching `cancel_backend_calls_for_resource`
  helper that drains its pending queues, in-flight calls, and
  translators. `run_until_quiescent` no longer hangs after
  close-while-pending.
- Tests previously pinning `ResourceBusy` for close-while-pending now
  assert the clean-cancel-and-close behavior. `examples/FINDINGS.md`
  is updated to mark the issue fixed.

### README Rewrite and Forward Roadmap Phases (Native DB / HTTP/2 / gRPC)

- Rewrote the project `README.md` to match the conventions of mature
  framework READMEs (Tokio, Mbanugo's Tina/Odin, Seastar): descriptive
  lead, property bullets, one canonical TCP-echo example, an
  architecture section with ASCII diagram and crate table, a
  deterministic-simulation section as one section among several, a
  quickstart, a documentation table, an honest status/limits paragraph,
  and a prior-art table that names the Rust neighbors (madsim, turmoil,
  ambitious, joerl, lunatic, glommio, monoio, loom, shuttle).
- Added forward-roadmap phases 055 (native database, Postgres via
  `postgres-protocol` plus SQLite via `rusqlite`), 056 (native HTTP/2),
  and 057 (native gRPC), with an "adopt-don't-rebuild" discipline note
  naming the sync codec crates Tina borrows (`httparse`,
  `postgres-protocol`, `rusqlite`, `hpack`, `prost`, `rustls`,
  `tungstenite`).

### Phase 048a Native HTTP Service Stack — Server First Form

- Added a new workspace crate `tina-http` containing the HTTP/1.1
  server first form: `parse` module wrapping `httparse` with typed
  `RequestParseError` variants (`BadRequestLine`, `HeadersTooLarge`,
  `UnsupportedTransferEncoding`, `InvalidContentLength`, `BodyTooLarge`,
  `UnsupportedRequestTarget`, `UnsupportedHttpVersion`,
  `HeaderReadTimeout`); `connection` module hosting an
  `HttpConnection<S: Shard>` isolate that reads, parses, accumulates a
  `Content-Length` body, calls a service isolate via `tina_runtime::call`,
  writes the response, and closes; `listener` module hosting an
  `HttpListener<S: Shard>` isolate that binds, accepts, and spawns one
  connection per accept; `types` module with `HttpRequest`,
  `HttpResponse`, and `HttpLimits` (including `header_read_timeout`).
- Pinned the connection isolate's `CallOutcome` -> HTTP status mapping
  with an exhaustive match on every `CallError` variant: `TargetFull`
  -> 503, `Timeout` -> 504, `TargetClosed` -> 500, every other variant
  -> 500. Adding a new `CallError` in `tina-runtime` is now a compile
  error in `tina-http`.
- Added a slow-loris guard: an in-flight `sleep(header_read_timeout)`
  fires concurrently with the head-read; if parsing has not completed
  by the deadline, the connection isolate stops and the runtime drops
  the stream. Documented the runtime's `tcp_close_stream`-while-read-
  pending limitation in `examples/FINDINGS.md`.
- Fixed the listener `Stop` race: a queued `Accepted(Ok)` arriving
  after `Stop` took the listener now closes the orphan stream instead
  of panicking on `self.listener.expect(...)`. Removed the dead
  `build_close_child` helper.
- Fixed RFC 7230 §6.1 parsing: `Connection: close, keep-alive` (in any
  order) now correctly reports `connection_close = true`. Fixed RFC
  7230 §3.3.2 parsing: conflicting `Content-Length` values now map to
  `400 Bad Request` (was incorrectly `411 Length Required`).
- Switched the parser's per-call header buffer to a stack-allocated
  array for the common case (`max_headers <= 64`), with a heap fallback
  for larger configurations.
- Switched the connection isolate's partial-write loop to a
  `drain(..count)` + `clone()` pattern (matching `tcp_echo.rs`) instead
  of slicing-with-offset, bounding total response-write copies to
  O(N) for an N-byte response.
- Added an `specimen_native_http` paired Tokio-vs-Tina comparison: axum
  on the Tokio side, `tina-http` on the Tina side, identical scripted
  client, asserts byte-equivalent outcomes. The Tina HTTP server runs
  with no Tokio runtime in the process — first Specimen comparison where
  Tina speaks the wire protocol itself.
- Added the 048 plan's "Slices", "User-Facing Shape (First Form)",
  "Crate Placement", and "Coordination With 047" sections, plus an
  honest split of rock 5 into 5a (typed-mapping overload visibility,
  shipped in 048a) and 5b (admission limits + metrics + wire-level
  Full coverage, deferred to 048b alongside the connection pool).
- Filed three new entries in `examples/FINDINGS.md` capturing real
  pain surfaced by 048a: the `#[tina_runtime::isolate(shard = S)]`
  macro does not accept a generic shard parameter (forces hand-rolled
  `Isolate` impls); `tcp_close_stream` rejects with `ResourceBusy`
  while a `tcp_read` is pending on the same lane (no
  `tcp_cancel_read` primitive exists, blocking the slow-loris path's
  ability to write 408 before close); wire-level `CallOutcome::Full`
  is not deterministically constructible on a single shard with the
  current API.
- 41 tests in `tina-http`: 25 unit (parser determinism, parser
  edge-cases, response encoder, exhaustive `CallError` mapping); 4 DST
  parser-replay (parser purity, corpus fingerprint stability,
  fingerprint sensitivity to limit changes, error->status mapping
  fingerprint); 6 bad-input integration (malformed line, oversized
  headers, chunked transfer encoding, oversized Content-Length,
  absolute-form target, peer close mid-request — all with follow-up
  request assertion to prove listener uncorrupted); 5 pressure
  integration (multi-read body with trace assertion, graceful
  shutdown, slow-loris timeout via deadline trace event, stop-race
  regression, wire-level 504 via a service that never replies); 1
  happy-path smoke. Plus the paired `specimen_native_http` comparison.

### Phase Baobab Production-Readiness Rails

- Added an executable readiness matrix in `tina-runtime/tests/readiness_matrix.rs`
  covering runtime-owned rails, bridge ingress, replay/DST, affinity, cost
  reporting, cancellation, backpressure, `io_uring` non-claim, and
  platform-gated Glommio comparison rows.
- Extended the canonical portable service harness with a Baobab user-service
  gauntlet that composes a TCP listener/session, Tina-owned timer, DNS, bounded
  process execution, runtime-owned file I/O, journal append, cross-shard
  isolate call, and terminal shutdown/report checks through `LocalSystem`.
- Added a live multi-shard Baobab service proof: one worker shard fails, sibling
  persisted work still completes, and calls into the failed shard surface typed
  closed/failure truth.
- Expanded portable service DST with saved-seed histories for observed-send +
  persistence + requester stop, pressure + shard failure + topology truth, and
  deletion shrinking over a requester-stop history.
- Added Baobab DST histories for persistence restart/corrupt/truncated recovery
  and bridge timeout/retry/shutdown behavior.
- Upgraded `make portable-runtime-cost` from shape-only rows to local timing
  rows over real Tina smoke paths for local send, live ingress, cross-shard
  send, isolate call, plus local TCP loopback, while keeping unmeasured
  TLS/bridge rows explicit and labeled "not benchmark".
- Hardened the Baobab TCP service proof and cost smoke so both use framed or
  accumulated TCP reads instead of assuming one read is one request.
- Folded the Baobab readiness gate into the single `make verify` command:
  readiness matrix, portable service, LocalSystem rail/backpressure e2e tests,
  service DST, bridge cancellation model/e2e, and cost smoke.

### Phase Portable Local Runtime Completion

- Added a canonical public-path portable service harness using
  `LocalMultiShardSystem`: configure budgets, register router/workers, route
  by key to shard-owned workers, perform journal append before reply, shut down,
  assert terminal topology/report truth, and replay durable journals.
- Fixed isolate-call continuation semantics in both the live runtime and
  simulator: runtime-owned call completions and observed-send completions now
  preserve the original isolate-call context, so a service can receive a call,
  do runtime-owned I/O/persistence, and reply afterward.
- Added direct live and DST proofs for observed-send continuation: accepted
  audit send outcomes can drive the original call reply, accepted audit sends
  eventually run the target side effect, and full audit sends return a typed
  failure without mutating the audit target or losing the caller reply.
- Added visible placement/backpressure proofs: wrong key-to-shard routing
  rejects before work runs, unknown shard registration returns
  `ThreadedRuntimeError::UnknownShard`, and busy retry uses a Tina-owned timer
  before returning a typed rejection.
- Completed the user-facing budget manifest path with builder knobs for DNS,
  TLS, process, signal, and shutdown drain timeout, plus terminal topology
  assertions that the configured shape survives shutdown reporting.
- Added service-level DST in `tina-sim`: saved-seed whole-service histories
  over cross-shard call, observed-send continuation, journal append, worker
  stop/closed outcomes, observed-send full before persistence, replay equality,
  invariant checks, and deletion shrinking.
- Added `make portable-runtime-cost` as an explicit cost-smoke command and
  folded the service harness, budget manifest, service DST, bridge cancellation
  model, and cost smoke into the project verification path. The cost command is
  labeled "local machine / not benchmark" and makes no speed claim.

### Phase Blue Whale

- Added `AffinityStatus` and shard/core ownership reporting to live topology:
  `LiveShardReport` now exposes worker name, worker thread id, configured
  core, optional observed core, and affinity status. The current portable
  backend reports configured cores as `AdvisoryOnly`; it does not claim hard
  OS pinning.
- Added `configured_core` to `LocalSystemConfig` and
  `ThreadedRuntimeConfig`. Multi-shard local systems treat it as the first
  core in stable shard order and report contiguous advisory core ownership.
- Added `PreallocationConfig` for setup-time runtime-owned metadata reserves:
  isolate entries, child records, supervisors, trace events, in-flight calls,
  translators, isolate-call metadata, driver-completion scratch, and per-step
  round scratch. User payloads, erased reply/message boxes, durable buffers,
  and backend-owned completion slots remain explicit non-claims.
- Added `remote_inbound_drain_budget` so a live destination shard harvests a
  bounded number of remote envelopes before giving local runtime work a turn.
  Cooperative isolate fairness remains one delivery chance per isolate per
  runtime step; Tina still does not preempt a synchronous handler.
- Exposed that fairness budget in `LiveShardReport`, added builder helpers for
  `LocalSystem`, and made low-level `ThreadedRuntime` reject zero budgets
  before starting a worker.
- Tightened fake-driver contract tests with a TCP-ish pending resource path
  that proves pending-call and table-owned resource reporting clear on cancel.
- Added a checked Blue Whale/Seastar principles table as a Rust test covering
  per-core ownership, thread pinning, bounded queues, preallocation, allocator
  locality, backend shape, NUMA, scheduling groups, DST/replay, and Tina's
  non-`await` user model.
- Added combined e2e coverage for advisory core ownership, preallocation
  posture, bounded remote drain budget, and live cross-shard isolate-call
  behavior. `make verify` passes.

### Phase Sadie's Ward

- Added typed worker-held and pending-driver-call accounting alongside the
  existing table-owned count: `LiveShardReport::worker_held_resource_count`
  and `pending_driver_call_count`, plus
  `LocalSystemShutdownReport::remaining_worker_held_resource_count`,
  `remaining_pending_driver_call_count`, and `unclean_reason`.
- Added `ShutdownUncleanReason` (`#[non_exhaustive]`) with a deterministic
  priority order: runtime error > failed shards > not-closed > worker-held
  remaining > pending-call remaining > table-owned remaining.
- Added `shutdown_lane_drain_timeout` to `LocalSystemConfig` and
  `ThreadedRuntimeConfig` (default `DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT`,
  100 ms). Per-shard shutdown drains lane workers up to that budget, then
  returns; stuck work surfaces in the terminal report rather than blocking
  shutdown forever. The Betelgeuse TCP shutdown drain replaces its
  64-step constant with the same deadline.
- Added Unix `SIGINT`/`SIGTERM` capture via `signal-hook` flag handlers
  (no Tokio dependency, no async signal task, no custom unsafe handler);
  flagged through to runtime-owned signal completions parked by
  `signal_wait`. Added `os_signal_capture_supported()` so non-Unix is an
  explicit unsupported capability instead of a silent no-op.
- Added `LiveShardReport::dns_lane`, `tls_lane`, `process_lane`, and
  `signal_lane` so every bounded lane capacity is reachable from the
  topology snapshot.
- Hardened threaded `try_send` (single-shard and multi-shard) so a
  `Failed` shard rejects ingress immediately with `WorkerStopped` instead
  of relying on the bounded sync channel to observe `Disconnected`.
- Changed live `trace()` to return a `TraceSnapshot`: default observation now
  keeps retained events even after a shard failure, while `complete_trace()`
  remains the strict all-shards-or-error path. Terminal reports retain partial
  trace instead of going blind when shutdown/failure is exactly what the user
  needs to inspect.
- Added `shutdown_report()` on the low-level threaded runtime owners so users
  can get terminal error, topology, resource counts, and retained trace
  together instead of losing report shape on driver shutdown failure.
- Added per-lane unit tests for the count rules, a unit test that the
  storage and Betelgeuse TCP shutdowns return inside their budget when a
  worker is stuck, a real `raise(SIGINT)` test that reaches a parked
  `signal_wait`, a live `LocalSystem` test that a failed shard rejects
  ingress while a healthy shard keeps running, low-level tests that retained
  trace survives sibling worker failure, DST combining timeout/remote-full/
  closed-target outcomes, and a live `LocalSystem` topology test that exposes
  every new field through the public API.

### Phase Jan de Quay

- Added native bounded live DNS support behind `dns_lookup`, with visible
  `DnsFull`, `DnsClosed`, timeout, queued cancellation, and tombstoned
  already-started resolver work.
- Added native rustls-backed TLS support behind `tls_connect`, `tls_read`,
  `tls_write`, and `tls_close`, with `TlsStreamId`, one pending operation per
  TLS stream, certificate/name/handshake/I/O/full/closed/timeout outcomes, and
  simulator TLS scripts for semantic replay.
- Added richer runtime-owned path operations: `path_metadata`,
  `rename_replace`, `remove_file`, `read_dir`, and `sync_parent`, with typed
  missing/unsupported/uncertain/I/O outcomes where platform behavior matters.
- Added runtime-owned shutdown notification through the signal rail so a live
  runtime can deliver `"shutdown"` to waiting isolates before the worker stops.
  Raw OS signal capture remains a non-claim.
- Updated `RuntimeCapabilities` so DNS, TLS, path/storage, process, UDP, and
  signal rails report their actual supported/lane-backed/poll-backed/
  completion-backed/tombstoned/drained shapes.
- Expanded `LocalSystem` e2e coverage for DNS, TLS, runtime-owned file/path
  operations, shutdown notification, and composed resource workloads.
- Expanded simulator/DST resource histories over DNS, TLS, path, signal,
  process, UDP, and TCP combinations, with replay and delete-shrink coverage.

### Phase Funkishus

- Added `RuntimeCapabilities` for runtime-owned resource families, including
  support status, execution shape, cancellation shape, shutdown shape, lane
  capacity, and durability support.
- Added runtime-owned UDP helpers: `UdpSocketId`, `udp_bind`,
  `udp_send_to`, `udp_recv_from`, and `udp_close_socket`.
- Implemented live UDP in the Tina driver with nonblocking runtime-owned
  sockets, visible truncation, same-resource receive lane ownership,
  `ResourceBusy` close/duplicate-receive behavior, and requester-stop
  cancellation.
- Added scripted simulator UDP bind/send/recv/close, loopback, truncation,
  receive capacity pressure, completion capacity pressure, and cancellation.
- Added DNS call vocabulary and typed helpers while keeping live DNS honestly
  unsupported on the current substrate. Added scripted simulator DNS success,
  failure, timeout, and bounded-lane full behavior.
- Added bounded local process execution with command-plus-args, null stdin,
  bounded stdout/stderr capture, timeout kill/reap, lane full/closed outcomes,
  and simulator parity for exit/failure/timeout/kill-uncertain paths.
- Added signal wait call vocabulary with simulator-first signal injection,
  timeout/failure/full/cancel behavior, and typed live `Unsupported` without
  installing process-global handlers.
- Kept TLS as an adapter-only capability; native TLS remains an explicit
  non-claim.
- Added a composed live proof where one `LocalSystem` service uses UDP,
  process execution, and journal append before committing durable state.
- Expanded DST over DNS/process/UDP/signal histories with replay, common trace
  invariants, and deletion shrinking.

### Naming Polish Before Funkishus

- Renamed the canonical live owner from `LocalApp` to `LocalSystem`, with
  matching `LocalMultiShardSystem`, `LocalSystemState`,
  `LocalSystemTerminalReport`, `LocalSystemShutdown`, and builder names.
- Renamed the user-facing live threaded runner from
  `BetelgeuseBackedRuntime` to `ThreadedRuntime`, with matching
  `ThreadedMultiShardRuntime`, `ThreadedRuntimeConfig`,
  `ThreadedRuntimeError`, `ThreadedTrySendError`, and
  `ThreadedSendObservedError`.
- Kept Betelgeuse as the named backend/driver implementation detail where the
  code is specifically talking about the completion backend.

### Phase Timmerhus

- Added first-class live topology reporting for the canonical local app path:
  `LiveTopologyReport`, `LiveShardReport`, `LiveQueueReport`,
  `LiveRemoteQueueReport`, and `LiveShardState`.
- Added `LocalSystem::topology()` and `LocalMultiShardSystem::topology()` so users
  can inspect shard ownership, worker names, lifecycle state, ingress capacity,
  remote queue capacity, storage-lane capacity, and honest pressure counters
  without scraping logs.
- Added terminal topology snapshots to `LocalSystemTerminalReport` so graceful
  shutdown and failed worker termination remain visible after the app owner is
  consumed.
- Kept queue pressure honest: exact depth is `None` unless exact by
  construction, measured counters are `Some(_)`, and unmeasured storage-lane
  counters are `None` instead of fake zeros.
- Named live shard worker threads as `tina-shard-{id}` and tracked
  per-shard lifecycle as `Running`, `Stopped`, or `Failed`.
- Added per-shard and source/target remote-queue metrics for
  threaded multi-shard runtimes.
- Added user-shaped live tests for topology before/after shutdown, bounded
  ingress full, bounded remote queue full, and one failed worker while another
  shard continues and then stops cleanly.
- Added Timmerhus DST coverage: a replayable topology/failure history, true
  live-vs-simulator projection comparison, mutation-after-rejection absence,
  common trace invariant checks, and deletion shrinking for the failing
  topology model.
- Hardened Timmerhus tests so normal tests pin known negative/edge contracts
  directly (`Closed` direct ingress after stop, bounded remote `Full`), while
  DST sweeps prove the random histories actually exercise `Full`, `Closed`,
  timer, and panic rocks instead of drifting back to happy paths.

### Phase Stuga

- Added `tina_sim::dst` with reusable `History`, `DstRun`, replay assertion,
  deletion shrinking, shrink reports, trace invariant suite, persistence-image
  replay helper, visible-pressure helper, and semantic projection comparison.
- Refactored randomized single-shard and multi-shard DST tests onto the shared
  harness and added an optional `TINA_DST_LONG=1` long seed sweep.
- Added harness self-tests proving replay equality, deletion shrinking,
  causality failure detection, and accepted settled-send fixtures.
- Added simulator-only `ScriptedStorageFaultConfig` for deterministic
  journal/snapshot failure, truncate, corrupt, and commit-uncertain durable
  image faults.
- Reworked persistence and TCP cancellation matrices into history-shaped DST
  runs using shared replay and invariant checks.
- Reworked bridge ingress model DST to use shared histories and deletion
  shrinking while keeping it explicitly model-only.
- Added live-vs-sim projection comparison helper and used it for oracle,
  simulator, and Betelgeuse runner parity checks.

### Phase Johan Rudolph Thorbecke

- Added a bounded live storage lane for snapshot/journal persistence work so
  persistence helpers no longer execute synchronously inside the shard worker
  on the preferred live path.
- Added `BetelgeuseBackedRuntimeConfig::storage_lane_capacity` plus
  `LocalSystem` single-shard and multi-shard builder knobs for that bounded lane.
- Added `CallError::StorageFull` and `CallError::StorageClosed` as named
  runtime-owned storage admission/lifecycle outcomes.
- Kept direct explicit-step runtime storage inline while using the bounded
  storage lane on live single-shard and multi-shard worker paths.
- Added storage-lane proofs for bounded full rejection without sleep-as-proof,
  cancellation swallowing late completions, and shutdown skipping buffered work
  that never started.
- Added `local_app_end_to_end_service` proof: multi-shard `LocalApp` ingress,
  cross-shard service routing, journal append before state apply, shutdown,
  fresh-app recovery, and recovery trace visibility.
- Added a composed live TCP plus persistence proof where a `LocalApp` service
  accepts a real TCP client, journals the payload before replying, shuts down,
  and replays the durable journal.
- Added `LocalAppTerminalReport::summary()` as trace-derived terminal
  accounting for completed, failed, rejected, abandoned, journaled, and
  recovered work.
- Pinned terminal summary accounting as zero-allocation over retained trace.
- Tightened storage-lane capacity to mean total accepted pending work, then
  proved a user-shaped `LocalApp` storage overload path where one journal
  append succeeds, the next returns `StorageFull`, and replay sees only the
  accepted record.
- Added a full live thread-per-core service proof: real TCP ingress on one
  shard, cross-shard durable journal append on another shard, cross-shard ack
  back, client reply after persistence, shutdown, and journal replay.
- Added randomized DST pressure for single-shard and multi-shard histories:
  delayed sends, timers, stop, panic, mailbox pressure, remote queue pressure,
  stale sends, unknown targets, replay equality, causal trace checks, and
  no-turn-after-stop invariants.
- Added DST pressure for persistence fault matrices, supervision plus
  persistence recovery, TCP cancellation tombstones, bridge ingress timeout
  cancellation, shrinker smoke proof, and live-vs-sim parity over send/stop
  closed-rejection semantics.

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

### Phase Victor Marijnen

- Made `LocalSystemConfig` the live bounded-shape manifest for ingress,
  shard-pair transport, storage, DNS, TLS, process, signal, trace retention,
  and idle wait.
- Added live local cross-shard isolate calls with requester-shard-owned pending
  state, bounded request/reply transport, typed success/full/closed/timeout
  outcomes, and DST coverage for reply paths.
- Split live source-to-destination remote transport from worker command ingress
  so shard-pair capacity is a real bounded queue, not a soft metric.
- Added native inbound TLS server support with `TlsListenerId`, `tls_bind`,
  `tls_accept`, `tls_close_listener`, existing TLS read/write/close, static
  cert/key scope, total accept/handshake deadline, and negative-path tests for
  invalid key, failed handshake, lane full, timeout, and shutdown.
- Added live resource inventory and terminal shutdown accounting for TCP
  listeners/streams, TLS listeners/streams, UDP sockets, files, and pending
  driver work. Shutdown `clean` now requires no remaining owned resources.
- Added and updated LocalSystem e2e proofs for topology, resource accounting,
  cross-shard calls, TLS server hosting, configured lane capacities, worker
  failure visibility, and shutdown behavior.
- Updated `tina-sim` TLS scripts to model server bind/accept/close outcomes
  deterministically without pretending to test cryptography.
