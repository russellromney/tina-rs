# Phase 123: Adversarial Hardening

## Status

- Ready.
- One PR preferred. Two PRs allowed only if HTTP protocol hardening and
  runtime/bridge hardening conflict badly.

## Goal

Fix the real bugs found by the two intensive adversarial reviews.

This is not another review phase. The reviews are done. Build the fixes.

Grug truth:

- parser accepts only what downstream code can safely consume;
- timeout truth is the same across bridge crates;
- remote replies are never silently dropped;
- process and persistence failure paths settle;
- hot runtime paths do not hide O(n) work;
- pool/resource ownership cannot leak quietly;
- compile-time helpers do not depend on fragile crate names;
- tests prove user-visible behavior, not just helper internals.

Canonical review artifact: `docs/adversarial-review.md` from PR #135. PR
#134 is superseded once its unique findings are folded into #135.

## Rock 1: HTTP/1 Keepalive Chunked Safety

Fix C1.

Current bug: keepalive client accepts `Transfer-Encoding: chunked`, treats
`content_length = 0` as complete, returns an empty body, and leaves chunk bytes
to corrupt the next response.

Implement the full safe path:

- `KeepaliveConnection` must decode chunked response bodies using the existing
  chunked decoder.
- It must not deliver until the terminating chunk is consumed.
- First form is grug-simple: decode the chunked body, return it, and retire the
  connection after every chunked response. Do not try to preserve pipelined
  bytes in this phase.
- It must reject unsupported transfer codings visibly and retire the connection.
- It must never turn a chunked response into an empty buffered body unless the
  decoded body is truly empty.

Required tests:

- one chunked response over keepalive returns decoded body;
- two sequential keepalive requests where first response is chunked and second
  is content-length do not cross-contaminate;
- chunked response with malformed chunk returns typed parse/protocol error and
  retires;
- chunked response larger than body cap returns typed body-too-large and
  retires;
- chunked plus `Connection: close` returns decoded body and retires;
- a smuggling-shaped server response
  (`5\r\nhello\r\n0\r\n\r\nHTTP/1.1 200...`) cannot make request 2 see stale
  bytes as its response unless request 2 was actually issued and bytes belong
  to the real next response.

## Rock 2: HTTP Parser And WebSocket Strictness

Fix H14, L13, A6, M1, M6, A7, and L14. If any one is already fixed on current
main, add/keep the regression test that proves it and mark it fixed in the
review doc.

Chunked decoder:

- reject whitespace before chunk size;
- use checked arithmetic for `decoded_total + size` and frame-end math;
- fix the `DataCrlf` remaining/input check so it reads the right buffer;
- add focused property-style tests around chunk sizes near caps and
  `usize::MAX` parse inputs.

WebSocket parser:

- reject non-minimal length encodings:
  - len < 126 must not use 126 form;
  - len <= 65535 must not use 127 form;
  - 127 form with high bit set is invalid;
- use checked arithmetic for computed frame end;
- validate fragmented text as UTF-8 across continuation frames before delivery,
  and reject invalid sequences with a typed close/protocol outcome;
- keep close-code and control-frame rules visible.

HTTP/1 origin form:

- reject protocol-relative targets like `//host/path` if the parser still
  accepts them as origin-form.

Required tests:

- chunk size line with leading SP/HT is rejected;
- chunk overflow after prior decoded bytes returns body-too-large/parse error,
  never panic/wrap;
- `DataCrlf` split across reads works and malformed CRLF is rejected;
- WebSocket 126-form length 1 rejects;
- WebSocket 127-form length 125 rejects;
- WebSocket 127-form high bit rejects;
- WebSocket huge frame end cannot overflow;
- fragmented WebSocket text with invalid UTF-8 across frame boundaries rejects
  before application delivery;
- `GET //evil.test/path HTTP/1.1` rejects as bad target.

## Rock 3: HTTP/2 Protocol Hardening

Fix C2, C3, C5, H9, M7, and M14.

Implement real HTTP/2 handling for:

- DATA `PADDED` flag:
  - strip pad length byte and trailing padding before body accounting;
  - reject invalid padding with `PROTOCOL_ERROR`;
- HEADERS `PADDED` and `PRIORITY` flags:
  - strip padding;
  - skip the 5-byte priority section before HPACK;
  - reject malformed flag/payload combinations;
