# Adversarial Review — 2026-05-20

Target: `main` @ `81e3e62`. Read-only review. Method: nine parallel deep-dive
agents over playbook tracks A–I (`.intent/review/adversarial-review-playbook.md`),
an invariant pass, a truth-gap second pass, and manual source verification of the
top findings.

43 findings: 7 High, the rest Medium/Low (two Medium findings are High-in-practice:
a TLS slowloris DoS and a journal-bricking availability bug). The codebase is
disciplined; the dangerous bugs cluster at known boundaries plus one new systemic
theme (flat-Vec linear scans on the live hot path).

`✓` marks findings re-verified against source during the review.

## How to read this

Each finding gives: severity, confidence, `file:line`, the invariant or protocol
rule it breaks, the concrete bug, why it happens in real use, a repro/failing-test
idea, a fix, and whether it looks like an LLM-style bug pattern. Fix the top-10
first; they are ranked by severity × confidence × blast radius × likelihood.

## Summary by risk boundary

- **Cross-shard terminal-cause truth** — a real terminal reply (`Replied`/`Full`/
  `Closed`) can be dropped on a saturated reverse queue and degrade into a caller
  `Timeout`. The "settles exactly once with a typed cause" invariant is violable
  under ordinary fan-in. (C1, reinforced by D1/D2/D3.)
- **Hot-path data structures (new, systemic)** — call settlement, driver
  completion, and timeout harvest do linear scans / `Vec::remove` over uncapped
  flat vectors keyed by id. One isolate per connection ⇒ C10k throughput collapse.
  (I1, I2, I3, I6, I7.)
- **Durability / OS truth** — a torn write to the journal index sidecar
  permanently bricks the journal; macOS `file_fsync` is not `F_FULLFSYNC`; a
  post-reap process-group kill can hit a recycled pid. (F1, F2, F3.)
- **HTTP/2 protocol law** — DATA padding excluded from flow control (silent
  stall); SETTINGS window-increase does not resume parked responses; `TE` value
  and empty `:path` unenforced; stream-scoped errors escalated to connection
  GOAWAY. (B1–B6.)
- **WebSocket framing** — an unfragmented data frame mid-fragmentation corrupts a
  later message instead of failing the connection. (A1.)
- **Fairness / availability** — single serial TLS worker head-of-line-blocks the
  shard (slowloris); cross-shard inbound drain starves higher-id sources; a 1 ms
  sleep caps shard throughput. (F4, I4, C3, I5.)
- **Containment & safety rails** — restart-factory panic crashes the shard;
  shutdown joiner spawn-failure leaks threads and reports a false "Closed"; a
  `pub` `runtime_internal` helper defeats the must-answer-caller rail; macro
  identifier collisions on natural names. (E2, E1, H4, H1/H3.)

## Top 10 fixes

| # | Sev | Finding | Location | Fix |
|---|-----|---------|----------|-----|
| 1 | High ✓ | Cross-shard terminal reply dropped → caller `Timeout` (typed cause lost; uncapped `pending_isolate_calls` vs bounded reverse queue) | `threaded_multi_shard.rs:1025`, `multi_shard.rs:326`, `dispatch.rs:695` | Never `let _=` a terminal `CallReply`; re-buffer in a per-pair overflow area or reserve a reply slot at admission |
| 2 | High ✓ | Torn `.idx` write permanently bricks the journal | `persistence.rs:214-218` | Degrade short/garbage idx to `Ok(None)` (replay fallback); write idx via tmp+rename |
| 3 | High ✓ | O(N) scan over all registered isolates per completion (1 isolate/conn ⇒ C10k collapse) | `dispatch.rs:1189,1432,1688,1852` | `HashMap<IsolateId, usize>` index beside `entries`; keep the generation check |
| 4 | High ✓ | O(k·n) timeout harvest every step over uncapped pending vec | `dispatch.rs:1495` | `BinaryHeap`/`BTreeMap` by deadline, or cache `earliest_deadline` + early return |
| 5 | High ✓ | O(K²) driver-completion delivery (`position` + `Vec::remove` ×2) | `dispatch.rs:1764,1085,134` | Key `in_flight_calls`/`translators` by `HashMap<CallId,_>`; `swap_remove` |
| 6 | High | rpc-tokio shim overflow → awaiter hangs forever + slot leak when `Client.max_in_flight > 2×bridge.max_in_flight` | `tina-rpc-tokio/src/lib.rs:351,502` | Backstop `rx.await` with a deadline timeout that releases the slot; or size shim to `client+bridge` in-flight |
| 7 | High ✓ | HTTP/2 DATA padding excluded from flow control → silent stall | `tina-http/src/http2.rs:1196` | Charge/credit windows on full padded `frame.payload.len()`, deliver on unpadded |
| 8 | Med-High ✓ | Restart-factory panic crashes the whole shard (restart path not in `catch_unwind`) | `dispatch.rs:2239` | Wrap `recipe.create` in `catch_unwind`; emit `RestartChildSkipped{FactoryPanicked}` |
| 9 | Med-High | Single serial TLS worker HOL-blocks the shard (slowloris DoS on HTTPS) | `driver/tls.rs:1018,804` | Non-blocking poll like the Unix lane, or small worker pool; small internal poll grain |
| 10 | Med-High | Cross-shard inbound drain starves higher-id sources (shared budget, fixed order) | `threaded_multi_shard.rs:1000` | Round-robin start index / per-source budget floor |

## Cross-cutting truth-gaps

Fix these as patterns, not one-offs:

1. **"Bounded handle" ≠ "bounded work."** `pending_isolate_calls`,
   `in_flight_calls`, `translators` are uncapped Vecs while their delivery paths
   (reverse terminal queue, body cap) are bounded. Same shape leaks a bridge slot
   (D1) and lets sqlite run +1 over `max_in_flight` (D4).
2. **Terminal error → timeout / wrong cause.** C1 (Full/Closed→Timeout across
   shards), D2 (S3 retryable throttle→Fatal), D3 (reqwest "timeout" means cancel,
   others mean "work continues, late result counted"), C5/G1 (late-reply cause
   degrades). The shared vocabulary is not enforced across bridges.
3. **Flat-Vec linear scan on the hot path.** I1/I2/I3/I6/I7 are one anti-pattern.
   The author already fixed one instance (`dispatch.rs:1548` partition rewrite) but
   not the per-completion paths.
4. **Stream scope escalated to connection scope.** B5/B6 turn a single misbehaving
   stream into a connection-wide GOAWAY that kills every concurrent stream.
5. **Protocol rule assumed, not enforced.** A1 (WS fragmentation), B1/B3/B4
   (padding FC, `TE` value, empty `:path`), F2 (fsync ≠ durable on macOS).
6. **Safety rail with a side door.** H4 (`pub runtime_internal` re-wraps `noop()`
   into a `RequestEffect`, defeating the must-answer rail the compile-fail test
   claims to guard), E1/E2 (containment), G1/G3 (proof harness can claim coverage
   it did not deliver).

## Findings

### Track A — HTTP/1, chunked, WebSocket parsing

