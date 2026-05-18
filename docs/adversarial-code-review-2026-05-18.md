# Adversarial Code Review Findings

Review date: 2026-05-18

Base reviewed: `origin/main` at `c8b217b8025f3b78f80e8d2303da489f46718b87`

This review focused on correctness, reliability, security-adjacent failure modes,
data-loss risks, protocol edge cases, async/concurrency races, and LLM-style
implementation mistakes. It is not a style review.

## Findings

### 1. Postgres timeout cancellation can cancel the wrong query

- Severity: High
- Confidence: Medium
- References: `tina-sqlx-bridge/src/worker.rs:392`,
  `tina-sqlx-bridge/src/worker.rs:837`
- Risk: The timeout path captures a Postgres backend PID and later fires
  `SELECT pg_cancel_backend(pid)` from a sidecar task. If the original query
  completes around the timeout boundary and the pool reuses the same backend
  before the sidecar cancel executes, the cancel can hit a different request.
- Why this can happen in practice: Pool size 1 or a hot pool makes backend reuse
  immediate. The code clears the PID after `run_on_conn`, but the timeout poll
  can already have swapped out a non-zero PID and scheduled the sidecar cancel.
  `pg_cancel_backend` identifies only a backend, not a specific query instance.
- Reproduction idea: With a pool size of 1 and cancel-on-timeout enabled, run
  request A with a timeout near its natural completion, then immediately run
  request B on the same pool. Force or delay the sidecar cancel so it lands while
  B is active; B can be canceled even though it did not time out.
- Suggested fix: Coordinate cancellation with the task that owns the pooled
  connection. On timeout, either hold/quarantine/drop the connection until the
  cancel attempt has completed, or use a cancellation mechanism tied to the
  specific connection owner rather than a detached backend-PID sidecar. If this
  cannot be made query-specific, default to not issuing DB-side cancel.
- LLM-style pattern: Yes. This looks like a plausible atomic race fix that
  misses resource identity reuse.

### 2. Keepalive HTTP client accepts chunked responses but returns an empty body

- Severity: High
- Confidence: High
- References: `tina-http/src/parse.rs:580`,
  `tina-http/src/keepalive.rs:618`, `tina-http/src/keepalive.rs:634`
- Risk: `parse_response_head` accepts `Transfer-Encoding: chunked` by setting
  `content_length = 0`, while the keepalive client treats `content_length == 0`
  as a complete response. It delivers an empty body and leaves the encoded
  chunks unread on the socket, corrupting the next response parse.
- Why this can happen in practice: HTTP/1.1 servers commonly use chunked
  transfer encoding on reusable connections. The non-keepalive client supports
  chunked responses, so callers have no obvious reason to avoid this path.
- Reproduction idea: A test server replies
  `HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n`
  and keeps the socket open. The first keepalive request currently succeeds with
  an empty body; the next request starts parsing at the leftover chunk bytes.
- Suggested fix: Either implement chunked decoding in `KeepaliveConnection`, or
  reject chunked responses in the keepalive path and retire the transport. Do not
  allow the parser's chunked success case to flow into a length-based body path.
- LLM-style pattern: Yes. The parser and consumer enforce different invariants.

### 3. Process timeout can hang forever when grandchildren inherit pipes

- Severity: High
- Confidence: Medium
- References: `tina-runtime/src/driver/process.rs:299`,
  `tina-runtime/src/driver/process.rs:314`,
  `tina-runtime/src/driver/process.rs:337`
- Risk: On timeout or cancellation, the runtime kills only the direct child and
  then joins stdout/stderr drain threads. If the child spawned a grandchild that
  inherited stdout or stderr, those pipes remain open and the drain threads never
  reach EOF.
- Why this can happen in practice: Shell commands and wrappers often spawn
  background children. `sh -c 'sleep 1000 & sleep 1000'` is enough to leave
  inherited descriptors behind.
- Reproduction idea: Run a process command such as
  `sh -c 'sleep 1000 & sleep 1000'` with a very small timeout. The direct child
  is killed, but the drain join can block on the grandchild-held pipe.
- Suggested fix: On Unix, spawn the command into a new process group/session and
  kill the whole group on timeout. On Windows, use a job object or document the
  limitation. Also avoid unbounded joins on drain threads after a kill path.
- LLM-style pattern: Yes. Direct-child lifecycle handling looks complete but
  misses process-tree semantics.

### 4. AWS bridge timeouts do not free `max_in_flight` capacity

