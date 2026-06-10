# Second pass — truth-gap review (2026-06-09)

HEAD `0cd6a31` (= origin/main), read-only. Job: find what the nine first-pass
tracks missed, aiming at the seams BETWEEN tracks. Carve-outs honored: HTTP/2
client window credit for dropped streamed responses; inbound frame-size vs
advertised MAX_FRAME_SIZE; tina-tracing install/live shutdown flush; non-unix
process supervision; per-variant AWS slot audit. Known dedups acknowledged:
C-1 ≡ I-NEW-2, C-5 ≡ I-NEW-1.

Status: COMPLETE (focused pass).

## Summary

The first-pass tracks each audited their own surface; the gap they all left
is the **streaming-body seam between the HTTP servers and the runtime
isolate lifecycle**. The HTTP/2 server's stream-teardown paths were written
per-site instead of through one funnel, and every per-site copy forgot a
different obligation: telling the response source (SP-A), returning
accepted-but-unconsumed flow-control credit (SP-C), transitioning state
after a failed flush (SP-E), and emitting the reset fact (SP-F). One shared
`reset/teardown` helper fixes four findings at once. Independent of that
cluster: the success-path source lifecycle has no owner at all (SP-B), and
the gRPC streaming client invents `Ok` from a missing `grpc-status` (SP-D)
— the exact "field promising truth never checked" shape, with the honest
unary twin sitting next to it.

## Seam targets investigated