#### A1 — Unfragmented data frame accepted mid-fragmentation corrupts a later message
- Severity: Medium · Confidence: High · LLM-style: Y
- `tina-http/src/connection.rs:1616` (arm `0x1 | 0x2 if frame.fin`), interacting with `:1630-1655`
- Rule: RFC 6455 §5.4 — once a fragmented message is open, only continuation (0x0) or control frames are legal until FIN; a new data frame is a protocol error.
- Bug: the `0x1 | 0x2 if frame.fin` arm delivers immediately without checking `ws.fragmented_message`. A new FIN message is delivered standalone; the half-built fragment is left in place. A later continuation frame appends to the stale fragment, silently merging/mis-attributing two messages instead of failing the connection.
- Repro: write `[text FIN=0 "hel"]`, then `[text FIN=1 "XYZ"]`, then `[cont FIN=1 "lo"]`; assert the server protocol-closes. Today it delivers "XYZ" then "hello".
- Fix: in that arm, if `fragmented_message.is_some()` return `websocket_protocol_close(ProtocolError)`.

#### A2 — `Transfer-Encoding: identity` / empty silently treated as length-framed 0-body
- Severity: Low · Confidence: High · LLM-style: Y
- `tina-http/src/parse.rs:258-275`
- Rule: RFC 7230 §3.3.1 removed `identity`; TE present without final `chunked` should not become a 0-length body.
- Bug: `identity`/empty TE yields `chunked=false, unsupported=false`; with no Content-Length the body parses as 0 and any sent body is orphaned. Low because this server never proxies upstream.
- Fix: any TE whose final coding is not `chunked` (incl. `identity`/empty) → `UnsupportedTransferEncoding`.

#### A3 — Client request-line built from unvalidated path → CRLF injection
- Severity: Low · Confidence: Medium · LLM-style: partial
- `tina-http/src/parse.rs:412`; path set by `request_builder.rs:48`
- Bug: header *values* are CRLF-validated by the `http` crate, but the request target is a plain `String` written verbatim by `encode_request_internal`. A `\r\n` in the path injects a header line on the wire.
- Fix: validate the path in the builder (reject bytes `<0x21`/`>0x7e`, at minimum CR/LF/space/NUL).

#### A4 — Chunked size-line buffer truncation silently drops bytes (latent)
- Severity: Low · Confidence: Medium · LLM-style: Y
- `tina-http/src/chunked_decoder.rs:260` (`take = i.min(size_buf.len() - size_len)`)
- Bug: when a partial size line nearly fills the 64-byte buffer and the next feed's CRLF is past the remaining space, mid bytes are dropped while `consumed = i + 2`. No divergent-decode exploit was constructed (oversized hex already errors; only extension bytes truncate), so latent.
- Fix: if `i > space_left`, return `BadChunkSize` instead of truncating.

### Track B — HTTP/2 and gRPC protocol law

#### B1 ✓ — DATA-frame padding excluded from flow-control accounting (window leak / stall)
- Severity: High · Confidence: High · LLM-style: Y
- `tina-http/src/http2.rs:1195-1306`; credit return `:1462-1469`, `:2105-2110`
- Rule: RFC 9113 §6.9.1 — the entire DATA payload, incl. Pad Length and Padding, is flow-controlled.
- Bug: `data_payload(&frame)` strips padding before `len`; every window decrement and every WINDOW_UPDATE credit uses unpadded length. The peer charged its send window for the full padded frame, so its window monotonically shrinks and the stream/connection silently stalls on padded uploads.
- Repro: small connection window; several padded DATA frames (tiny content, large pad); assert returned WINDOW_UPDATE sums to full padded bytes. Today it sums to unpadded only.
- Fix: charge/credit windows on `fc_len = frame.payload.len()`; deliver/buffer on the unpadded `payload`.

#### B2 — Parked responses not re-flushed after a SETTINGS window increase (liveness)
- Severity: Medium-High · Confidence: High · LLM-style: Y
- `tina-http/src/http2.rs:992-1012`, `:1024-1083` vs `:1349-1369` (only WINDOW_UPDATE flushes)
- Rule: RFC 9113 §6.9.2 — a SETTINGS_INITIAL_WINDOW_SIZE increase can unblock a previously-blocked send.
- Bug: `apply_setting` adjusts `send_window` but neither `handle_settings` nor the SETTINGS arm calls `flush_pending_responses`. A response parked on a 0 window stays parked forever if the peer enlarges the window via SETTINGS only.
- Fix: after applying settings + ACK, run `flush_pending_responses` + `push_ready_response_pulls` (thread `effects` in as the WINDOW_UPDATE arm does).

#### B3 — `TE` header not validated (forbidden connection-specific header)
- Severity: Medium · Confidence: High · LLM-style: Y
- `tina-http/src/http2.rs:518-523`
- Rule: RFC 9113 §8.2.2 — `TE` MUST NOT carry any value other than `trailers`.
- Bug: the forbidden set omits `te`, so `te: gzip`/`chunked` is accepted and forwarded, contradicting the strict handling of the other connection headers and weakening the gRPC `te: trailers` contract.
- Fix: after the forbidden-name check, reject `te` whose value `!= "trailers"`.

#### B4 — Empty `:path` accepted for http/https requests
- Severity: Low-Medium · Confidence: Medium · LLM-style: Y
- `tina-http/src/http2.rs:484-489`, `:2278-2295`
- Rule: RFC 9113 §8.3.1 — `:path` MUST NOT be empty for http/https (except OPTIONS `*`).
- Bug: `validate_request_headers` only checks `.is_none()`, never emptiness; `:path ""` dispatches to the router.
- Fix: reject empty path for non-asterisk forms.

#### B5 — Stream-level flow-control overrun escalated to a connection GOAWAY
- Severity: Low · Confidence: Medium · LLM-style: Y
- `tina-http/src/http2.rs:1239-1250`
- Rule: RFC 9113 §6.9.1 — a stream flow-control violation should be a stream error (RST_STREAM FLOW_CONTROL_ERROR), not connection-fatal.
- Bug: stream-window overrun returns `Err(FlowControl)`, which GOAWAYs the whole connection and kills every concurrent stream.
- Fix: for the stream-window branch, emit `rst_stream_frame(id, FLOW_CONTROL_ERROR)`, drop the stream, `return Ok(())`.

#### B6 — Zero-increment WINDOW_UPDATE on a stream treated as a connection error
- Severity: Low · Confidence: Medium · LLM-style: Y
- `tina-http/src/http2.rs:1360-1362`
- Rule: RFC 7540 §6.9 — zero increment on stream 0 is a connection error; on a stream it is a stream error.
- Bug: any zero increment returns `Err(WindowOverflow)`, tearing down the whole connection.
- Fix: branch on `stream_id == 0`; otherwise RST_STREAM(PROTOCOL_ERROR), keep the connection.

Deliberate-by-design (not bugs): non-default HEADER_TABLE_SIZE rejected (tested specimen choice); standalone/unfinished CONTINUATION → connection error (documented fail-closed); gRPC `te: trailers` not required (spec SHOULD).

