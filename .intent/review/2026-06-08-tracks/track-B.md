# Track B — HTTP/2 and gRPC protocol law (2026-06-08)

Scope: `tina-http/src/http2/{server,frame,headers,errors}.rs`, `tina-http/src/grpc.rs`,
`tina-rpc/src/frame.rs`. Oracle: RFC 7540 / RFC 9113, gRPC HTTP/2 spec. HEAD 49c3580.

## Summary by boundary

The six prior HTTP/2 findings (B1–B6) from the 2026-05-20 review are **all still
present** at HEAD. None were fixed; the module-split line numbers in that review's
verification table still match the current source. Track B re-confirms each with a
fresh line reference and adds sibling findings (B7–B10) found in the same scan.

The recurring pattern is *protocol rule assumed, not enforced*: padding is stripped
before it is counted, `:path`/`TE`/scheme rules are checked partially or not at all,
and stream-scoped faults are escalated to connection teardown.

---

## Re-verified prior findings (B1–B6)

### B1 ✓ STILL PRESENT — DATA padding excluded from flow-control accounting
- Severity High / Confidence High. LLM-pattern: yes (strip-then-measure).
- `tina-http/src/http2/server.rs:757-759`, credit at `:1024-1031` and `:1680-1687`.
- Rule: RFC 7540 §6.9.1 — the *entire* DATA frame payload, including the
  pad-length byte and all padding octets, counts against both stream and
  connection flow-control windows.
- Bug: `handle_data` computes `len = data_payload(&frame).len()`, which has padding
  already stripped (`frame.rs:150-162`). Every window check (`:760`, `:801`),
  every debit (`:867-868`), and every WINDOW_UPDATE credit
  (`dispatch_stream:1030-1031`, `reply_pending_request_chunk:1680-1687`) use the
  unpadded length. The peer debits its send window by the full padded frame, the
  server only credits the unpadded portion, so the peer's send window leaks
  downward and a padded upload silently stalls.
- Trigger: any client (Go grpc, curl, browsers under some configs) that sends
  padded DATA frames on an upload.
- Repro: small `initial_connection_window`; send N padded DATA frames (tiny
  content, large pad). Assert the sum of returned WINDOW_UPDATE increments equals
  the total padded bytes. Today it equals the unpadded total only.
- Fix: thread the on-the-wire DATA length (pad-length byte + content + padding)
  through flow control; keep `data_payload` only for what is handed to the body.
  e.g. add `data_frame_flow_len(&frame)` returning `frame.payload.len()` and use
  it for every window debit/credit, while delivering `data_payload`.

### B2 ✓ STILL PRESENT — parked responses not re-flushed after a SETTINGS window increase
- Severity Medium-High / Confidence High. LLM-pattern: yes (one resume site wired).
- `tina-http/src/http2/server.rs:586-605` (`handle_settings`) and
  `apply_setting` `SETTINGS_INITIAL_WINDOW_SIZE` arm `:620-634`.
- Rule: RFC 7540 §6.9.2 — a SETTINGS_INITIAL_WINDOW_SIZE change adjusts every open
  stream's send window; credit that newly opens must let blocked frames flow.
- Bug: `apply_setting` adjusts each `stream.send_window` by the delta but does not
  run `flush_pending_responses` / `push_ready_response_pulls`. Only the
  WINDOW_UPDATE arm (`handle_frame:558-562`) threads `effects` and flushes. A peer
  that opens credit by *raising the initial window via SETTINGS* (instead of
  WINDOW_UPDATE) never resumes a response parked in `pending_response`. Liveness
  hole. `handle_settings` doesn't even take `&mut effects`, so it structurally
  cannot resume work.
- Repro: park a buffered response on a zero send window; send a SETTINGS frame that
  raises INITIAL_WINDOW_SIZE above the body length; assert the DATA frames flush.
  Today nothing is written until an unrelated WINDOW_UPDATE arrives.
- Fix: give `handle_settings` an `effects: &mut Vec<Effect<Self>>`, and after the
  ACK call `self.flush_pending_responses(effects)?; self.push_ready_response_pulls(effects);`
  exactly as the WINDOW_UPDATE arm does.