- Severity: Medium
- Confidence: High
- References: `tina-aws-bridge/src/worker.rs:365`,
  `tina-aws-bridge/src/sqs_worker.rs:362`,
  `tina-aws-bridge/src/dynamodb_worker.rs:374`,
  `tina-aws-bridge/src/sns_worker.rs:358`,
  `tina-aws-bridge/src/secrets_worker.rs:357`
- Risk: After returning a Tina-side timeout, the workers reinsert the in-flight
  entry and keep polling until the SDK future completes. A stuck SDK call can
  permanently consume a `max_in_flight` slot, causing subsequent requests to
  fail with `Full`.
- Why this can happen in practice: Network calls can hang for a long time under
  DNS, TLS, proxy, endpoint, or SDK retry failures. The public bridge reports a
  timeout, so callers reasonably expect capacity to recover.
- Reproduction idea: Configure `max_in_flight = 1` and direct the bridge at a
  never-completing endpoint or test client. After the first request times out,
  submit a second request and observe `Full` until the SDK future ends.
- Suggested fix: Separate user-visible in-flight capacity from physical late
  task tracking. Once a timeout is reported, release admission capacity and track
  abandoned futures under a separate bounded budget, or abort/drop the SDK task
  when supported.
- LLM-style pattern: Yes. Late-result accounting was implemented, but capacity
  semantics were left coupled to physical task completion.

### 5. Crash-truncated journal tails block all future appends

- Severity: Medium
- Confidence: High
- References: `tina-runtime/src/persistence.rs:164`,
  `tina-runtime/src/persistence.rs:245`
- Risk: `replay_journal` treats a truncated tail as a warning and returns the
  valid prefix, but `validate_next_journal_index` rejects any warning. After a
  crash during append, the system can read recovered records but cannot append a
  new record without manual repair.
- Why this can happen in practice: The journal append writes bytes before fsync.
  Power loss or process death can leave a partial header or payload at the end.
- Reproduction idea: Create a journal with one valid record plus a partial
  second header. `replay_journal` returns the first record with a warning, then
  `append_journal_record` returns `CorruptRecord`.
- Suggested fix: Make replay expose the valid byte length, then truncate the
  journal to that length before accepting the next append. Alternatively add an
  explicit repair API and call it from append validation.
- LLM-style pattern: Yes. The recovery warning exists, but the write path does
  not complete the recovery story.

### 6. Journal append is O(total journal size) and can starve storage

- Severity: Medium
- Confidence: High
- References: `tina-runtime/src/persistence.rs:131`,
  `tina-runtime/src/persistence.rs:158`
- Risk: Every append calls `validate_next_journal_index`, which replays the
  entire journal with `read_to_end`. Appending many records becomes quadratic,
  and a large journal can consume substantial memory and block the storage lane.
- Why this can happen in practice: Journals are expected to grow over time until
  compaction/snapshotting. Small-record workloads make the O(n^2) behavior show
  up quickly.
- Reproduction idea: Append many small records and measure append latency against
  journal size. Latency grows with the full file, not with the new record.
- Suggested fix: Track the last committed index in runtime state, scan only the
  final record, or maintain a compact sidecar/checkpoint. Keep full replay for
  startup/repair, not for every append.
- LLM-style pattern: Yes. A simple validation path is correct on small inputs but
  has hidden scaling failure.

### 7. Chunked decoder length accounting can overflow

- Severity: Medium
- Confidence: Medium
- References: `tina-http/src/chunked_decoder.rs:96`,
  `tina-http/src/chunked_decoder.rs:119`
- Risk: `self.decoded_total + size` can overflow after some decoded bytes and a
  very large declared chunk size. Debug builds can panic; release builds can wrap
  and move into a huge `Data { remaining }` state.
- Why this can happen in practice: Chunk size is peer-controlled hexadecimal
  input. A hostile client can send a valid small chunk followed by
  `ffffffffffffffff`.
- Reproduction idea: Feed `1\r\na\r\nffffffffffffffff\r\n` into a decoder with a
  small max body and assert that it returns `BodyTooLarge` rather than panicking
  or entering `NeedMore`.
- Suggested fix: Replace the addition with `checked_add` or compare
  `size > max_body_bytes - decoded_total` after guarding `decoded_total <= max`.
- LLM-style pattern: Yes. This is classic unchecked arithmetic in parser code.

### 8. Snapshot temp files are left behind after rename failure

- Severity: Low
- Confidence: High
- References: `tina-runtime/src/persistence.rs:95`,
  `tina-runtime/src/persistence.rs:106`