### Track C — runtime calls, cross-shard delivery, fairness

#### C1 ✓ — Cross-shard terminal reply silently dropped; caller's typed outcome degrades to `Timeout`
- Severity: High · Confidence: High · LLM-style: Y
- `threaded_multi_shard.rs:1025` (`let _ = route_remote(outbound)`), `multi_shard.rs:326-349` (`let _ = enqueue_remote_envelope`), `dispatch.rs:695-712,805-823`
- Invariant: every call settles exactly once with a typed terminal cause; Full/Closed/Rejected/timeout/cancel never silently converted.
- Bug: a terminal `CallReply` (a real `Replied`, or `Full`/`Closed` from `harvest_remote_send`) travels back over the bounded reverse terminal queue (`shard_pair_capacity`, default 64). When that queue is full the reply is discarded (`let _ =`). The requester's `pending_isolate_calls` entry (an uncapped `Vec`) is never settled and instead times out — losing the true cause.
- Why: uncapped pending population vs bounded reverse queue. A fan-in worker with >64 outstanding calls from one shard overflows the reverse queue in one drain window.
- Repro: two shards, `shard_pair_capacity: 1`; B replies to N>1 calls from A in one window; assert every caller observes its `Replied`/`Full`, not `Timeout`. B's trace shows `CallReplyRejected{ReplyPathFull}` with no caller settlement.
- Fix: never `let _=` a terminal `CallReply`. Re-buffer overflow terminals in a per-pair holding area drained next step, or reserve a reply slot at call admission; at minimum convert a persistent reverse-full into a typed `Closed` for the caller.

#### C2 — Stale "load-bearing assertion" comment on `take_by_local_call_id`
- Severity: Medium · Confidence: High · LLM-style: Y
- `tina-runtime/src/deferred.rs:65-78`; contradicted by `dispatch.rs:407-419` (builds `DeferredRouting::Remote`)
- Bug: the comment claims every promoted slot is `Local` and "the assertion makes that invariant load-bearing", but `Remote` slots exist and no assertion is present. `take_by_local_call_id` silently skips `Remote` slots — safe today only by shard-ownership accident. A future change that trusts the comment leaks a never-closed remote deferred slot.
- Fix: add the `debug_assert!(matches!(routing, Local))` the comment claims, or rewrite the comment to state the real (shard-ownership) reason and assert `Local` on the matched record.

#### C3 — Ordinary cross-shard sends starved when terminal replies fill the drain budget
- Severity: Medium · Confidence: Medium · LLM-style: Y
- `threaded_multi_shard.rs:939-954`
- Bug: `ordinary_budget = budget - terminal_delivered`; a sustained terminal flood drives `ordinary_budget` to 0, so ordinary inbound sends get zero service that pass (local commands still poll). 
- Fix: give the ordinary lane a guaranteed floor (split the budget or `max(1, budget/2)` per lane).

#### C4 — Caller-mailbox-full at delivery removes the pending call but drops the continuation
- Severity: Low · Confidence: Medium · LLM-style: N (consistent, traced design)
- `dispatch.rs:1716-1751`, `:1459-1492`, `:1218-1248`
- Bug: when the outcome can't enqueue into the requester's own full mailbox, the pending call is already removed, so it is "settled" in bookkeeping but the caller never sees the outcome — only a `CallCompletionRejected{MailboxFull}` trace. Differs from Full/Closed/Timeout which the caller does observe.
- Fix: document as a distinct terminal class, or hold the outcome for redelivery next step.

#### C5 — Late-reply cause attribution degrades to `NoPendingCall` after ring eviction
- Severity: Low · Confidence: High · LLM-style: N (bounded with a counter)
- `dispatch.rs:1588-1601` (`CANCELLED_CALL_RING_CAPACITY`)
- Bug: beyond ring capacity, a late reply for an evicted call_id is classified `NoPendingCall` instead of `CallerCancelled`/`CallerTimedOut`/etc. Best-effort by design.
- Fix: document the degradation, or size the ring relative to the (now unbounded) pending population.

### Track D — bridges and external work

#### D1 — rpc-tokio shim mailbox overflow permanently hangs awaiter and leaks admission slot
- Severity: High · Confidence: High · LLM-style: Y
- `tina-rpc-tokio/src/lib.rs:351-356` (shim sized `bridge.max_in_flight * 2`), `:502-535` (`rx.await`, no fallback timeout)
- Invariant: every call settles once; bounded means bounded.
- Bug: `ClientResultMsg` notifications are bounded by the underlying `Client.max_in_flight` (default 64), and connection close fans out one per pending request in a single turn. If `Client.max_in_flight > 2 * bridge.max_in_flight`, the shim mailbox overflows; the runtime drops the overflowed `Effect::Send`, so a live call's notification never arrives. The awaiter is parked on `rx.await` with no tokio-level timeout → hangs forever, slot leaked.
- Repro: `Client.max_in_flight=64`, `BridgeClient.max_in_flight=4` (shim=8); cancel-and-readmit bursts to push >8 in-flight, then force connection close (>8 notifications in one turn); assert a live awaiter resolves and `available_slots()` returns to max.
- Fix: wrap `rx.await` in `tokio::time::timeout` keyed to the deadline (release the slot on that path); or require the Client's `max_in_flight` at `BridgeClient::new` and size the shim to `client + bridge`.

#### D2 — S3 worker collapses every SDK error to `SdkUnknown`; throttles/not-founds lose their typed cause
- Severity: Medium · Confidence: High · LLM-style: Y
- `tina-aws-bridge/src/worker.rs:592,605,627,640`; classifier `classifier.rs:65`
- Bug: every S3 error maps to `S3Error::Sdk(_) → Fatal(SdkUnknown)`. The other four AWS services classify per-variant. A transient S3 throttle (503 SlowDown) surviving the SDK retry budget is presented as Fatal (do-not-retry), so a caller keyed on `is_retryable()` won't retry transient pressure.
- Fix: add `Throttled`/`NotFound`/`AccessDenied` S3 variants and classify per-operation like the Dynamo worker; throttle → `Retryable(ServiceThrottled)`.

#### D3 — Reqwest bridge has a different timeout/late-result/cancel semantic than every other bridge
- Severity: Medium · Confidence: High · LLM-style: Y
- `tina-reqwest-bridge/src/worker.rs:505-539` vs sqlx/aws/sqlite keep-slot-until-terminal
- Bug: on timeout, reqwest `abort()`s and frees the slot with no late-result accounting and no `ExternalWorkMayContinue`. The other bridges keep the slot leased until physical terminal and count late results. Same word ("timeout") means "we cancelled" here and "still running, will count late" elsewhere — cross-bridge metrics mis-attribute.
- Fix: align with the other bridges (track the aborted attempt as a late terminal) or document reqwest's timeout as a genuine cancel in the shared vocabulary.