### B3 ✓ STILL PRESENT — `TE` header not validated (forbidden connection-specific header)
- Severity Medium / Confidence High. LLM-pattern: yes (incomplete forbidden list).
- `tina-http/src/http2/headers.rs:127-132`.
- Rule: RFC 7540 §8.1.2.2 — `TE` MUST NOT appear with any value other than
  `trailers`; `Connection`, `Keep-Alive`, `Proxy-Connection`, `Transfer-Encoding`,
  `Upgrade` MUST be treated as malformed.
- Bug: the forbidden-name set omits `te` entirely. `te: gzip` (or any value) is
  accepted and forwarded to the handler. The connection-specific-header check
  never special-cases `te` for the `trailers`-only rule.
- Repro: decode a request header block containing `te: gzip`; assert
  `InvalidPseudoHeaders` / a malformed-request rejection. Today it decodes clean.
- Fix: in `add_header`, after the uppercase check, if `name == "te"` and the value
  (case-insensitive, trimmed) is not exactly `trailers`, return
  `Http2ProtocolError::InvalidPseudoHeaders`.

### B4 ✓ STILL PRESENT — empty `:path` accepted for http/https requests
- Severity Low-Medium / Confidence High. LLM-pattern: yes (presence check, not value check).
- `tina-http/src/http2/headers.rs:93-98` (stores `:path` as-is) and
  `validate_request_headers:237-253` (checks `path.is_none()`, not emptiness).
- Rule: RFC 7540 §8.1.2.3 — `:path` MUST NOT be empty for `http`/`https` URIs
  (the only exception is OPTIONS with `*`).
- Bug: an empty `:path` value parses to `Some("")` and passes validation, reaching
  the handler with an empty path.
- Repro: header block with `:method GET`, `:scheme http`, `:authority h`, `:path ""`;
  assert rejection. Today it dispatches.
- Fix: in `validate_request_headers`, reject when `:path` is empty unless
  (`:method` == OPTIONS and path == "*"). Keep `:scheme` presence as is.

### B5 ✓ STILL PRESENT — stream-level flow-control overrun escalated to connection GOAWAY
- Severity Low / Confidence High. LLM-pattern: yes (stream fault returned as connection error).
- `tina-http/src/http2/server.rs:801-811`.
- Rule: RFC 7540 §6.9 / §5.4.2 — a stream-level flow-control violation is a
  *stream* error (RST_STREAM with FLOW_CONTROL_ERROR), not a connection error.
- Bug: when `streams[idx].recv_window < len_i32`, `handle_data` returns
  `Err(Http2ProtocolError::FlowControl)`. `handle_read` maps that to a connection
  GOAWAY (`:503-512`), tearing down every other stream on the connection. One
  misbehaving stream kills the whole connection.
- Repro: open two streams; overrun one stream's receive window with the connection
  window still positive; assert the other stream survives and only the offending
  stream is RST. Today the connection GOAWAYs.
- Fix: in the stream-window branch, send `rst_stream_frame(id, ERR_FLOW_CONTROL_ERROR)`,
  emit the reset fact, `reset_active_stream_for_protocol`, and `return Ok(())`
  rather than `Err`.

### B6 ✓ STILL PRESENT — zero-increment WINDOW_UPDATE on a stream treated as connection error
- Severity Low / Confidence High. LLM-pattern: yes (one check before the scope branch).
- `tina-http/src/http2/server.rs:922-924`.
- Rule: RFC 7540 §6.9 — a WINDOW_UPDATE with a 0 increment on a *stream* is a
  *stream* error (PROTOCOL_ERROR → RST_STREAM); only on the connection (stream 0)
  is it a connection error.
- Bug: `handle_window_update` returns `Err(WindowOverflow)` for `increment == 0`
  before it branches on `frame.stream_id`. A zero stream WINDOW_UPDATE therefore
  becomes a connection GOAWAY.
- Repro: send WINDOW_UPDATE(stream=1, inc=0); assert RST_STREAM on stream 1, not
  GOAWAY. Today GOAWAY.
- Fix: move the `increment == 0` check inside the `stream_id == 0` branch as a
  connection error, and in the `else` branch RST the stream and return `Ok`.

---

## New sibling findings

### B7 — buffered request upload larger than the receive window deadlocks
- Severity Medium / Confidence Medium. LLM-pattern: yes ("known limitation" mirror of B2 on the inbound side).
- `tina-http/src/http2/server.rs:870-876` (buffering path) vs the only inbound
  WINDOW_UPDATE sites: `dispatch_stream:1024-1031` (fires at EOF) and
  `reply_pending_request_chunk:1680-1687` (streaming path only).