- SETTINGS payload:
  - parse every 6-byte setting;
  - ACK only after applying known settings;
  - apply `INITIAL_WINDOW_SIZE` delta to all open streams;
  - enforce peer `MAX_FRAME_SIZE` for outbound DATA frames;
  - `HEADER_TABLE_SIZE`: support the peer setting only if the current HPACK
    encoder has a real table-size API. If it does not, reject non-default table
    size with a connection-level `SETTINGS_ERROR`. Do not silently ACK a setting
    Tina cannot honor.
  - reject invalid settings values with `PROTOCOL_ERROR`;
- forbidden HTTP/1 connection-control headers in HTTP/2:
  - `connection`, `keep-alive`, `proxy-connection`, `transfer-encoding`,
    `upgrade`;
  - reject uppercase header names;
  - require `:authority` or a valid equivalent if the method/path requires
    authority;
- rapid reset guard:
  - add config fields with conservative defaults for reset rate window/count;
  - count open+reset churn, not only concurrent streams;
  - send GOAWAY/RST with `ENHANCE_YOUR_CALM` when exceeded;
  - report/trace the pressure.

Required tests:

- padded DATA body delivers only real data and counts caps on unpadded bytes;
- bad DATA padding emits/returns protocol error;
- priority HEADERS with valid HPACK succeeds;
- padded+priority HEADERS succeeds;
- malformed padded/priority HEADERS rejects;
- SETTINGS initial window shrink blocks outbound DATA until WINDOW_UPDATE;
- SETTINGS max frame size affects outbound frame splitting;
- invalid SETTINGS values reject;
- HTTP/2 request with `connection` rejects;
- uppercase header rejects;
- missing authority rejects where required;
- rapid reset storm hits guard and closes/goaways before unbounded service
  dispatch;
- normal reset rate remains accepted.

## Rock 4: Cross-Shard Reply Truth

Fix C4 and M13.

Current bug: a remote reply envelope can be dropped when the reverse remote
queue is full. Caller then sees timeout instead of the real `Full`/`Closed`/
rejection truth.

Implement bounded terminal-reply reliability:

- add a reserved terminal-reply lane per shard-pair, separate from ordinary
  remote send/call request traffic;
- if a terminal reply cannot be delivered immediately, put it in that bounded
  terminal lane;
- terminal lane drains before ordinary remote traffic;
- if the terminal lane is full, settle the requester with a typed local
  `CallOutcome::Closed`/rejection if the requester shard still owns the pending
  call. If the requester cannot be found, emit a loud trace event that names the
  lost terminal cause. The caller must never wait silently forever because a
  terminal reply was dropped;
- add concrete trace vocabulary for this path, for example
  `RemoteTerminalQueued`, `RemoteTerminalDelivered`, and
  `RemoteTerminalDropped`. The names can differ, but the facts must be typed,
  grep-able, and tested;
- simulator and live multi-shard must share the same contract;
- every remote call/send attempt must have exactly one visible terminal outcome.

Required tests:

- live two-shard call to saturated target returns `Full`, not timeout;
- reverse queue saturated while returning `Full` still eventually reaches
  caller;
- reverse retry lane full has a typed terminal trace/outcome, not silent drop;
- simulator mirrors the same cases;
- generated/property test: every cross-shard call attempt ends in one of
  `Replied`, `Full`, `Closed`, `Rejected`, or `Timeout` for an actual deadline,
  never because a terminal reply was dropped.

## Rock 5: Bridge Timeout And Capacity Truth

Fix H1, H2, A1, H13, M8, and the duplicate AWS/SQLx timeout semantics.

One bridge contract for all bridge crates:

- `max_in_flight` caps admitted external work, not just callers currently
  waiting;
- caller-visible timeout settles caller authority promptly;
- late physical work is tracked separately as `late_in_flight` /
  worker-terminal truth;
- late physical work remains counted against external capacity until terminal.
  This is the chosen first form. Caller capacity is reclaimed; external
  capacity is not.
- metrics distinguish:
  - admitted/current caller waiters;
  - external work still running;
  - late work after caller timeout/cancel;
  - worker-terminal outcome;
  - caller-visible outcome;
- no crate may silently choose the opposite semantics under the same field name.

Specific fixes:

- `tina-rpc-tokio`: `CancelGuard::drop` releases a permit only if it actually
  removed the pending entry. Add the race test that was described in the
  review, but make it call the real guard path.
