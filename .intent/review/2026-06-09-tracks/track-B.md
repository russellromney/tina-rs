# Track B — HTTP/2 and gRPC protocol law (2026-06-09)

Scope: `tina-http/src/http2/{server,client,frame,headers,errors}.rs`,
`tina-http/src/grpc.rs`. Oracle: RFC 9113 (RFC 7540), gRPC-over-HTTP/2 spec.
HEAD `0cd6a31` (= origin/main). Source treated read-only.

Carve-outs honored (covered by sibling agents, not reviewed here): (a) client
connection-window credit when a streamed response is dropped never-pulled; (b)
server/client inbound frame-size validation vs advertised MAX_FRAME_SIZE.

## Summary by boundary

The 2026-06-08 cluster (SP1/SP2/SP3, B1–B7, I10) is genuinely fixed on this HEAD
— re-verified, see "Disproven / confirmed-fixed" below. The recurring prior
pattern, *cap/length honored on one path and ignored on its symmetric twin*,
still produces two live findings here, both on the **server's inbound DATA
path** where the *client* handles the same case correctly:

- **B8-residual (Medium).** DATA on a stream the server already closed-and-removed
  (post-response, or racing the server's own RST_STREAM) is escalated to a
  connection GOAWAY instead of a stream-scoped RST_STREAM(STREAM_CLOSED). The
  prior B8 fix only covered the still-in-table half-closed-remote case. The
  client's DATA path handles the removed-stream case correctly — exact twin
  asymmetry.
- **B11 (Medium).** Every "reset the stream, keep the connection" path in the
  server's `handle_data` returns without crediting the connection-level
  flow-control window for the DATA bytes the peer already debited. The accepted-
  DATA path credits the connection window; the rejected-DATA paths do not. Same
  family as the (fixed) B1 padding leak: a peer's connection send window leaks
  toward zero and eventually stalls every stream.

Lower-tier residuals from the prior review remain un-actioned (B9 PRIORITY
length, B10 `:authority`/Host + gRPC `te`), recorded below.

---

## Live findings

### B8-residual — DATA on a closed/removed stream GOAWAYs the whole connection
1. **Severity:** Medium
2. **Confidence:** High
3. `tina-http/src/http2/server.rs:1016-1018` (`handle_data`); error→GOAWAY mapping
   at `:643-656`.
4. **Rule:** RFC 9113 §5.1 — after a stream is closed, a peer may still have
   frames in flight that were sent before it learned of the close. "An endpoint
   that receives any frame other than PRIORITY after receiving a RST_STREAM MUST
   treat that as a stream error of type STREAM_CLOSED." A closed/half-closed
   stream receiving DATA is a **stream** error (RST_STREAM STREAM_CLOSED), not a
   **connection** error (GOAWAY). Only DATA on an *idle* id (never opened,
   `id > highest_client_stream_id`) is a connection PROTOCOL_ERROR.
5. **Bug:** `find_stream(stream_id).ok_or(StreamClosed)?` collapses "stream was
   open, now removed from the table" and "id never opened" into the same
   `StreamClosed` error, which `handle_read` turns into a connection GOAWAY +
   `closing_after_write`. The in-table fix the prior review landed only covers
   streams still present and marked `request_eof`/`state == Closed`
   (`:1019-1044`, which correctly RST). But every normal completion path removes
   the stream from the table: `send_pending_response` (`:1707-1708`),
   `handle_stream_chunk` Eof/GrpcStatus (`:1888`, `:1904`), and
   `reset_active_stream_for_protocol` (`:2003`) all call `remove_stream`. Once
   removed, the id is gone from `stream_index`, so a late DATA frame on it →
   GOAWAY. The `ActiveStream.reset` flag (`:303`, checked at `:1019`) is **dead
   code** — it is never set to `true` anywhere in the file (`grep` confirms zero
   assignments), so the intended "keep a reset stream in the table to absorb
   racing DATA" was never wired; that is the root cause.
6. **Why real:** Two benign races. (i) Server sends a complete response and
   removes the stream while the client is still uploading its request body
   (RFC 9113 §8.1 explicitly allows early responses); the client's in-flight
   DATA arrives → GOAWAY tears down every other multiplexed stream on the
   connection. (ii) Server RSTs a stream (flow-control error, body cap,
   protocol error) and removes it; the peer's DATA already on the wire arrives
   before it sees the RST → GOAWAY. Both turn a one-stream condition into a
   whole-connection kill.
7. **Repro / failing test:** Open stream 1 (HEADERS without END_STREAM), make the
   service reply a complete response so the stream is removed, then send a DATA
   frame on stream 1. Assert RST_STREAM(STREAM_CLOSED) on stream 1 and the
   connection stays up (no GOAWAY, `closing_after_write == false`). Today: GOAWAY.
8. **Fix:** Distinguish idle from closed at the lookup miss:
   ```rust
   let idx = match self.find_stream(stream_id) {
       Some(idx) => idx,
       None => {
           // Idle id (never opened) is a connection PROTOCOL_ERROR;
           // a closed/removed id is a stream error.
           if stream_id > self.highest_client_stream_id {
               return Err(Http2ProtocolError::StreamClosed); // -> GOAWAY
           }
           // Count the frame against the connection window the peer debited
           // (see B11), then RST the stream and keep the connection.
           self.add_connection_window_credit(flow_len);
           self.enqueue_frame(rst_stream_frame(stream_id, ERR_STREAM_CLOSED))?;
           self.emit_protocol_fact(effects, /* Http2StreamReset ... */);
           return Ok(());
       }
   };
   ```
   The client already does exactly this at `client.rs:2176-2182` (RST_STREAM +
   `Ok(())`); mirror it.
9. **LLM-style pattern:** Yes — lookup-miss collapsed to one cause; happy-path
   completion removes the table entry, and the error path never reasons about
   the just-removed id.

### B11 — server `handle_data` leaks connection flow-control window on every reset path
1. **Severity:** Medium
2. **Confidence:** Medium-High
3. `tina-http/src/http2/server.rs` — reset/early-return branches at `:1019-1031`
   (closed), `:1032-1044` (request_eof), `:1045-1067` (stream flow overrun),
   `:1097-1121` (body cap). Accepted-DATA credit (the path these omit) is
   `add_request_window_credit` + `maybe_flush_request_window_credit` at
   `:1122-1148`, `:2091-2103`, `:2135-2143`.
4. **Rule:** RFC 9113 §6.9.1 — the entire DATA frame counts against the
   connection flow-control window, and a receiver must return that credit even
   for a frame it discards/rejects on a stream it is closing, or the sender's
   connection send window is permanently consumed.
5. **Bug:** The connection recv-window check at `:1004` does **not** debit
   `self.recv_window`; debit happens only on the accepted path at `:1122`, paired
   with a credit-back. Every reset branch returns `Ok(())` *before* `:1122`,
   neither debiting nor crediting, and never emits a connection
   WINDOW_UPDATE(stream 0) for `flow_len`. The peer, however, debited its
   connection send window by `flow_len` when it sent the frame. So the peer's
   connection send window drops by `flow_len` per rejected DATA frame and is
   never returned; the server's own `recv_window` stays ~full and it never
   notices.
6. **Why real:** A benign-but-aggressive client that overshoots `max_body_bytes`
   on a few uploads (each → EnhanceYourCalm RST at `:1099`), or whose streams hit
   the per-stream window overrun (`:1055`), leaks up to `max_frame_size` (16 KiB
   default) of connection send window per event. On a long-lived multiplexed
   connection this accumulates until the peer's connection send window reaches 0
   and *all* subsequent streams stall — the same failure shape as the B1 padding
   leak the prior review fixed, on a different (reset) path.
7. **Repro / failing test:** Small `initial_connection_window`. Send N DATA frames
   that each exceed the stream's body cap (so each is reset). Assert the sum of
   emitted connection-level WINDOW_UPDATE(stream 0) increments equals the total
   DATA wire bytes the server consumed. Today the reset bytes contribute zero
   connection credit.
8. **Fix:** On every reset/early-return path that has passed the `:1004`
   connection-window check, return the connection credit for `flow_len`
   (enqueue `window_update_frame(0, flow_len)` or fold into
   `pending_recv_window_credit` + flush), exactly as the accepted path does. The
   stream-level credit is moot once the stream is reset, but the connection-level
   credit is not.
9. **LLM-style pattern:** Yes — flow-control accounting wired on the happy path;
   the reset/early-return branches "just return Ok" and silently skip the
   connection-window bookkeeping.

---

## Lower-tier residuals (from prior review, still live, recorded not re-filed)

- **B9 [Low] still present** — `server.rs:776-778` `handle_priority`: a PRIORITY
  frame with length != 5 returns `BadFrameLength` → connection GOAWAY. RFC 9113
  §6.3 makes a wrong-length PRIORITY a *stream* FRAME_SIZE_ERROR. (stream_id 0 →
  connection error is correctly kept.) Fix: RST_STREAM(FRAME_SIZE_ERROR) + Ok.
- **B10 [Low] still present** — `headers.rs:588` `validate_request_headers`:
  `has_authority = authority_non_empty || host_non_empty`; when both `:authority`
  and `Host` are present and disagree, RFC 9113 §8.3.1 wants rejection — not
  enforced. gRPC routes also never require `te: trailers` on admission
  (`grpc.rs` content-type check only). Both accept non-conformant peers; no
  smuggling because the path/method/scheme rules are still enforced.

---

## Disproven / confirmed-fixed on this HEAD (with proof)

- **SP1 (streamed response content-length truth)** — FIXED. Buffered client path
  checks `declared != response_body.len()` at `client.rs:2249-2252`; streamed
  client path compares `response_content_length` to a running
  `response_body_received` counter at `client.rs:1624-1635`
  (`complete_streaming_stream`), RST+ProtocolError on mismatch. Server streamed
  response: short-source (`server.rs:1866-1876`) and overrun
  (`server.rs:1793-1803`) both RST before delivering a clean End.
- **SP2 (client-streaming gRPC decode count cap)** — FIXED. `decode_streaming_body`
  errors `TooManyMessages` once `messages.len() >= limits.max_messages`
  (`grpc.rs:1732-1737`), before pushing — symmetric with the encode side
  (`:466-491`). `GrpcLimits.max_messages` is a real public field (`:48`).
- **SP3 (server initial SETTINGS)** — FIXED. `initial_settings_frame`
  (`server.rs:2227-2246`) advertises MAX_CONCURRENT_STREAMS, INITIAL_WINDOW_SIZE,
  MAX_FRAME_SIZE, ENABLE_PUSH=0 from config; sent on preface
  (`process_buffer:684`).
- **B1 (DATA padding counted toward flow control)** — FIXED. `data_payload_view` /
  `into_data_payload` return `flow_len = payload.len()` (full on-wire length
  incl. pad-length byte + padding); every server window check/debit uses
  `flow_len_i32` (`server.rs:1003-1004,1045,1122-1123`); client credits padding
  back at `client.rs:2198-2203`. Unit tests `into_data_payload_padded_*`
  (`frame.rs:303-336`).
- **B2 (SETTINGS INITIAL_WINDOW_SIZE re-flush)** — FIXED. `handle_settings` now
  takes `effects` and calls `flush_pending_responses` + `push_ready_response_pulls`
  after the ACK (`server.rs:805-807`); client `flush_outbound_data` at
  `client.rs:2035`.
- **B3 (`TE` value)** — FIXED. `headers.rs:363-365` rejects any `te` value other
  than `trailers`.
- **B4 (empty `:path`)** — FIXED. `is_valid_request_path("")` is false (no leading
  `/`) → InvalidPseudoHeaders (`headers.rs:585-587,603`). Test
  `request_path_pseudo_header_must_not_be_empty`.
- **B5 (stream-window overrun escalated to GOAWAY)** — FIXED.
  `server.rs:1045-1067` now RST_STREAM(FLOW_CONTROL_ERROR) +
  `reset_active_stream_for_protocol` + `Ok(())`.
- **B6 (zero-increment WINDOW_UPDATE on a stream)** — FIXED.
  `server.rs:1207-1224`: increment 0 on stream 0 → WindowOverflow (connection);
  on a non-zero stream → RST_STREAM(PROTOCOL_ERROR) + Ok.
- **B7 (buffered upload deadlock)** — FIXED. Buffered DATA credits flow per frame
  mid-upload (`server.rs:1140-1148`) via `add_request_window_credit` +
  `maybe_flush_request_window_credit`, mirroring the streaming path. Live test at
  `http2_live.rs:533`.
- **I10 (find_stream O(S))** — FIXED. Both server and client index streams by id
  in a `HashMap<u32, usize>` kept consistent across `swap_remove`
  (`server.rs:2268-2288`, `client.rs:1527-1547`).
- **Duplicate pseudo-headers / content-length** — correct. `add_header_with_storage`
  rejects a second `:method/:path/:scheme/:authority/:status`
  (`headers.rs:312-351`) and a second `content-length` (`:386-392`);
  pseudo-after-regular via `saw_regular` (`:309`).
- **Trailer validation** — correct. `validate_trailer_block` rejects pseudo-headers
  and `content-length` in trailers (`headers.rs:628-641`); client requires
  trailers to arrive with END_STREAM (`client.rs:2104-2107`).
- **CONTINUATION flood** — structurally impossible. HEADERS without END_HEADERS →
  `HpackUnsupported` (`server.rs:892-894`, `client.rs:2059-2061`); standalone
  CONTINUATION → `UnexpectedContinuation` (`server.rs:767`). No header-block
  buffering.
- **Stream-id monotonicity / even ids / id 0** — rejected
  (`server.rs:889-906`, `client.rs:2056-2058`).
- **WINDOW_UPDATE overflow past i32::MAX** — `add_window` returns WindowOverflow
  (`frame.rs:271-277`).
- **PING** — stream 0 + 8-byte payload enforced, ACK reflected, ACKs ignored
  (`server.rs:850-858`, `client.rs:2319-2341`).
- **gRPC request-body pull terminal causes (0cd6a31)** — correct.
  `classify_request_chunk` (`grpc.rs:1557-1583`) keeps Full→ResourceExhausted,
  Closed/Rejected→Cancelled, Timeout→DeadlineExceeded distinct; test
  `request_chunk_outcome_taxonomy_keeps_runtime_causes_distinct`. No new
  misclassification introduced.
- **gRPC message framing** — `decode_one_grpc_message` validates the 5-byte header
  and length-prefix against the body slice before allocation, rejects compressed
  (`grpc.rs:1794-1827`); `encode_grpc_message_into` caps at
  `min(max_message_bytes, u32::MAX)` before reserving (`:1770-1792`).

---

## Areas wanting deeper review (out of this track's time budget)

- The server's connection-window auto-credit design (`recv_window` credited on
  consume, only batched WINDOW_UPDATE to the wire): confirm B11's leak does not
  also exist on the *streaming* request path's reset (`reset_active_stream` mid
  request body) — same `add_request_window_credit` omission likely applies.
- `flush_response_stream` accumulates `response_pending_data` without a cap when
  the send window is parked (`server.rs:1930-1964`); resident outbound bytes for
  a streamed response may not be bounded if the source outruns flow control.
  Likely a separate (Track-I-ish) memory concern, not protocol law.

## Suggested tests
- Server: complete a response on stream 1 (stream removed), then send DATA on
  stream 1 → assert RST_STREAM(STREAM_CLOSED) + connection alive (B8-residual);
  send DATA on a never-opened higher id → assert GOAWAY (idle path stays correct).
- Server: drive N body-cap-exceeding DATA frames on a small
  `initial_connection_window`; assert summed connection WINDOW_UPDATE(0)
  increments equal the consumed wire bytes (B11).