- Rule: a server that advertises `max_body_bytes` (default 1 MiB) but never
  replenishes the receive window mid-upload cannot actually receive a buffered
  body larger than `initial_connection_window` (default 65 535).
- Bug: for a non-gRPC (buffered) request, each DATA frame debits the connection and
  stream receive windows (`:867-868`) but no WINDOW_UPDATE is emitted until the
  request is dispatched at END_STREAM. Once 64 KiB has arrived the connection
  receive window is 0; the peer parks, the server waits for END_STREAM that can
  never come. Deadlock for buffered uploads in (64 KiB, 1 MiB].
- Repro: POST a 256 KiB body to a non-gRPC HTTP/2 service with default limits;
  assert the body is fully received. Today it stalls after ~64 KiB.
- Fix: credit and emit a WINDOW_UPDATE as buffered DATA is accepted (mirror the
  streaming path's `maybe_flush_request_window_credit`), or route buffered bodies
  through the same pending-credit accounting. At minimum, document that
  `max_body_bytes` cannot exceed `initial_*_window` for buffered routes.

### B8 — DATA on an idle (never-opened) stream id is not a clean connection PROTOCOL_ERROR
- Severity Low / Confidence Medium. LLM-pattern: yes (lookup-miss collapsed to one cause).
- `tina-http/src/http2/server.rs:772-774`.
- Rule: RFC 7540 §5.1 — DATA received for a stream in `idle` state (id never
  opened, e.g. higher than `highest_client_stream_id`) is a connection
  PROTOCOL_ERROR. DATA for a `closed` stream is STREAM_CLOSED.
- Bug: `find_stream(...).ok_or(StreamClosed)` collapses both "never opened" and
  "already closed and evicted" into `StreamClosed`. `StreamClosed` maps to a
  connection GOAWAY with PROTOCOL_ERROR code (`handle_read:507`), so the connection
  is torn down in both cases — but the wire code/classification does not match the
  state, and a legitimately-closed stream (peer DATA racing our RST) gets a
  connection GOAWAY instead of a stream-scoped RST_STREAM(STREAM_CLOSED). The flow
  window for the frame is also already checked (`:760`) but not debited before the
  early return, so a flood of DATA on closed/idle ids does not consume window — a
  minor accounting asymmetry vs. RFC 7540 §6.9 ("a receiver MUST count toward flow
  control even for streams it cannot accept").
- Repro: open stream 1, finish it; send DATA on stream 1 again with connection
  window positive; assert RST_STREAM(STREAM_CLOSED) on stream 1, connection stays
  up. Today: connection GOAWAY.
- Fix: distinguish idle (`id > highest_client_stream_id` and odd) → connection
  PROTOCOL_ERROR, vs closed/evicted → emit RST_STREAM(STREAM_CLOSED) and keep the
  connection. Debit flow control for the frame regardless, since the peer did.

### B9 — PRIORITY / RST_STREAM bad length should be stream FRAME_SIZE_ERROR, not connection
- Severity Low / Confidence Medium. LLM-pattern: yes (frame-size escalation).
- `tina-http/src/http2/server.rs:580-582` (PRIORITY) and `:939-941` (RST_STREAM).
- Rule: RFC 7540 §6.3 — a PRIORITY frame of length != 5 is a *stream*
  FRAME_SIZE_ERROR; §6.4 — RST_STREAM of length != 4 is a *connection* error
  (this one is correctly connection-scoped). PRIORITY is the misclassified one.
- Bug: `handle_priority` returns `BadFrameLength` for a wrong-size PRIORITY frame,
  which `handle_read` escalates to a connection GOAWAY. RFC wants RST_STREAM on
  that stream only.
- Repro: send PRIORITY(stream=1) with a 4-byte payload; assert RST_STREAM(stream 1,
  FRAME_SIZE_ERROR), connection survives. Today: GOAWAY.
- Fix: in `handle_priority`, on wrong length send `rst_stream_frame(id,
  ERR_FRAME_SIZE_ERROR)` and return `Ok(())`. (Stream-id 0 PRIORTY stays a
  connection error — that part is correct.)

### B10 — gRPC content-type accepted without verifying the HTTP/2 `TE: trailers` requirement; trailers-only and `:authority`/Host duplication unenforced
- Severity Low / Confidence Medium. LLM-pattern: yes (assumed-not-enforced).
- `tina-http/src/grpc.rs:189-196` (content-type), `headers.rs:241-249` (authority).
- Rule: gRPC over HTTP/2 requires `te: trailers` on requests and uses HTTP/2
  trailers for status; a server that does not require/enforce `te` will still
  "work" with compliant clients but accepts non-conformant ones. Separately,
  RFC 9113 §8.3.1: if `:authority` is present it is authoritative; a request with
  both `:authority` and `Host` that disagree should be rejected.
- Bug: nothing checks `te: trailers` is present for gRPC routes (only the blocking
  test client sends it). `validate_request_headers` treats `:authority` OR `Host`
  as interchangeable (`has_authority`) and never compares them when both are
  present, so a mismatched `:authority`/`Host` pair is accepted.
- Repro: send a gRPC request with no `te` header / with `:authority a` and
  `host: b`; assert rejection. Today both are accepted.
- Fix: require `te: trailers` on gRPC request admission; when both `:authority`
  and `Host` are present and non-equal, return `InvalidPseudoHeaders`.

---

## Disproven / already-correct (recorded with proof)

- **CONTINUATION flood**: `handle_frame:570-571` rejects any HEADERS without
  END_HEADERS (`handle_headers:665-667` → `HpackUnsupported`) and any standalone
  CONTINUATION (`UnexpectedContinuation`). No header-block continuation is ever
  buffered, so the CVE-2024-27316-style CONTINUATION-flood is structurally
  impossible here. Not a bug; it is a documented non-support.
- **Duplicate pseudo-headers**: `add_header:84-119` rejects a second `:method`,
  `:path`, `:scheme`, `:authority`, `:status` with `InvalidPseudoHeaders`.
  Pseudo-header-after-regular is rejected via `saw_regular` (`:80`). Test
  `pseudo_header_after_regular_header_is_rejected` covers it. Correct.
- **Duplicate / malformed content-length**: `add_header:138-145` fails closed on a
  second `content-length` (`saw_content_length`) and `parse_content_length:150-158`
  rejects empty/non-digit/overflow. END_STREAM-on-HEADERS with non-zero declared
  length is rejected (`handle_headers:713-717`); DATA overrun and EOF-mismatch are
  rejected (`handle_data:813-833`, `:877-897`). Response-side content-length truth
  is enforced for streaming bodies (overrun `:1411-1421`, short-source `:1484-1494`).
  This is the prior review's "content-length lies" closed. Correct.
- **Stream id monotonicity / reuse**: `handle_headers:662-681` rejects even ids,
  id 0, id <= highest, and reuse of a live id (RST + reset). Correct.
- **WINDOW_UPDATE overflow**: `add_window:198-204` returns `WindowOverflow` past
  `i32::MAX`; test `window_update_overflow_is_typed`. Correct (note B6 is about the
  *zero* case, not overflow).
- **PING**: `handle_ping:647-655` requires stream 0, 8-byte payload, ACKs
  non-ACK pings, ignores ACKs. Correct.
- **SETTINGS ACK with payload / non-multiple-of-6**: rejected (`:590-598`). Correct.
- **tina-rpc frame codec**: `decode_body` validates length prefix before allocation,
  bounds every variable field against the body slice, rejects trailing bytes,
  enforces kind/error-code consistency both ways. The "decode before allocate"
  invariant holds; no smuggling or over-allocation found. Correct.

---

## Areas needing deeper review

- Inbound flow-control replenishment for buffered bodies (B7) — confirm with a live
  large-upload test against the native server.
- The native HTTP/2 *client* (`http2/client.rs`, not fully read here) likely mirrors
  B1's padding accounting and should be checked the same way.
- `:scheme` value is stored but never validated against `http`/`https` for non-CONNECT;
  CONNECT (`:method CONNECT`) requires *absence* of `:scheme`/`:path` — unenforced.

## Suggested tests
- Property: for any sequence of padded DATA frames, sum of emitted WINDOW_UPDATE
  increments == sum of on-wire DATA payload lengths (catches B1).
- Integration: park response on zero window, raise window via SETTINGS, assert flush
  (B2). POST 256 KiB buffered body, assert full receipt (B7).
- Unit: `te: gzip` rejected (B3); empty `:path` rejected (B4); stream-window overrun
  RSTs one stream only (B5); WINDOW_UPDATE(stream,0) RSTs the stream (B6); DATA on
  closed stream RSTs, not GOAWAY (B8); 4-byte PRIORITY RSTs the stream (B9).
