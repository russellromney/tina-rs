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

Fix H14, L13, A6, M1, A7, and L14. If any one is already fixed on current
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

## Rock 9: Docs, Findings, And Follow-Up Split

Update docs after fixes land:

- `docs/adversarial-review.md`: mark fixed items with phase/test references or
  add a top "fixed in Phase 123" note. Do not delete the original bug text.
- `CHANGELOG.md`: record user-visible hardening.
- `ROADMAP.md`: move fixed items out of future work; leave only true future
  work.
- Close/supersede #134 once #135 is canonical and this phase references it.

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
cargo test -p tina-proof-harness --tests -- --nocapture
cargo clippy -p tina-http --tests -- -D warnings
cargo clippy -p tina-runtime --tests -- -D warnings
cargo clippy -p tina-sqlx-bridge --tests -- -D warnings
cargo clippy -p tina-aws-bridge --tests -- -D warnings
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
- Process timeout and runtime shutdown do not hang forever.
- Persistence can repair truncated tails and append without replaying the whole
  journal every time.
- Runtime trace/supervision hot paths no longer hide obvious production traps.
- Tests prove the weird user-shaped failures that the reviews found.