- Risk: If the temp snapshot is written and fsynced but `rename` fails, the temp
  file is not cleaned up. Repeated failures can accumulate stale state files next
  to the real snapshot.
- Why this can happen in practice: Rename can fail if the target path is an
  existing directory, if permissions change, or under platform-specific
  filesystem errors.
- Reproduction idea: Point the snapshot path at an existing directory name or
  otherwise force `rename` to fail after temp creation. The temp file remains.
- Suggested fix: On any failure after temp creation, attempt a best-effort
  `remove_file(temp_path)` while preserving the primary error.
- LLM-style pattern: Yes. Partial-failure cleanup is often missed.

### 9. WebSocket frame parser accepts non-canonical lengths and uses unchecked end math

- Severity: Low
- Confidence: Medium
- References: `tina-http/src/websocket.rs:740`,
  `tina-http/src/websocket.rs:746`, `tina-http/src/websocket.rs:779`
- Risk: The parser accepts non-minimal extended payload lengths, such as a
  one-byte payload encoded with the 126 form. It also computes
  `offset + 4 + len` without checked arithmetic.
- Why this can happen in practice: Malicious or non-compliant peers can send
  non-canonical frames. Custom limits could make unchecked arithmetic relevant
  even though defaults are small.
- Reproduction idea: Send a masked text frame with length marker `126` and an
  actual length of 1. The parser accepts it instead of closing as protocol
  error.
- Suggested fix: Reject non-minimal encodings and use checked addition for the
  computed frame end.
- LLM-style pattern: Yes. The parser handles ordinary framing but misses strict
  wire-protocol edge cases.

## Highest-Risk Modules Reviewed

- `tina-http`: parsing, client, keepalive, WebSocket, and streaming paths.
- `tina-runtime`: persistence, storage lane, and process lane.
- `tina-rpc`: frame codec, client, and connection isolates.
- `tina-aws-bridge`: S3, SQS, SNS, DynamoDB, and Secrets worker patterns.
- `tina-sqlx-bridge` and `tina-sqlite-bridge`: timeout and shutdown behavior.
- `tina-mailbox-spsc`, pool, wait-list, and guarded pending primitives.

## Areas That Need Deeper Review

- HTTP/2 and gRPC live paths, especially stream reset and flow-control failure
  semantics.
- Supervisor restart behavior under partially completed I/O effects.
- DST simulator invariants for persistence repair and connection lifecycle.
- Multi-process or multi-runtime ownership assumptions around persistence files.

## Suggested Tests

- Keepalive integration test for chunked responses on a reusable HTTP/1.1
  connection.
- Chunked decoder overflow unit test and property test around declared sizes.
- Process timeout test with a grandchild that inherits stdout/stderr.
- Journal truncated-tail repair test.
- Journal append benchmark/property test that guards against O(n^2) behavior.
- AWS bridge stuck-future test showing admission capacity recovers after timeout.
- SQLx pool-size-1 cancellation race integration test.
- WebSocket frame fuzzing for length canonicalization and checked length math.

## Invariants To Enforce

- A parser must not accept a transfer encoding unless every downstream consumer
  on that path can decode or explicitly reject it.
- A timeout cancellation mechanism must not affect work that began after the
  timed-out operation released its resource.
- User-visible timeout must not permanently consume admission capacity.
- Journal replay warnings need a repair/truncation path before the next append.
- Bounded parsers should use checked arithmetic for peer-controlled lengths.
- Persistence temp files should be removed on failed commit attempts.

## Top 10 Fixes To Prioritize

1. Prevent SQLx Postgres timeout cancellation from canceling later queries.
2. Fix or reject chunked responses in the keepalive HTTP client.
3. Kill process groups and avoid unbounded pipe-drain joins on process timeout.
4. Release or separately account AWS bridge capacity after user-visible timeout.
5. Add journal truncated-tail repair before future appends.
6. Remove O(total journal size) validation from every journal append.
7. Add checked arithmetic to the chunked decoder.
8. Clean up snapshot temp files on failed commit.
9. Enforce strict WebSocket length encoding and checked frame-end math.
10. Add regression/fuzz coverage for the protocol and persistence invariants.

## Validation Performed

- `cargo test -p tina-http chunked -- --nocapture`
- `cargo test -p tina-rpc frame_codec -- --nocapture`

The HTTP chunked-filtered tests passed. The RPC command built successfully, but
the filter matched zero tests in the current test names.