- `tina-sqlx-bridge`: after timeout, keep the external slot occupied until the
  SQLx task reaches terminal. Caller receives timeout promptly, but new work may
  see `Full` until the physical DB work finishes. Metrics must name
  `caller_waiting`, `external_in_flight`, and `late_in_flight`.
- `tina-sqlx-bridge`: DB-side Postgres cancel must not target a later query.
  Keep the connection quarantined until the cancel sidecar has completed or the
  original query has reached terminal. If that cannot be implemented cleanly,
  remove DB-side cancel-on-timeout from the public config for now and document
  Tina-side timeout as "stop waiting, DB may still run." Do not keep the current
  backend-PID race.
- `tina-aws-bridge`: S3/SQS/SNS/DynamoDB/Secrets already mostly keep external
  capacity occupied after timeout. Keep that contract, but rename/report metrics
  so users can see caller timeout vs late external work instead of thinking the
  bridge leaked caller authority.
- `tina-reqwest-bridge`: do not retry a vanished worker task
  (`TryRecvError::Closed`) as if it were a retryable network error. Classify as
  internal/fatal.
- AWS late-result metrics must not double-count class counters.

Required tests:

- RPC Tokio bridge cancellation/drop race cannot inflate permits above max;
- SQLx timeout with `max_in_flight = 1`: second call behavior matches the
  documented external-capacity contract and metrics prove why;
- SQLx DB-side cancel race proof with pool size 1: request B is not cancelled
  by request A's late cancel;
- AWS fake/stub worker stuck future: timeout settles caller and later
  calls/metrics match the chosen contract;
- reqwest closed task is fatal and not retried for non-idempotent request;
- bridge pressure reports include caller/external/late counts where relevant;
- docs table says the same timeout/capacity story for sqlx, aws, reqwest,
  sqlite.
- every bridge with caller timeout has one test where caller timeout fires,
  external work later finishes, and capacity/metrics show exactly who was still
  running.

## Rock 6: Process Timeout And Shutdown Hardening

Fix A2, H6, L10, and L11.

Process rail:

- Unix: spawn child commands in a new process group/session;
- timeout/cancel kills the whole process group, not only the direct child;
- Windows: use a job object. If the current platform/backend cannot support it,
  return/report a typed `ProcessGroupUnsupported` style outcome for group-kill
  tests rather than pretending direct-child kill is equivalent;
- stdout/stderr drain joins after kill must have a bounded wait;
- if drains do not settle, return/report a typed partial-kill/pipe-still-open
  outcome. Do not hang.

Runtime shutdown:

- `ThreadedRuntime::shutdown()` keeps current cooperative blocking semantics.
- Add `shutdown_with_timeout(timeout)` that returns a loud `TimedOut`/`Failed`
  report instead of joining forever.
- `Drop` must not call an unbounded join. It should initiate best-effort
  shutdown and detach/return after the configured/default drop budget, with a
  visible report path for explicit shutdown callers.
- trace/report keeps terminal truth.

Driver cancel loops:

- replace `thread::yield_now()` spin loops in storage/DNS/TLS/process cancel
  paths with bounded sleep/backoff or blocking/cancellable design.

Required tests:

- process timeout for `sh -c 'sleep 1000 & sleep 1000'` settles on Unix and
  does not hang drains;
- process stdout grandchild inheritance does not hang;
- process timeout report names partial cleanup if platform cannot kill group;
- runtime shutdown with a wedged handler returns timed-out/failed report within
  budget;
- shutdown still succeeds normally for cooperative handler;
- no cancel loop burns with pure yield in the tested stuck path.

## Rock 7: Persistence Hot-Path And Repair

Fix A3, A4, A5, and strengthen old persistence claims.

Journal repair:

- `journal_replay` must expose valid prefix byte length when it returns a
  truncated-tail warning;
- before the next append, repair/truncate to the valid prefix or return a typed
  `NeedsRepair` with a repair helper;
- appending after crash-truncated tail must not be permanently blocked.

Journal append scaling:

- remove full-journal replay from every append hot path;
- track/validate last committed index using tail metadata, sidecar, or runtime
  state;
- startup/full recovery may still replay the whole journal;
- append latency should be O(new record + small tail state), not O(total
  journal).

Snapshot temp cleanup:

- after temp file creation, any failure path attempts best-effort temp cleanup
  while preserving the primary error. Cleanup failure is also recorded in the
  returned report/error; do not make temp cleanup a silent lie;
- test rename failure leaves no temp garbage.

Required tests:

- valid record + truncated tail replays prefix and then append repairs/truncates
  and succeeds;
- complete corrupt record still fails visibly and does not truncate silently;
- append many small records and assert validation does not read/replay the whole
  file each time. Use an instrumentation seam if needed;
- snapshot rename failure removes temp file;
- simulator durable image still replays snapshot/journal order;
- docs say local persistence is snapshot/journal helper, not durable mailbox.

## Rock 8: Runtime Hot-Path And Supervision Truth

Fix H4, H5, H7, H10, H11, and L5.

Trace retention:

- replace bounded trace `Vec::remove(0)` with `VecDeque` or equivalent O(1)
  ring;
- preserve event order and public trace snapshots;
- existing trace hashes must remain stable for unbounded/exact traces.

Trace observer:

- keep synchronous observer available and documented;
- add a bounded buffered observer path for production use;
- dropping observer events due to full buffer must be counted and visible.

LiveTrace determinism:

- proof-harness live trace snapshot must sort by event id before hashing,
  especially multi-shard.

Supervision budget:

- rename or extend `RestartBudget` so semantics are honest;
- preferred: add time-window budget (`within(max, period)`) and keep lifetime
  limit as an explicit `lifetime(max)` form;
- tests prove old permanent-exhaustion surprise is gone for windowed budget.

Entry table/restart leak:

- stopped entries must not grow unbounded across restarts after all in-flight
  work settles;
- add O(1) lookup if needed.

Cancel cause ring:

- make cancelled-call cause attribution capacity configurable or otherwise
  prove overflow degrades loudly with a metric/event.

Required tests:

- bounded trace retention under high event count has O(1)-shape proof or
  allocation/move regression test;
- buffered observer full increments dropped count and does not block shard;
- multi-shard live trace hash is deterministic across repeated same workload;
- windowed restart budget resets after period and lifetime budget remains
  explicit;
- repeated child restarts do not grow live entry table without bound;
- cancel-cause overflow is visible.

## Rock 9: Pool, RPC, And Resource Ownership Truth

Fix H3, M5, M21, L1, L6, L8, and L15.

Pool leases:

- `PoolLease` must not silently leak a resource forever on panic, early return,
  owner stop, or `OneForAll` stop.
- Preferred user shape: add a closure/turn-scoped lease helper that makes the
  safe path the copied path.
- Also add visible leak/abandoned-lease accounting for stored leases that cannot
  be auto-returned safely. Do not pretend Rust `Drop` can always send a Tina
  effect from arbitrary context.
- Pool close must still retire outstanding leases as already promised.

RPC request identity:

- server-side `Connection.in_flight` must become request-id aware, not only a
  count;
- duplicate request id while an old one is in flight must return a typed
  protocol error or close the connection. It must not dispatch duplicate work.

Bridge shim sizing:

- bridge shim mailbox sizing must be safe across repeated cancel/retry waves,
  not only one wave;
- add pressure tests for two back-to-back cancellation waves.

Deferred/pool internals:

- `PendingReplies::take()` must update reclaim counters or docs must stop
  promising it does;
- `DeferredSlotShared` ordering must match the call-handle ordering discipline
  unless a test proves `Relaxed` is enough;
- replace public `unsafe fn` contract markers with sealed/private authority
  types or clearly memory-safety-relevant unsafe. User-facing "do not call this"
  should not be `unsafe` theater.

SQLx fetch cap:

- `FetchMany` cap+peek must not decode/buffer an extra full row beyond the
  documented cap unless the docs and metrics name that one-row peek cost.

Required tests:

- panic or owner stop while holding a pool lease produces visible abandoned or
  retired resource truth and pool report stays coherent;
- safe scoped lease helper returns/retire resource on success and error paths;
- duplicate RPC request id cannot run service code twice;
- two cancel/retry waves cannot exceed shim mailbox budget;
- `PendingReplies::take()` counter behavior is pinned;
- loom/unit test pins deferred slot memory-ordering expectations;
- public pool constructors cannot mint/forge leases without authority;
- SQLx `FetchMany` cap test proves only the documented rows are decoded or that
  the one-row peek is explicitly counted.

## Rock 10: Runtime Call, Time, Lifecycle, And Shutdown Truth