#### D4 — Sqlite bridge drops in-flight gauge to 0 on timeout while the worker thread still runs; allows +1 over cap
- Severity: Low · Confidence: High · LLM-style: Y
- `tina-sqlite-bridge/src/worker.rs:419-431` vs `sync_channel(1)` at `:260`, admit `:359`
- Bug: timeout sets `set_in_flight(0)` and drops the slot while the blocking thread still executes; the next admit passes `in_flight.is_some()` and `try_send`s into the now-free channel buffer → up to two physical operations while `max_in_flight=1`. Gauge lies.
- Fix: keep the abandoned slot leased (like sqlx) until the worker thread reports terminal.

#### D5 — Reqwest retry-on-IO can duplicate a non-idempotent POST that already succeeded server-side
- Severity: Low (documented user responsibility) · Confidence: High · LLM-style: partial
- `tina-reqwest-bridge/src/worker.rs:633-656,724-730,894-919`
- Bug: with `on_reqwest_io: true`, a transport error during the response *body read* (after the server committed the POST) is retryable → re-sends, duplicating the effect. Connect-class and post-send IO errors are not distinguished.
- Fix: only retry connect-class IO errors, or surface a "may have duplicated" warning. (Idempotency is currently the user's documented promise.)

### Track E — resource ownership and drop paths

#### E1 — Shutdown joiner spawn-failure leaks every worker thread and reports a false "Closed"
- Severity: Medium · Confidence: High · LLM-style: Y
- `tina-runtime/src/shutdown.rs:261-291` (`ensure_joiner_started`)
- Bug: handles are `take()`n into `joinable` and moved into the spawn closure. `Builder::spawn` drops the closure on `Err`, detaching the threads. The fallback re-`take()`s from state (already `None`), so it joins nothing and caches a `Closed` report with no events. Every worker is leaked and the report lies. Triggered under FD/thread-limit exhaustion.
- Fix: do not pre-take before spawn; build `joinable` inside the closure, or run `joiner_main` inline with the same `joinable` on the `Err` branch.

#### E2 ✓ — Restart-factory panic escapes the supervised boundary and crashes the shard
- Severity: Medium-High · Confidence: High (uncaught) / Medium (intended contract) · LLM-style: Y
- `dispatch.rs:2239` (`recipe.create`), run loop `threaded.rs:1437`; handler turns are wrapped at `dispatch.rs:244`, the restart path is not.
- Bug: `recipe.create()` runs user factory closures at restart time outside any `catch_unwind`. A panic there unwinds out of `step()` and kills the shard worker thread — every isolate on the shard, all their leases/in-flight calls. The original child panic was supervised; the restart is not.
- Repro: a restart factory that panics on the 2nd invocation; trigger a child panic; assert the shard survives with a skip event.
- Fix: wrap `recipe.create` in `catch_unwind(AssertUnwindSafe(..))`; on panic emit `RestartChildSkipped{FactoryPanicked}` and leave the slot stopped (mirror the `NotRestartable` path).

Verified sound (suspicions cleared, covering tests cited): `PendingCancelableTicket` / `CallGroup` / `KeyedLimit` / `LocalPermitGate` ABA via monotonic generations; `WorkerPool` cancel-race recovery; `DeferredScopedCall::try_admit` rollback; `ConcurrencyPermit`/`SharedLease` drop; `SharedCapacityScope` CAS; restart entry-index stability; `gc_stopped_entries` refusal.

### Track F — persistence, process, filesystem, signals, TLS

#### F1 ✓ — Torn write to journal index sidecar permanently bricks all future appends
- Severity: High · Confidence: High · LLM-style: Y
- `tina-runtime/src/persistence.rs:206-230` (`load_journal_last_index`), `:177-183`, `:143`
- Bug: `store_journal_last_index` opens the `.idx` with `truncate(true)` then writes 24 bytes + fsync. A crash mid-write leaves a short file. `load_journal_last_index` does `read_exact(...).map_err(|_| CorruptRecord)?` and `if magic != .. { return Err(CorruptRecord) }`, so every subsequent append returns `Err(CorruptRecord)` even though the journal file itself is valid. The idx is rewritten on every append — the most frequently torn file.
- Repro: append; truncate `<p>.idx` to 4 bytes; append again → currently `Err(CorruptRecord)`, should fall back to replay.
- Fix: treat short/bad-magic idx as `Ok(None)` (replay fallback); also write the idx crash-atomically (tmp + rename).

#### F2 — macOS `file_fsync` rail does not use `F_FULLFSYNC`
- Severity: Medium · Confidence: High · LLM-style: Y
- `vendor-betelgeuse/io/darwin.rs:872` (`libc::fsync`); rail `call/files.rs:59`
- Bug: macOS `fsync(2)` only flushes to the drive cache; `F_FULLFSYNC` is required for stable media. The user `file_fsync` rail reports success without durability. (The persistence snapshot/journal path uses std `sync_all`, which does issue `F_FULLFSYNC` — only the user rail is affected.)
- Fix: in the darwin `Fsync` arm, prefer `fcntl(fd, F_FULLFSYNC)`, fall back to `fsync` on `ENOTSUP`.

#### F3 — PID-reuse race killing the process group after a reaped child
- Severity: Medium · Confidence: Medium · LLM-style: Y
- `tina-runtime/src/driver/process.rs:356-373`, leader reaped `:289-294`
- Bug: child is spawned in its own group (pgid == pid). After `try_wait()` reaps the leader, its pid is free for reuse. `process_exited` then `kill -KILL -<pgid>` (== old pid) on stdout/stderr truncation, possibly signalling an unrelated recycled group.
- Fix: kill the group before reaping the leader (while the pid is reserved), or retain a pidfd/group handle.

#### F4 — Single TLS worker: a quiet/malicious peer head-of-line-blocks all TLS work for the full I/O timeout
- Severity: Medium (High in practice — slowloris DoS) · Confidence: High · LLM-style: Y
- `tina-runtime/src/driver/tls.rs:1018-1048`, serial worker `:804-815`; `tls_io_timeout` default 30 s (`listener_tls.rs:46`)
- Bug: the TLS lane has one worker thread executing serially; `read_tls`/`write_tls`/`accept_tls` block it for up to the per-call timeout. The driver synthesizes a logical `Timeout` at the deadline but the thread stays physically blocked, so no other stream's TLS work and no new accept runs. A peer that completes the handshake then sends nothing stalls the whole HTTPS listener for 30 s.
- Fix: non-blocking reads/writes with short polls (mirror the Unix lane), or a small TLS worker pool; at minimum a small internal poll grain so the worker yields between streams.

#### F5 — `kill_and_reap` performs an unbounded blocking `child.wait()`
- Severity: Low-Medium · Confidence: Medium · LLM-style: Y
- `tina-runtime/src/driver/process.rs:316`
- Bug: after SIGKILL, the blocking `child.wait()` has no deadline. If reaping blocks (D-state child, or a kill that didn't land), the process worker wedges; on Drop the thread is detached and leaks (`worker_held`).
- Fix: bound the post-kill reap with a `try_wait()` deadline loop; surface `KillUncertain` if it doesn't reap in budget.

TLS verification is **not** weakened: no `dangerous()`/no-verify config, empty roots fail closed, real hostname check. Snapshot commit ordering (write→fsync→rename→dir-fsync), decode bounds, journal replay tail/checksum all correct.

### Track G — determinism, simulation, proof harness

#### G1 — `sweep_seeds` doesn't enforce the materialized case matches the swept seed
- Severity: Medium · Confidence: High · LLM-style: Y
- `tina-sim/src/dst/sweep.rs:113-129`
- Bug: reports `failing_seed = seed` (loop var) but `failing_case = make_case(seed)`, with no check that `make_case(seed).seed == seed`. A `make_case` that forgets to thread its argument yields "found bug at seed 743 after 1000 seeds" while every run used seed 0 — the harness asserts coverage it never delivered.
- Fix: `debug_assert_eq!(case.seed, seed, ...)` inside the loop (also in `discover_constants`).

#### G2 — `fault_selector` collapses all fault streams to `seed % modulus` at ordinal 0
- Severity: Low · Confidence: High · LLM-style: Y
- `tina-sim/src/sim_impl.rs:5633-5639`
- Bug: `if ordinal == 0 { return seed % modulus; }` short-circuits before mixing `tag`, so the first timer/TCP/reorder fault decisions are perfectly correlated. Determinism is intact, but seed-sweep cross-product coverage is quietly narrower than it looks.
- Fix: drop the special case; always mix tag + ordinal through `splitmix64`.

#### G3 — Buffered observer silently drops events; a hash/invariant over a buffered capture lies
- Severity: Low (Medium if wired to a hashing collector) · Confidence: Medium · LLM-style: N
- `tina-runtime/src/observer.rs:82-91`; interacts with `tina-proof-harness/src/live_replay.rs:94-102`
- Bug: `BufferedTraceObserver::on_event` `try_send`s and counts drops on Full. If used to feed a hashing/`InvariantSuite` collector, dropped events create id gaps → spurious `events_are_monotonic` violation and a hash that depends on buffer pressure.
- Fix: document that buffered observers must not feed proof collectors, or assert `dropped_count() == 0` before trusting the hash.

The core hash/replay chain is honest: `stable_trace_hash` is order-sensitive and really compared against pinned constants; no wall-clock leakage; shared `IdSource` across shards; seed-derived faults.

### Track H — macros and public API contracts

#### H1 — Unhygienic hardcoded `msg` collides with the user's call binding in split-service `handle_call`
- Severity: Medium · Confidence: High · LLM-style: Y
- `tina-macros/src/lib.rs:312-316`
- Bug: split `handle_call` is generated with a hardcoded `msg` parameter; `validate_call_handler` lets the user name the call arg anything. Naming it `msg` yields two `msg` params → E0415 inside generated code.
- Fix: use a hygienic mixed-site ident (`__tina_service_message`) for the synthesized parameter.

#### H2 — `#[deny(unused_variables)]` on generated `handle_call` hard-errors an unused request payload
- Severity: Medium · Confidence: High · LLM-style: Y
- `tina-macros/src/lib.rs:309`
- Bug: the blanket deny covers the spliced body and the `Request(#request_name)` binding. A handler answering the caller without reading a unit/marker request gets a hard compile error.
- Fix: drop the blanket deny (the `RequestEffect` linear type already enforces answering), or scope it to the call binding only.

#### H3 — RPC client builders have no guard against trait-arg names colliding with reserved params
- Severity: Medium · Confidence: High · LLM-style: Y
- `tina-rpc-macros/src/lib.rs:561-566`
- Bug: generated `*_request` fns append literal `deadline`/`correlator`/`reply_to`/`max_payload` params; a trait method arg with any of those names → E0415.
- Fix: detect collisions in `extract_method` and emit a spanned diagnostic ("argument name `reply_to` is reserved").

#### H4 — Public `runtime_internal` escape hatch defeats the split-service safety rail; compile-fail coverage stale
- Severity: Low-Medium · Confidence: High · LLM-style: Y
- `tina/src/lib.rs:418-425`; rail test `tina-runtime/tests/safety_rails_compile_fail/split_request_forged_effect.rs`
- Bug: `runtime_internal::request_effect_from_consumed_effect::<I>(noop())` is fully `pub` (doc-hidden) and re-wraps `noop()` into a `RequestEffect`, letting app code never answer the caller — the exact bad state the rail claims to forbid. The fixture pins only the private constructor.
- Fix: gate `runtime_internal` behind a sealed runtime-only witness, or move the helper behind it; add a foreign-crate compile-fail fixture.

#### H5 — `tina_runtime::isolate` generates `::tina`-rooted paths
- Severity: Low · Confidence: Medium · LLM-style: Y
- `tina-macros/src/lib.rs:202-205`
- Bug: only `call` uses `::tina_runtime`; everything else roots at `::tina`. A crate depending only on `tina-runtime` gets `unresolved import ::tina`. Works in-tree because examples also depend on `tina`.
- Fix: default `tina_crate` to `::tina_runtime` for the `runtime_isolate` entry point (re-export needed items), or document the `tina` requirement.

#### H6 — `isolate_types!` mixes `::std` and `::core` `Infallible` paths
- Severity: Low · Confidence: High · LLM-style: Y
- `tina/src/lib.rs:272,309,311`
- Bug: same macro uses `::std::convert::Infallible` and `::core::convert::Infallible` in different arms; latent no_std blocker, contradicts the proc-macro path.
- Fix: use `::core::convert::Infallible` everywhere.

### Track I — performance as correctness

#### I1 ✓ — Per-completion O(N) scan over all registered isolates
- Severity: High · Confidence: High · LLM-style: Y
- `dispatch.rs:1189,1432,1688,1852`; `entries: Vec<RegisteredEntry>` (`lib.rs:316`)
- Bug: every isolate-call completion, observed-send delivery, and cancel delivery does `entries.iter().position(|e| e.id == .. && e.generation == ..)`. One isolate per connection ⇒ O(N) per I/O completion ⇒ super-linear collapse under C10k.
- Repro: K idle isolates + 1 busy caller; replies/sec must stay flat as K→10⁴ (it grows linearly).
- Fix: `HashMap<IsolateId, usize>` beside `entries`, updated on register/stop; keep the generation check after lookup.

#### I2 ✓ — Per-driver-completion O(K) scans + O(K) removes (O(K²) to drain)
- Severity: High · Confidence: High · LLM-style: Y
- `dispatch.rs:1764` (`deliver_completion`), `:1632`, `:1078`, `:149`
- Bug: `in_flight_calls.iter().position` then `.remove(idx)`, and the same for `translators` — both flat Vecs keyed by call_id. Lookup O(K), `Vec::remove` shift O(K). The author already fixed the analogous owner-cancel path to a single-pass partition (`:1548`) but not these.
- Fix: key both by `HashMap<CallId, _>` (or merge translator into the in-flight entry); `swap_remove`.

#### I3 ✓ — `harvest_isolate_call_timeouts` full O(n) scan every step, O(k·n) on expiry
- Severity: High · Confidence: High · LLM-style: Y
- `dispatch.rs:1495` (called every `step_with_remote`, `:195`)
- Bug: `while let Some(index) = pending.iter().enumerate().filter(deadline<=now).min_by(deadline).map(index) { remove(index) }`. With n pending and 0 expired, a full scan every step; with k expired, O(k·n) re-scans + O(n) remove shifts. `pending_isolate_calls` uncapped.
- Repro: park 10⁴ far-future calls, loop `step()` with no traffic; CPU per empty step grows with n.
- Fix: `BinaryHeap`/`BTreeMap` keyed by `(deadline, insertion_order)`, pop only expired; or cache `earliest_deadline` and early-return.

#### I4 — `drain_remote_inbound` per-source starvation under cross-shard flood
- Severity: Medium-High · Confidence: High · LLM-style: Y
- `threaded_multi_shard.rs:1000` (receivers ordered by ascending source ShardId)
- Bug: a single shared `remote_inbound_drain_budget` (default 64) is consumed in fixed source order; the lowest-id flooder drains its budget every pass, starving higher-id sources' sends *and terminal replies* (compounds C1).
- Fix: round-robin start index per pass, or per-source budget floor.

#### I5 — Fixed 1 ms `thread::sleep` in the busy worker loop caps shard throughput
- Severity: Medium · Confidence: Medium-High · LLM-style: Y
- `threaded_multi_shard.rs:986`
- Bug: after a productive iteration the loop unconditionally sleeps 1 ms; `step()` delivers ≤1 message per isolate per round, so a backlogged shard advances at ~1000 rounds/s regardless of pending work and idle CPU.
- Fix: sleep only when an iteration did not fully consume available work; keep looping (or `yield_now`/`spin_loop`) while `delivered>0`; reserve `recv_timeout` for the truly-idle branch.

#### I6 — `WaitList`/`GuardedPendingReplies` O(capacity) per admission (multiple full scans)
- Severity: Medium · Confidence: High · LLM-style: Y
- `tina-runtime/src/wait_list.rs:391`, mirror `guarded_pending.rs:413`
- Bug: each `park` runs `sweep()` + `count_for_key()` + `len()` + `store_entry()` position scan — four O(cap) passes. A stampede coalescer sized for thousands admits each caller with thousands of comparisons ⇒ O(cap²) on the shard turn.
- Fix: free-slot stack for O(1) `store_entry`; incremental `live_count`; `HashMap<K,usize>` per-key counts; lazy sweep.

#### I7 — `WorkerPool::alloc_waiter_slot` / `live_waiter_count` O(max_waiters) per acquire
- Severity: Low-Medium · Confidence: High · LLM-style: Y
- `tina-runtime/src/pool.rs:342,280`, swept every turn `:865,881`
- Bug: parking acquires scan the slab for count and first-free; smaller blast radius than I1–I3 because pools are usually small.
- Fix: free-slot stack + incremental live counter (as I6).

Cleared (not bugs): SPSC mailbox (bounded ring, correct close-vs-producer race), `observer.rs` (documents the sync-on-shard hazard, ships a bounded buffered adapter), `host_burst.rs` (100µs polled wait, not a busy-spin), chunked decoder bounds, deterministic multi-shard queues, streaming pull backpressure, body_metrics (benign TOCTOU under the one-per-shard single-threaded model).

## Invariants violated

- Every call settles exactly once with a typed terminal cause — C1, C4.
- Full/Closed/Rejected/timeout/cancellation never silently converted — C1, D2, D3.
- Bounded capacity bounds the real thing, not a handle — C1, D1, D4, I1–I3.
- Shutdown eventually settles and tells the truth — E1, E2, F5.
- Protocol headers / body lengths tell the truth — B1, B3, B4, A1, A2.
- Replay / proof actually proves what it claims — G1, G3.
- One traffic class cannot starve another — C3, I4, F4.

## Areas needing deeper review

- `admission.rs` (recent rate-policy work, large) — no dedicated Track-I pass; per-request rate-limiter structures may add more hot-path scans.
- `tina-rpc/src/connection.rs` server side — terminal-reply / reverse-queue saturation (Track C class), not audited.
- Linux io_uring `IORING_OP_FSYNC` vs `FSYNC_DATASYNC` flag usage (only the submit side confirmed).
- `local_system.rs` shutdown ordering, `registration.rs` restart-path capacity — spot-checked only.
- Loom linearization of `tina-mailbox-spsc` `close()` vs racing `try_send` — claimed, not re-verified.
- Cross-shard delivery to a stopped target isolate; sim-vs-live settlement-order diff.

## Suggested fuzz / property / integration tests

- Property: cross-shard fan-in with outstanding calls > `shard_pair_capacity` ⇒ every caller settles non-`Timeout` when the worker actually replied (fails today — C1).
- Bench: K idle isolates + 1 busy caller, replies/sec flat as K→10⁴ (fails — I1); drain 10⁵ queued messages on an idle core, CPU-bound not 100 s (fails — I5).
- Unit: torn `.idx` (truncate to 4 bytes) ⇒ next append succeeds via replay (fails — F1); padded h2 DATA ⇒ returned WINDOW_UPDATE sums to full padded bytes (fails — B1).
- Trybuild pass: call arg named `msg` (H1); RPC trait arg named `deadline` (H3); handler ignoring its request payload (H2). Trybuild fail: `runtime_internal::request_effect_from_consumed_effect(noop())` from a foreign crate must not compile (H4).
- Integration: two HTTPS conns, one silent peer ⇒ the other still progresses within bounded time (fails — F4); restart factory that panics ⇒ shard survives with a skip event (fails — E2).
- Harness: `sweep_seeds` rejects `make_case(seed).seed != seed` (G1); buffered observer at capacity 1 into an InvariantSuite refuses to treat a gapped trace as passing (G3).

## Track coverage map

A → A1–A4 · B → B1–B6 · C → C1–C5 · D → D1–D5 · E → E1–E2 · F → F1–F5 ·
G → G1–G3 · H → H1–H6 · I → I1–I7. Every track produced findings; E was the
cleanest (most primitives verified sound).

## Verification

Verification target: current `main` @ `a0c2003` after PR #144 landed. This is an
append-only check of the findings above against the current tree, not a fix pass.

| Finding | Status | Current severity | Verification note |
|---|---|---|---|
| A1 | CONFIRMED | Medium | `tina-http/src/connection.rs:1616` still delivers `0x1 \| 0x2 if frame.fin` without checking `ws.fragmented_message`; continuation handling at `:1630-1655` still appends to the old fragment. |
| A2 | CONFIRMED | Low | `tina-http/src/parse.rs:258-274` still treats empty / `identity` transfer-encoding tokens as ignored, so TE present without final `chunked` can become a 0-body parse. |
| A3 | CONFIRMED | Low | `tina-http/src/parse.rs:410-415` still writes `request.path.as_bytes()` directly into the request line. Path construction is still plain string based. |
| A4 | CONFIRMED | Low | `tina-http/src/chunked_decoder.rs:255-264` still caps copied size-line bytes with `i.min(space)` and then consumes `i + 2`, so excess extension bytes can be dropped. |
| B1 | CONFIRMED | High | HTTP/2 moved into modules, but `tina-http/src/http2/server.rs:757-759` still uses `data_payload(&frame)?.len()` for flow-control length; WINDOW_UPDATE credit at `:1023-1031` still uses delivered body bytes. |
| B2 | CONFIRMED | Medium-High | `tina-http/src/http2/server.rs:600-604` ACKs SETTINGS after `apply_setting`; `SETTINGS_INITIAL_WINDOW_SIZE` updates stream windows at `:620-633`, but no response flush runs there. |
| B3 | CONFIRMED | Medium | `tina-http/src/http2/headers.rs:127-132` still omits `te` from the forbidden-header rule and has no value check for `te: trailers`. |
| B4 | CONFIRMED | Low-Medium | `tina-http/src/http2/headers.rs:237-253` checks `path.is_none()` but not empty path; `add_header` stores `:path` as-is at `:93-98`. |
| B5 | CONFIRMED | Low | Stream receive-window overrun in `tina-http/src/http2/server.rs:801-811` still returns `Err(Http2ProtocolError::FlowControl)`, which the caller maps to connection-level teardown. |
| B6 | CONFIRMED | Low | `tina-http/src/http2/server.rs:921-924` still treats any zero WINDOW_UPDATE increment as `WindowOverflow` before branching on stream id. |
| C1 | CONFIRMED | High | `tina-runtime/src/threaded_multi_shard.rs:1022-1025` and `tina-sim/src/multi_shard.rs:339-349` still drop failed terminal reroutes with `let _ = ...`; `pending_isolate_calls` remains an uncapped Vec. |
| C2 | CONFIRMED | Medium | `tina-runtime/src/deferred.rs:65-77` still says the assertion is load-bearing but contains no assertion; it filters Local slots only. |
| C3 | CONFIRMED | Medium | `tina-runtime/src/threaded_multi_shard.rs:939-954` still subtracts terminal delivery from the entire ordinary budget, allowing ordinary remote sends to get zero budget. |
| C4 | NEEDS-DISCUSSION | Low | Code still removes pending before requester-mailbox enqueue in `tina-runtime/src/dispatch.rs:1632-1650` then can reject completion later. This is traced, but the caller-visible terminal story is still weaker than `Full/Closed/Timeout`. |
| C5 | NEEDS-DISCUSSION | Low | Bounded cause ring remains best-effort; late cause degrades after eviction. Current code around `dispatch.rs:1588-1601` still uses the bounded recent-cancel ring. Decide whether doc-only is enough. |
| D1 | CONFIRMED | High | `tina-rpc-tokio/src/lib.rs:351-356` still sizes the shim mailbox as `bridge.max_in_flight * 2`, and `:502-516` still awaits `rx` without a deadline backstop. |
| D2 | CONFIRMED | Medium | S3 still maps SDK failures to stringly `S3Error::Sdk` at `tina-aws-bridge/src/worker.rs:592,605,627,640`; per-variant retry classification is not present. |
| D3 | NEEDS-DISCUSSION | Medium | Reqwest still aborts on timeout at `tina-reqwest-bridge/src/worker.rs:505-539` and frees the slot; this may be intended as true cancel, but the cross-bridge timeout vocabulary still differs. |
| D4 | CONFIRMED | Low | SQLite timeout still sets `metrics.set_in_flight(0)` and drops `self.in_flight` at `tina-sqlite-bridge/src/worker.rs:415-431` while the blocking worker may still be running. |
| D5 | NEEDS-DISCUSSION | Low | Reqwest retry policy still treats request IO as retryable by configured policy; because idempotency is documented caller responsibility, fix may be docs/classifier tightening rather than code. |
| E1 | CONFIRMED | Medium | `tina-runtime/src/shutdown.rs:261-290` still takes worker handles before `thread::Builder::spawn`; if spawn fails, the moved closure is dropped and the fallback re-takes empty handles. |
| E2 | CONFIRMED | Medium-High | `tina-runtime/src/dispatch.rs:2239` still calls `recipe.create(self, parent)` without `catch_unwind`; handler-turn panic containment does not cover restart factory creation. |
| F1 | CONFIRMED | High | `tina-runtime/src/persistence.rs:206-218` still maps short read or bad magic in the `.idx` sidecar to `CorruptRecord`, bricking later appends instead of replay fallback. |
| F2 | CONFIRMED | Medium | `vendor-betelgeuse/io/darwin.rs:867-878` still implements the user `Fsync` rail with `libc::fsync`, not `F_FULLFSYNC` fallback. |
| F3 | CONFIRMED | Medium | Process-driver group kill/reap ordering still allows post-reap group signalling in `tina-runtime/src/driver/process.rs`; needs a focused fix/proof. |
| F4 | CONFIRMED | Medium-High | TLS lane is still a single serial worker and read/write/accept calls can block for the configured timeout; needs nonblocking/pool work, not a doc tweak. |
| F5 | CONFIRMED | Low-Medium | `kill_and_reap` still has an unbounded blocking `child.wait()` path after kill in `tina-runtime/src/driver/process.rs`. |
| G1 | CONFIRMED | Medium | `tina-sim/src/dst/sweep.rs:113-125` still records loop `seed` separately from `case.seed` and does not assert the materialized case matches the swept seed. |
| G2 | CONFIRMED | Low | `tina-sim/src/sim_impl.rs:5653-5658` still returns `seed % modulus` for `ordinal == 0`, bypassing tag mixing for first decisions. |
| G3 | NEEDS-DISCUSSION | Low-Medium | `tina-runtime/src/observer.rs:82-89` still drops buffered events on full; `tina-proof-harness/src/live_replay.rs:93-101` can hash a captured subset unless callers check drops. Decide API guard vs stronger docs. |
| H1 | CONFIRMED | Medium | `tina-macros/src/lib.rs:310-315` still synthesizes a hardcoded `msg` parameter in split-service `handle_call`. |
| H2 | CONFIRMED | Medium | `tina-macros/src/lib.rs:307-318` still applies `#[deny(unused_variables)]` around generated split `handle_call`, including user request bindings. |
| H3 | CONFIRMED | Medium | `tina-rpc-macros/src/lib.rs:561-566` still appends reserved parameter names (`deadline`, `correlator`, `reply_to`, `max_payload`) without visible collision diagnostics. |
| H4 | CONFIRMED | Medium | `tina/src/lib.rs:332-425` still exposes `pub mod runtime_internal` and `request_effect_from_consumed_effect` publicly, so foreign crates can bypass the must-answer rail. |
| H5 | CONFIRMED | Low | The proc macro still defaults `tina_crate` to `::tina` at `tina-macros/src/lib.rs:202-205`; runtime-only consumers need an explicit answer. |
| H6 | CONFIRMED | Low | `tina/src/lib.rs:272,290,309,311` still mixes `::std::convert::Infallible` and `::core::convert::Infallible` in `isolate_types!`. |
| I1 | CONFIRMED | High | `tina-runtime/src/dispatch.rs:1189,1432,1688,1852` still uses `entries.iter().position(...)` on completion delivery paths. |
| I2 | CONFIRMED | High | `tina-runtime/src/dispatch.rs:1764-1777` still does two linear `position` scans and `Vec::remove` for driver completions. |
| I3 | CONFIRMED | High | `tina-runtime/src/dispatch.rs:1495-1508` still scans all pending calls, finds min expired deadline, and `remove`s in a loop. |
| I4 | CONFIRMED | Medium-High | `tina-runtime/src/threaded_multi_shard.rs:1000-1033` still drains source receivers in fixed order under one shared budget. |
| I5 | CONFIRMED | Medium | `tina-runtime/src/threaded_multi_shard.rs:975-987` still sleeps 1 ms after productive work when any local/remote/in-flight activity exists. |
| I6 | CONFIRMED | Medium | `tina-runtime/src/wait_list.rs:390-410` still performs sweep/count/len/store scans per park; `guarded_pending.rs` mirrors the same shape. |
| I7 | CONFIRMED | Low-Medium | `tina-runtime/src/pool.rs:280-282,342-348,865-881` still scans waiter slabs for live count/free slots on acquire/report paths. |

Verification conclusion: most findings remain live on current main. The
`NEEDS-DISCUSSION` entries are not disproven; they are design-policy choices where
the current code is real but the fix may be vocabulary/docs/API rather than a
straight bug patch. The first implementation wave should still start with C1/F1
and the I1-I3 runtime table work.

## Fix wave — 2026-05-21 (append-only)

Each finding below either has a fix PR open against `main` or an explicit
deferral with the exact reason. PR numbers are the fix branches, not the
original review PR.

Earlier wave (pre-existing PRs, ranges confirmed): A1 #161, A2 #162, A3 #163,
A4 #164, B1 #151, B2 #159, B3 #160, B4 #165, B5 #166, B6 #167, C1 #145, C2 #173,
C3 #158, D1 #150, D2 #168, D3 #174, D4 #169, D5 #175, E2 #152, F1 #146, F2 #176,
F4 #153, G1 #170, H1 #177, I1 #147, I2 #148, I3 #149, I4 #154, I5 #155, I6 #156,
I7 #157.

This wave (red-PR repairs + remaining uncovered):

- **D1 #150** — was red on Ubuntu+macOS (`E0433 cannot find select in tokio`):
  enabled the tokio `macros` feature for the bridge backstop `select!`.
- **E2 #152** — was red (`E0004` non-exhaustive match): added the
  `RestartSkippedReason::FactoryPanicked` arm in `tina-tracing`.
- **I5 #155** — was red (`ordinary_remote_throughput_still_progresses` 396 vs
  500): the unthrottled producer overran the default 64-deep cross-shard pair
  queue into typed `SendRejected{Full}`. Sized the test's `shard_pair_capacity`
  to the finite burst; product Full semantics unchanged.
- **I7 #157** — was red (`clippy::items_after_test_module`): moved the new
  tests module after the trailing `Isolate` impl.
- **G2 #171** — was red (pinned-seed determinism): re-pinned the seeds in
  `faulted_replay.rs`, `io_simulation.rs`, and `multishard_dispatcher.rs` to
  exercise the same property under the splitmix64-mixed selector. `make verify`
  green on the branch.
- **H2 #180** — dropped `#[deny(unused_variables)]` on the generated
  split-service `handle_call`; the `RequestEffect` linear type already enforces
  answering. Regression fixture added.
- **H3 #181** — reject reserved arg names (`deadline`/`correlator`/`reply_to`/
  `max_payload`) in service trait methods with a spanned diagnostic; trybuild
  fixture pins it.