1. HTTP server → runtime call terminal-cause truth (Full/Closed/Timeout → HTTP status) — clean
2. HTTP/2 streamed-response outbound buffering vs flow control ("bounded" truth) — clean (disproven below)
3. Stream/connection teardown → response-source lifecycle — **SP-A, SP-B**
4. Streaming-upload flow-credit accounting at stream death — **SP-C** (completes track B's flagged-but-unverified thread)
5. RPC wire: duplicate/malformed request ids, terminal-cause → wire frames — clean
6. WebSocket server outbound backpressure — clean
7. gRPC response-side status truth — **SP-D**
8. Keepalive `must_retire` vs pool capacity — clean (disproven below)
9. Outbound-queue-pressure flush failure — **SP-E**
10. Protocol-fact truth for server-initiated resets — **SP-F**
11. Sim-vs-live socket semantics — recorded observation (below), not filed

## Ranked fixes

1. **SP-A** (Med-High) — cancel response sources on every HTTP/2 server
   teardown/reset path; one `reset_stream_with_cleanup` helper.
2. **SP-C** (Med-High) — return dropped `request_chunks` flow credit on
   stream removal; flush connection-level credit with zero live streams.
3. **SP-D** (Med) — missing `grpc-status` at END_STREAM is not `Ok`.
4. **SP-B** (Med) — give the streamed-response source a success-path
   terminator (or self-stop on Eof) and name the owner in the contract.
5. **SP-E** (Low-Med) — flush-failure path must transition the stream.
6. **SP-F** (Low) — emit reset facts from the three silent sites.

## Findings

### SP-A — HTTP/2 server never cancels streaming response sources on connection teardown or mid-stream reset; source isolates stranded forever

1. **Severity:** Medium-High
2. **Confidence:** High (mechanism code-proven on every path; impact follows from the documented source contract)
3. **File:line:**
   - Connection teardown with live streams, no source cancel anywhere:
     `tina-http/src/http2/server.rs:550` (`Closed(_) => stop()`), `:549`
     (write error → `close_now`), `:636-638` (read EOF → `close_now`),
     `:643-656` (protocol error → GOAWAY + `closing_after_write`),
     `:618-632` (`begin_goaway_shutdown`), `:1246-1255` (rapid-reset GOAWAY).
     None iterate `self.streams` to cancel `response_source` /
     `response_pull_handle` / `pending_call`.
   - Mid-stream reset paths inside `handle_stream_chunk` that
     `remove_stream(...)` and **discard** the returned `ActiveStream` without
     `cancel_response_source`: overrun `:1797-1803`, response body cap
     `:1812-1829`, short-source `:1866-1876`, pull-terminal
     (`Full|Closed|Rejected|Timeout`) `:1918-1926`. Buffered-response cap in
     `enqueue_response` `:1481-1490` same shape (no source on that stream, but
     the removed stream's `pending_call` is also dropped unconsumed there).
   - Contrast the two paths that do it right: `handle_rst_stream:1291` and
     `reset_active_stream_for_protocol:2017` both call
     `cancel_response_source` (`:2021-2045`), which sends
     `ResponseChunkMsg::Cancel` and cancels the in-flight pull.
   - Contrast the **HTTP/1 server**, which cancels on every abandonment path:
     `connection.rs:486,512,524,1481,1495,1525,1551` plus the defensive cancel
     in `begin_close` (`:1612`).
   - Contrast the **HTTP/2 client**, which cancels its request-body source on
     `fail_stream` and all three bulk-teardown drains
     (`client.rs:2417,2472,2497,2670`).
4. **Violated invariant:** the source contract is explicit
   (`streaming.rs:66-69`): "The connection is abandoning the wire. The source
   should stop producing and release any owned resources." `scope.rs:243`
   re-states it: "the same `ResponseChunkMsg::Cancel` the connection sends
   when..." — a promise the HTTP/2 server keeps only for peer-RST and
   stream-level protocol errors, not for any connection-level death or any
   server-initiated mid-chunk reset.
5. **Concrete bug:** a streamed (or gRPC server-streaming) response whose
   connection dies — client disconnect (read EOF / write error: the **most
   common** end of a long download), connection protocol error, rapid-reset
   GOAWAY, graceful `Stop` — leaves every active stream's chunk-source isolate
   alive and waiting for a `Next` that never comes. `IterBodySource` (the
   blessed helper) stops **only** on `Cancel` (`streaming.rs:303,310`), so the
   isolate, its boxed iterator, and whatever the source owns (file handles,
   downstream slots per the contract wording) leak for runtime lifetime. The
   server-initiated mid-stream resets (overrun/cap/pull-timeout) strand the
   source the same way even while the connection stays healthy.
6. **Real-use scenario:** gRPC server-streaming subscriptions or large file
   downloads; every client that disconnects mid-body strands one source
   isolate. Sustained churn = unbounded isolate/memory growth on the serving
   shard (compounds with track C's C-3 if sources are spawned children).
7. **Failing test idea:** start an HTTP/2 streamed response from an
   `IterBodySource`-style source that records `Cancel`; sever the client TCP
   socket mid-body; assert the source receives `Cancel` (or stops) within a
   bound. Today it never does. Repeat for the body-cap reset path
   (`max_response_body_bytes` overrun) on an otherwise healthy connection.
8. **Fix sketch:** (a) in `handle_stream_chunk`'s four reset paths, take the
   removed stream mutably and call `self.cancel_response_source(...)` exactly
   as `handle_rst_stream` does; (b) add a `cancel_all_streams(&mut effects)`
   that drains `self.streams`, cancels each `pending_call`,
   `response_pull_handle`, and `response_source`, and call it from the
   write-error/read-EOF/GOAWAY/rapid-reset paths before `close_now`/`stop()`
   (the HTTP/1 `begin_close` defensive-cancel shape).
9. **LLM-pattern?** Yes — the cancel helper exists and is wired on the two
   paths someone thought about (peer RST, stream protocol error); every other
   abandonment path was added without re-asking "who tells the source". The
   correct twins (HTTP/1 server, HTTP/2 client) make the omission stark.

### SP-C — HTTP/2 server: accepted-but-unconsumed streaming upload bytes are debited from the connection recv window forever when the stream dies (ratchets to connection death)

1. **Severity:** Medium-High
2. **Confidence:** High (mechanism is unambiguous in code; completes the thread
   track B flagged as "wanting deeper review" but did not verify)
3. **File:line:** `tina-http/src/http2/server.rs:1122-1134` (accepted DATA
   debits `self.recv_window` and `streams[idx].recv_window`, then queues the
   bytes as `RequestDataChunk { flow_credit }`); credit returns **only** on
   consume at `:2070-2073` (`reply_pending_request_chunk` →
   `add_request_window_credit`). Stream-death sites that drop queued chunks
   with their un-returned `flow_credit`: `handle_rst_stream:1257`
   (`remove_stream` — client cancels the upload),
   `reset_active_stream_for_protocol:2003`, every `remove_stream` in
   `handle_stream_chunk`/`enqueue_response`/`handle_data` reset paths, and
   connection-lifetime stream teardown. No path sums the dropped chunks'
   `flow_credit` back into `recv_window` / `pending_recv_window_credit`.
4. **Violated invariant:** RFC 9113 §6.9.1 — bytes the receiver consumed (or
   will never consume) must be credited back, or the connection window
   permanently shrinks. Distinct from track B's B11: B11 is DATA **rejected at
   arrival** on reset paths (never debited, peer-side-only leak); this is DATA
   **accepted, debited, queued** and then dropped — it shrinks the *server's
   own* `recv_window` too, so the failure is server-visible and terminal.
5. **Concrete bug:** a streaming-dispatched request (`request_dispatched_streaming`)
   whose stream dies with chunks still queued — client RST mid-upload (user
   cancels), service slow to pull, server-side reset — leaks
   `sum(queued flow_credit)` from the connection recv window. Each cancelled
   upload can leak up to the whole stream window of in-flight bytes. After
   enough cancels, `self.recv_window` approaches 0; then any DATA frame hits
   the `:1004` check `recv_window < flow_len` → `Err(FlowControl)` →
   `handle_read:647-655` maps it to a **connection GOAWAY**
   (FLOW_CONTROL_ERROR) blamed on a peer that never violated anything.
   Before that point, the peer (whose own view of the connection send window
   matches the leak) simply stalls all uploads.
6. **Real-use scenario:** gRPC client-streaming / large uploads on long-lived
   multiplexed connections where clients cancel — the normal case for upload
   progress bars, retries, and deadline-cancelled RPCs. The connection
   degrades invisibly and then dies with a protocol-blame GOAWAY.
7. **Failing test idea:** small `initial_connection_window`; start a streaming
   upload, send W bytes (accepted, unconsumed), RST the stream; repeat until
   cumulative W exceeds the connection window; assert the connection still
   accepts a fresh upload (today: FlowControl GOAWAY) and that emitted
   WINDOW_UPDATE(0) credits equal total accepted bytes.
8. **Fix sketch:** in `remove_stream` (or each caller that can hold queued
   chunks), fold the removed stream's unconsumed credit back:
   `let leaked: usize = stream.request_chunks.iter().map(|c| c.flow_credit).sum();`
   then `recv_window += leaked` and add to `pending_recv_window_credit` +
   schedule a flush. Note the flush helper has its own zero-stream blind spot:
   `flush_deferred_request_window_credit:2201-2208` iterates live streams
   only, so connection-level pending credit with **zero** live streams is not
   flushed until a later request happens to trigger it — fix by flushing the
   connection-level credit unconditionally (WINDOW_UPDATE on stream 0 needs
   no live stream).
9. **LLM-pattern?** Yes — credit bookkeeping implemented on the consume path;
   every abandon path "just removes the stream". Same family as the fixed B1
   and the open B11; this is the third symmetric twin.

### SP-B — HTTP/1 success path never releases the response source either: completed streamed responses leak their source isolate by design-gap

1. **Severity:** Medium
2. **Confidence:** High (mechanism); Medium (whether some out-of-band owner is "supposed" to stop it — no such owner exists in docs or examples)
3. **File:line:** `tina-http/src/connection.rs:1360` (known-length complete:
   `self.stream_source = None`, no cancel), `:1567-1580` (`finish_stream_eof`:
   same), `tina-http/src/streaming.rs:283-313` (`IterBodySource` stops only on
   `Cancel`), `:201-206` (doc: "the source isolate is per-stream, not
   per-route" — i.e. one registered isolate per response).
4. **Violated invariant:** "bounded capacity means the real thing is bounded"
   — the per-stream source lifecycle has a start (register per response) and
   no end on the success path.
5. **Concrete bug:** when a streamed response completes normally (Eof, or
   known-length fully written), the connection silently drops its source
   reference. No `Cancel` is sent, the source never stops, and nothing tells
   the owning service the response finished so it could stop the source
   itself. With the documented per-stream `IterBodySource` pattern, every
   successful streamed response permanently leaks one registered isolate
   (mailbox + boxed iterator). The HTTP/2 server has the same success-path
   shape (`handle_stream_chunk` Eof/GrpcStatus arms remove the stream without
   cancel — the source did learn it sent Eof, but learning ≠ stopping).
6. **Real-use scenario:** any server streaming N responses leaks N isolates;
   `examples/specimen_http_body_streaming` exhibits it (sources registered
   per-route, single-use, never stopped) but only serves one request per
   source so it never shows.
7. **Failing test idea:** serve K streamed responses to completion on one
   runtime; assert registered-entry count returns to baseline. Fails today.
8. **Fix sketch:** send `Cancel` (or a new `Finished` variant) to the source
   on the success path too — duplicate cancels are documented harmless; or
   make `IterBodySource` `stop()` itself when replying `Eof` and document
   that custom sources must self-stop after Eof. Either way the contract in
   `streaming.rs` should name whose job source termination is on success.
9. **LLM-pattern?** No — design gap: the cancel path was built for
   abandonment, and nobody owned the success-path lifecycle.

### SP-D — gRPC streaming client synthesizes `Ok` when END_STREAM carries no `grpc-status`

1. **Severity:** Medium
2. **Confidence:** High (code + blessed consumption pattern); Medium on
   real-world frequency
3. **File:line:** `tina-http/src/grpc_client.rs:484-485`
   (`decode_stream_chunk`, `End` arm:
   `grpc_status_from_header_map(&trailers).unwrap_or_else(|| GrpcStatus::new(GrpcStatusCode::Ok))`);
   contrast the unary twin `finish_unary:325-335`, which maps a missing
   status on HTTP 200 to `Malformed(GrpcError::MissingTrailers)`.
4. **Violated invariant:** gRPC HTTP/2 transport mapping — a response that
   ends without a `grpc-status` is **not** OK; receivers must synthesize a
   non-OK status (grpc-go/grpc-java use Unknown/Internal "server closed the
   stream without sending trailers"). Status truth must not be invented from
   absence. Also the in-repo standard the unary path already sets.
5. **Concrete bug:** a server-streaming/bidi response whose stream ends with
   END_STREAM and no `grpc-status` (trailer-stripping L7 proxy, an HTTP/1
   hop, or a buggy server that half-closes after its last message) yields
   `GrpcStreamItem::Status(Ok)`. The doc comment hedges "the head may have
   carried it", but `decode_stream_chunk` cannot see the head, and the
   blessed consumption loop (`tina-http/tests/grpc_client_live.rs:434-441`,
   `collect_grpc_stream`) treats this item as the final truth. The
   `decoder.finish()` guard catches a mid-message cut, but a cut at a clean
   message boundary — exactly what a graceful-but-trailerless close produces
   — reads as a successful, complete stream when the server may have had
   more messages and a real status to send.
6. **Real-use scenario:** misconfigured proxies and connection draining are
   the classic producers of trailerless ends; the failure converts "stream
   died early" into "stream completed OK" — silent data truncation.
7. **Failing test idea:** drive `decode_stream_chunk` with
   `End { trailers: HeaderMap::new() }` after one complete message when the
   head carried no status; assert the item is a non-OK status (Unknown /
   Internal or a `Malformed(MissingTrailers)`), not `Status(Ok)`.
8. **Fix sketch:** make the default non-OK: synthesize
   `GrpcStatus::new(GrpcStatusCode::Unknown)` with a message naming the
   missing trailer, or return `Malformed(GrpcError::MissingTrailers)` to
   mirror unary. If trailers-only/head-carried status is the concern, thread
   the head status into the decode (e.g. `GrpcStreamDecoder` remembers
   `stream_head_status`) instead of guessing Ok.
9. **LLM-pattern?** Yes — `unwrap_or(Ok)` is the plausible-looking
   completion of a match; the sibling unary path got the honest treatment
   and the streaming twin didn't.

### SP-E — HTTP/2 server: streamed-response flush failure under outbound-queue pressure RSTs the wire but keeps the stream; stream (and source) can wedge until an unrelated peer event

1. **Severity:** Low-Medium
2. **Confidence:** High on the inconsistency; Medium on the wedge being
   reachable in practice (needs outbound queue at cap exactly when a chunk
   lands)
3. **File:line:** `tina-http/src/http2/server.rs:1841-1848` — on
   `flush_response_stream` error the code enqueues RST (itself a
   `let _ =`, which **also fails** when the cause was
   `ensure_outbound_slots` queue-cap, `:2254-2264`) and then leaves the
   stream in the table with its `response_pending_data` and
   `response_source` intact. No `remove_stream`, no
   `cancel_response_source`, no protocol fact.
4. **Violated invariants:** after sending RST_STREAM an endpoint must not
   treat the stream as live; and "every parked obligation has a retry edge".
   The only retry edges for parked response data are peer-driven
   (`flush_pending_responses` from SETTINGS `:805-807` / WINDOW_UPDATE
   `:2277`); `handle_wrote` (`:2164-2189`) drains the write queue but never
   re-attempts `flush_response_stream`.
5. **Concrete bug:** multiplexed connection with a slow socket fills
   `write_queue` to `connection_outbound_queue_capacity`; a response chunk
   lands → flush errors (`StreamLimitFull`) → RST enqueue fails too →
   nothing is sent and nothing is recorded. The queue then drains via
   `handle_wrote`, but the stream's pending data is never re-flushed and no
   next chunk is pulled (`response_pending_data` non-empty). If the peer has
   no reason to send WINDOW_UPDATE/SETTINGS (large initial window, it is
   *waiting for us*), the stream — and its source — hang until the service
   call timeout fires at the client.
6. **Failing test idea:** tiny `connection_outbound_queue_capacity`; park
   the transport write; deliver a chunk so flush hits the cap; then let the
   write queue drain with no peer frames; assert the stream completes (data
   eventually flushed or stream reset+source cancelled). Today neither
   happens until a peer event.
7. **Fix sketch:** either treat flush failure like the other reset paths
   (remove stream + cancel source + fact — consistent with `:1797-1829`),
   or add a `flush_pending_responses` pass to `handle_wrote` when the queue
   has drained (the locally-driven retry edge).
8. **LLM-pattern?** Yes — error branch returns the right *type* and looks
   handled (`RST + continue`), but the state machine isn't transitioned.

### SP-F — Three server-initiated HTTP/2 stream resets are invisible to the protocol-fact/trace surface

1. **Severity:** Low
2. **Confidence:** High
3. **File:line:** `tina-http/src/http2/server.rs:1797-1803` (response
   overrun), `:1866-1876` (short source), `:1918-1926` (pull terminal) —
   all `rst_stream_frame(...)` + `remove_stream(...)` with **no**
   `emit_protocol_fact(Http2StreamReset/Http2StreamClosed)`. Sibling reset
   sites emit facts: body-cap `:1821-1827`, buffered-response cap
   `:1486-1496`, DATA-on-closed `:1020-1029`, flow-overrun `:1055-1064`,
   peer RST `:1260-1276`.
4. **Violated invariant:** trace/proof truth — "the trace shows every
   wire-visible stream reset". A proof harness or scoped-request report
   consuming `Http2StreamReset` facts will under-count exactly the
   server-initiated truth-enforcement resets (the ones an operator most
   wants to see), and `report.closed_streams` disagrees with the fact
   stream.
5. **Fix sketch:** emit the same `Http2StreamReset` (+`Http2StreamClosed`)
   facts on the three paths; one helper `reset_stream_with_fact(...)` would
   also prevent the next omission (and is the natural place for SP-A's
   source cancel and SP-C's credit return).
6. **LLM-pattern?** Yes — fact emission copy-pasted per site instead of
   funneled; new sites forget it.

## Disproven suspicions (with proof)

- **HTTP/2 server unbounded `response_pending_data` while flow-blocked**
  (track B's "areas wanting deeper review" item): DISPROVEN. The next
  `Next` pull is only issued when `response_pending_data.is_empty()`
  (`server.rs:1853-1858`, and `push_ready_response_pulls:2148-2160` has the
  same predicate), so at most one source chunk is resident per stream while
  parked; cumulative bytes are additionally capped by
  `max_response_body_bytes` (`:1812`). Bounded.
- **HTTP/2 client request-body buffering unbounded:** DISPROVEN — same
  demand-driven shape: `pump_request_pulls` skips a stream with
  `has_outbound()` (`client.rs:1702-1712`), so one chunk per stream max.
- **RPC server duplicate `request_id`:** handled — `in_flight_ids` set,
  duplicate gets a typed `Decode` error frame with b"duplicate request_id"
  (`tina-rpc/src/connection.rs:593-607`). Late router replies after close
  are dropped deliberately (`:702-705`); `CallOutcome::Timeout` →
  no wire frame is a documented invariant ("client times out locally",
  `:770-774`), and the slot is freed — not a leak, not a cause blur
  (`Closed/Rejected → Internal` is the wire's vocabulary limit, recorded
  not filed).
- **HTTP server terminal-cause → status-code truth:** verified honest on
  both servers. HTTP/2: Full→503/RESOURCE_EXHAUSTED(grpc),
  Timeout→504/DEADLINE_EXCEEDED, Closed/Rejected→500/INTERNAL
  (`server.rs:1419-1449`); HTTP/1 buffered path same mapping
  (`connection.rs:2522-2526`). No Full↔Timeout conversion at this seam.
- **WebSocket server outbound backpressure:** bounded and typed —
  `admit_websocket_send` enforces frame slots + `max_queued_outbound_bytes`
  and surfaces `OutboundQueueFull`/`OutboundBytesFull` to the app via
  `SendOutcome` (`connection.rs:2095-2152`); unsolicited queueing closes
  with a pressure event (`websocket_pressure_then_close`, `:2176-2185`).
- **Keepalive `must_retire` shrinking pool capacity:** DISPROVEN — the
  connection isolate drops the bad transport and reconnects cold on the
  next request in the same slot (`keepalive.rs:17-20,824-868`); releasing
  `Retire` (capacity loss) is a separate, documented consumer choice. The
  A-F3 over-send retire therefore does not rot capacity.
- **HTTP/2 server deferred window credit lost when write queue is full:**
  partially disproven — credit is retained (`pending_recv_window_credit`)
  and re-flushed from `handle_wrote:2184` once the queue drains; live
  streams will carry it out. Residual zero-live-streams blind spot is
  folded into SP-C's fix note.
- **`HttpConnectionPool` slot wedge on dropped continuation:** DISPROVEN —
  the `Returned` continuation rides the non-droppable runtime-call
  continuation lane (D2 fix), and every `CallOutcome` arm resets
  `in_flight` and maps causes 1:1 (`tina-http/src/pool.rs:100-110`).
- **`reply_pending_request_chunk` oversized-chunk handling:** correct —
  splits at `request_stream_chunk_size`, re-queues the remainder with its
  flow credit, credits only the delivered prefix (`server.rs:2057-2073`).