Fix M2, M3, M16, M23, M24, M25, L2, L3, L4, and L16.

Call/cancel/time:

- `cancel_call` before the call effect is admitted must return a typed outcome,
  not panic the shard;
- call deadline math must use checked/saturating add like Tina time helpers;
- `call_blocking` must distinguish the host wait budget from the target call
  deadline. User can choose same duration, but the API/report must not conflate
  them.

Lifecycle reports:

- `ShutdownChoreography::record` must report the highest completed step, not
  whichever step happened last;
- trace/event projection must preserve `CallError::Rejected(reason)` inner
  reason so dashboards do not lose the cause;
- `AddressGeneration` should either become real generation truth or be removed
  from public claims. Do not keep a dead field that suggests stale-address
  detection exists.

Signals and TLS close:

- signal registration must be process-global and explicit. Do not silently
  multiply SIGINT/SIGTERM handlers by shard count or demote an embedder's
  existing handler without a typed setup error/report;
- TLS close must not return `TlsFull` only because cancelled-but-still-pending
  ops are counted as live close pressure.

Timers:

- `RecurringTick::Bounded(0)` missed-tick count must match the documented
  policy;
- `elapsed_periods` must not silently truncate `u128` to `u64`.

Required tests:

- cancel-before-admit returns typed failure and shard keeps running;
- huge duration call deadline does not panic/wrap;
- `call_blocking(host_budget < target_deadline)` reports host timeout
  distinctly;
- shutdown choreography out-of-order records highest step correctly;
- trace projection includes rejected reason;
- signal setup is one-per-process and reports conflict;
- TLS close after cancellation pressure either admits close or returns a typed
  truthful outcome;
- recurring tick edge cases are pinned.

## Rock 11: TLS Lane, SPSC Mailbox, And Macro Hygiene

Fix H8, H12, M11, M12, M22, L7, L9, and L12.

TLS lane:

- the public TLS lane docs/config must stop implying queue capacity means
  concurrency;
- either add real bounded worker concurrency for TLS work or rename/report the
  shape as one worker plus bounded queue;
- blocking TLS read/write/flush under `Arc<Mutex<_>>` must remain owner-thread
  only by type/API, or be refactored so future shared access cannot deadlock.

SPSC mailbox:

- require power-of-two capacity or use arithmetic that remains sound across
  wraparound for all capacities;
- add loom coverage for producer + consumer + closer interleavings.

Macros:

- proc macros must not hard-code `::tina`, `::tina_runtime`, or `::tina_rpc`
  without an escape hatch;
- use parent-crate `__private` re-exports or an explicit `*_crate = path`
  attribute;
- emitted `Infallible` should use `core` where possible;
- the call-authority lint must not reject helper-macro-based handlers if they
  still consume the authority correctly;
- RPC macro positional tuple ABI must be documented or replaced with a stable
  named encoding.

Required tests:

- renamed Cargo dependencies compile for `tina` and `tina-rpc` macros;
- helper macro around call authority compiles when it really consumes the
  authority and fails when it does not;
- no-std-ish macro expansion uses `core::convert::Infallible` where possible;
- SPSC loom test covers close racing producer/consumer;
- non-power-of-two mailbox capacity is rejected or proven safe;
- TLS lane pressure report names queued vs concurrent work truth.

## Rock 12: Simulator And Proof-Harness Truth

Fix M9, M10, M15, M18, M19, M20, L17, and L18.

Simulator faults and time:

- replace `(seed + tag + ordinal) % one_in` with a real deterministic PRNG per
  simulator, with per-tag streams;
- simulator virtual time must not leak wall-clock `Instant::now()` into replay
  facts;
- saved replay cases must verify projection/config facts structurally, not only
  keep `Debug` strings that nobody checks.

Proof harness:

- `LiveTrace` poisoned mutex must fail loudly or preserve prior good truth; it
  must not silently drop events and bless a bad hash;
- `MultiShardSimulator` needs trace observer support or docs/tests must stop
  implying live/sim observer parity;
- `BadPeerScenario::ResetImmediately` must produce a real TCP RST or be renamed
  to FIN;
- `LoadReport::first_error_op_index` must be global if named global, or renamed
  to local;
- `run_storm` must report all connection errors or aggregate counts, not only
  the last error while claiming `connected=true`.

Required tests:

- same seed produces same fault sequence; different tags do not correlate
  trivially;