- **H4 #182** — made `runtime_internal::request_effect_from_consumed_effect`
  an `unsafe fn` (the only stable cross-crate barrier; not memory safety),
  routed the runtime's 8 callers through one `pub(crate)` safe wrapper, added a
  foreign-crate compile-fail fixture. Flagged as a design tradeoff in the PR.
- **H5 #179** — documented that `tina_runtime::isolate` roots at `::tina`
  (needs the `tina` dep / `tina_crate = ...`). The re-export-and-reroot
  alternative was deferred as a public-API decision.
- **H6 #178** — `isolate_types!` now uses `::core::convert::Infallible`
  everywhere.
- **E1 #183** — shutdown joiner no longer pre-takes worker handles before the
  joiner spawn, so a failed spawn cannot drop+leak them and lie with an empty
  `Closed` report. Handle-taking moved into `run_joiner`; unit test with a
  forced-spawn-failure seam.
- **F5 #184** — bounded the post-kill `child.wait()` in `kill_and_reap` with a
  `try_wait` deadline loop (`KillUncertain` on expiry).

### Deferred to maintainer design decision (exact reasons)

- **C4 (Low) — DEFER, accept-as-traced.** Caller-mailbox-full at completion
  delivery already emits the distinct typed terminal
  `CallCompletionRejected{MailboxFull}` (`dispatch.rs:1226-1235`); the pending
  call is removed, so the caller does not observe it as a reply. The finding's
  two options are (a) document this distinct terminal class — already true in
  code — or (b) hold the outcome for redelivery next step. (b) is a real
  behavior change: it re-buffers settled outcomes and reopens the
  "bounded handle ≠ bounded work" question the wave otherwise closed. Recommend
  accepting (a) as the contract; not patched here because there is no bug to
  fix, only a policy to ratify.