- saved replay rejects mismatched structured config/projection;
- poisoned live trace path is visible;
- multi-shard sim observer sees events if the API exists;
- reset scenario is verified with peer-visible RST behavior or renamed;
- load/storm reports preserve aggregate error truth.

## Rock 13: SQLx Transaction Outcome And Bridge Edge Truth

Fix M4, M17, M18 where bridge/test harness surfaces overlap, and any
bridge-specific doc drift left by earlier rocks.

SQLx transaction ambiguity:

- transaction commit ambiguous outcome must include completed step records;
- users need to know which steps definitely ran and where ambiguity began.

Tokio bridge drain:

- `drain_and_shutdown` must not block a Tokio worker with a sleep polling loop;
- provide async-friendly or thread-offloaded drain, or document a synchronous
  blocking API with a non-Tokio caller test.

Bad-peer harness:

- if a bridge/specimen claims reset behavior, use the real reset helper fixed
  in Rock 12.

Required tests:

- SQLx transaction commit ambiguity returns completed steps plus error;
- Tokio bridge drain does not park the runtime worker under Axum-style shutdown;
- bridge docs do not promise late-result/capacity facts that metrics cannot
  prove.

## Rock 14: Docs, Findings, And Follow-Up Split

Update docs after fixes land:

- `docs/adversarial-review.md`: mark fixed items with phase/test references or
  add a top "fixed in Phase 123" note. Do not delete the original bug text.
- `CHANGELOG.md`: record user-visible hardening.
- `ROADMAP.md`: move fixed items out of future work; leave only true future
  work.
- PR #134 and PR #135 are already superseded by commits on main. Keep the
  review doc in git history even if a later cleanup removes or summarizes it.

If a finding is proven false while implementing, update the review doc with the
proof and test name. Do not silently ignore it.

## Tests To Run

Minimum targeted checks:

```sh
cargo fmt --all --check
cargo test -p tina-http keepalive --tests -- --nocapture
cargo test -p tina-http chunked --tests -- --nocapture
cargo test -p tina-http websocket --tests -- --nocapture
cargo test -p tina-http http2 --tests -- --nocapture
cargo test -p tina-runtime --test multishard_dispatcher -- --nocapture
cargo test -p tina-sim multishard --tests -- --nocapture
cargo test -p tina-rpc-tokio --tests -- --nocapture
cargo test -p tina-sqlx-bridge --tests -- --nocapture
cargo test -p tina-aws-bridge --tests -- --nocapture
cargo test -p tina-reqwest-bridge --tests -- --nocapture
cargo test -p tina-runtime process --tests -- --nocapture
cargo test -p tina-runtime persistence --tests -- --nocapture
cargo test -p tina-runtime supervisor --tests -- --nocapture
cargo test -p tina-runtime time --tests -- --nocapture
cargo test -p tina-runtime lifecycle --tests -- --nocapture
cargo test -p tina-mailbox-spsc --tests -- --nocapture
cargo test -p tina-macros --tests -- --nocapture
cargo test -p tina-rpc-macros --tests -- --nocapture
cargo test -p tina-proof-harness --tests -- --nocapture
cargo clippy -p tina-http --tests -- -D warnings
cargo clippy -p tina-runtime --tests -- -D warnings
cargo clippy -p tina-sqlx-bridge --tests -- -D warnings
cargo clippy -p tina-aws-bridge --tests -- -D warnings
cargo clippy -p tina-mailbox-spsc --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

If a crate has external-service integration tests, keep hermetic tests as the
merge gate and leave real-service tests ignored with clear env vars.

## Done Means

- HTTP/1 keepalive cannot smuggle chunked bytes into the next response.
- HTTP/2 handles padded/priority/settings/reset-storm/header-forbidden cases
  honestly.
- Cross-shard terminal replies do not disappear into timeout fog.
- Bridge timeout/capacity semantics are shared and visible across bridge crates.
- Pool/resource ownership leaks are either prevented by the safe path or loudly
  reported.
- Process timeout and runtime shutdown do not hang forever.
- Persistence can repair truncated tails and append without replaying the whole
  journal every time.
- Runtime trace/supervision hot paths no longer hide obvious production traps.
- Macro, simulator, mailbox, and proof-harness findings from the review doc are
  fixed or marked false with tests.
- Tests prove the weird user-shaped failures that the reviews found.