- **C5 (Low) — DEFER, accept-as-documented.** The recently-cancelled cause ring
  is already documented and bounded with a visible eviction counter
  (`cancelled_call_cause_evictions`, `dispatch.rs:1584-1595`). Past capacity a
  late cause degrades to `NoPendingCall` — best-effort by design. The only
  "fix" is to grow the ring relative to the pending population, which
  reintroduces unbounded growth and contradicts the bounded-capacity
  discipline. Recommend documenting the degradation (done in code) and keeping
  the bound.
- **F3 (Medium) — DEFER, needs platform-specific design.** The post-reap
  process-group kill in `process_exited` (`driver/process.rs:371`) signals
  `-pgid` where `pgid` equals the already-reaped leader's pid, which the OS may
  have recycled. A correct fix must keep the pid reserved across the group
  kill, which needs either `waitpid(WNOWAIT)` + raw `libc` (Unix-only,
  `unsafe`, and it must not double-reap `std::process::Child`) or a pidfd
  (Linux-only — and `pidfd_send_signal` targets a single pid, not a group, so
  it does not directly solve group kill). `tina-runtime` is `deny(unsafe_code)`
  outside `pool`. The timeout/cancel path (`kill_and_reap`) is already
  kill-before-reap and unaffected; the race is confined to the truncation
  cleanup branch after a natural exit. Not patched here because a correct fix
  is a deliberate platform/unsafe design decision, not a minimal change, and a
  half-fix risks double-reap. Recommend a focused Linux `WNOWAIT` design pass.
