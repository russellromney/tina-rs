# Track A — HTTP/1, chunked, WebSocket parser strictness

Reviewer: adversarial bug-hunt, branch `codex/review-fix-wave-record-2026-05-21`, HEAD 49c3580.
Focus crate: `tina-http` (keepalive.rs uncommitted diff, chunked_decoder.rs,
parse.rs, websocket.rs, connection.rs WS reassembly + body read).

Baseline: `cargo test -p tina-http --lib` = 153 passed, 0 failed.

## Summary by risk boundary

- **WS fragmentation law (A1 + siblings):** prior A1 (unfragmented data frame
  mid-fragmentation corrupts later message) is **FIXED** — see disproven D1.
  Siblings (continuation as first frame, control frames mid-fragment, oversized
  control, RSV, masking, non-minimal length) are all correctly handled.
- **Chunked decoding:** size-line overflow, non-minimal, split CRLF, trailer
  rejection, body cap across feeds are all correct. Overflow-truncation path
  (D2) is safe.
- **Request smuggling (CL/TE):** duplicate-CL-mismatch and CL+TE conflict are
  rejected; only `chunked` (± identity) TE is accepted. Two real gaps below
  (F1 chunked-not-last; F2 dropped pipelined bytes).
- **Keepalive client body-length truth:** F3 (server over-declares/over-sends
  body → silent truncation + reuse) and F4 (`Connection` header read with `get`
  not `get_all`) are honesty gaps against a hostile/buggy server.
- **Maintain (uncommitted):** no real bug. Lazy idle stamping is a documented
  approximation; close path is race-safe (guarded by `in_flight.is_none()`).

## Findings

---

### F1 — chunked not required to be the final transfer-coding

- Severity: Low
- Confidence: High
- File: `tina-http/src/parse.rs:258-275` (`parse_transfer_encoding`), used at
  `parse.rs:197-201` and `parse.rs:559-563`.
- Violated rule: RFC 7230 §3.3.1 — if `chunked` is present in
  `Transfer-Encoding` it MUST be the final coding; otherwise the message must be
  rejected (it's a smuggling vector when `chunked` is not last).
- Concrete bug: `parse_transfer_encoding` only tracks two booleans
  (`chunked`, `unsupported`); it ignores token order. `Transfer-Encoding:
  chunked, identity` sets `chunked=true`, `identity` is skipped as a no-op
  (line 265), `unsupported=false` → the request is accepted and decoded as
  chunked even though `chunked` is not the last coding.
- Why in real use: `identity` after `chunked` is the only accepted non-last
  case, and `identity` is a no-op coding, so this is not exploitable against
  Tina's own decoder (any *real* second coding like `gzip` sets `unsupported`
  and is rejected). It is a strictness gap, not a live smuggle on this stack.
- Failing test idea: `parse_request_head` on
  `"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked, identity\r\n\r\n"`
  should fail `UnsupportedTransferEncoding`; today it returns `chunked=true`.
- Fix: track the last non-identity token and reject if it isn't `chunked`:
  reject when any token follows `chunked` other than trailing `identity`/empty,
  or simpler — reject when `chunked` is seen but is not the final token.
- LLM pattern: yes — "set a bool per recognized token" misses ordering law.

---

### F2 — keepalive pipelined request bytes are silently discarded (lost request)

- Severity: Low
- Confidence: High
- File: `tina-http/src/connection.rs:855-860` (buffered dispatch) +
  `connection.rs:595-621` (`reset_for_next_request` clears `read_buf`).
- Violated invariant: HTTP/1.1 keepalive must process every complete request on
  the connection in order; a pipelined request must not be dropped.
- Concrete bug: in the buffered path, `buf.truncate(body_end)` drops any bytes
  after the current request body, and `read_buf` is then emptied. Between
  keepalive iterations `reset_for_next_request` calls `read_buf.clear()`. So if
  a client pipelines request #2 in the same TCP segment as request #1's body,
  request #2's bytes are discarded; the client hangs waiting for a response
  that never comes (until its own idle/read timeout).
- Why in real use: pipelining is rare but legal; aggressive clients, some load
  generators, and HTTP smuggling test tools pipeline. Result is a stall, not a
  smuggle (the dropped bytes are never re-interpreted), so impact is a hung
  request, not a security hole.
- Failing test idea: feed one connection
  `GET /a ...\r\n\r\nGET /b ...\r\n\r\n` in a single read; assert two responses.
  Today only `/a` is answered.
- Fix: after slicing the body, retain `read_buf[body_end..]` instead of
  discarding, and feed it to the parser at the next iteration (do not
  `read_buf.clear()` if leftover bytes exist). If pipelining is intentionally
  unsupported, detect leftover bytes and force `connection_close` so the client
  learns request #2 was refused rather than silently stalling.
- LLM pattern: yes — happy-path "one request per buffer" assumption.

---

### F3 — keepalive client trusts server body length; over-sent body is silently
truncated and the connection is reused

- Severity: Medium
- Confidence: Medium
- File: `tina-http/src/keepalive.rs:872-878` (`body_complete`) +
  `keepalive.rs:783-827` (`deliver_success`, non-chunked branch lines 791-794).
- Violated invariant: "Protocol headers and body lengths tell the truth" —
  parser output must be only what downstream can safely consume, and a framing
  desync must retire the transport, not be silently absorbed.
- Concrete bug: `body_complete` returns true as soon as
  `read_buf.len() >= head_len + content_length`. `deliver_success` then slices
  exactly `read_buf[head_len..body_end]` and **drops everything past
  `body_end`**. With `must_retire=false` (the common reusable case) the
  transport is kept and the next request starts with a fresh `read_buf`. If a
  server sends MORE bytes than its declared `Content-Length` (buggy or
  malicious origin), the extra bytes — which are the leading bytes of the next
  response or injected content — are discarded, and the slot is reused. The
  next request's response then begins parsing at whatever the server sends next,
  with no detection that framing already desynced.
- Why in real use: response-splitting / desync from a compromised or buggy
  upstream is exactly the case keepalive clients must defend against, because
  the connection is reused for later, unrelated requests. A request-response
  client (`encode_request`, `Connection: close`) is immune; the keepalive pool
  is not.
- Failing test idea: `ScriptedServer` replies `Content-Length: 5\r\n\r\nhelloEXTRA`
  on a reusable connection; assert the keepalive connection either retires
  (`must_retire=true`) or fails, rather than returning `Ok` + reusing.
- Fix: when non-chunked and `read_buf.len() > body_end` on a reusable
  (`!response_says_close`) connection, treat it as a framing error and set
  `must_retire=true` (drop the transport). Cheap and closes the desync window.
- LLM pattern: yes — `>=` "have enough bytes" check with no "too many bytes"
  branch; classic body-length-truth gap (mirrors prior HTTP/2 content-length
  lies the playbook calls out).

---

### F4 — keepalive client reads response `Connection` header with `get`, not
`get_all` (duplicate-header close missed)

- Severity: Low
- Confidence: High
- File: `tina-http/src/keepalive.rs:890-914` (`response_says_close`).
- Violated rule: a header that can legally appear on multiple lines must be
  evaluated across all of them; `Connection` is list-valued and can be split
  across lines.
- Concrete bug: `head.headers.get(CONNECTION)` returns only the first
  `Connection` value. A response with `Connection: keep-alive\r\nConnection:
  close` is read as keep-alive; the close intent on the second line is ignored,
  so the client reuses a connection the server is about to close. (The server
  side parses `Connection` correctly in `build_head`; only the keepalive client
  reader is single-valued.)
- Why in real use: self-healing — the reused socket's next request fails with
  `Closed`/`must_retire`, costing one wasted request + reconnect. Not a
  security hole, but it defeats the close signal under duplicate headers.
- Failing test idea: server response with two `Connection` lines (`keep-alive`
  then `close`); assert `must_retire=true`.
- Fix: iterate `headers.get_all(CONNECTION)` and OR the `close` test across all
  values (and likewise for the HTTP/1.0 keep-alive check).
- LLM pattern: yes — `get` vs `get_all` on a list-valued header.

---

## Disproven suspicions (recorded with proof)

### D1 — A1 (unfragmented data frame mid-fragmentation) — FIXED

`connection.rs:1617-1623`: a TEXT/BINARY frame (opcode 0x1/0x2, not FIN) that
arrives while `ws.fragmented_message.is_some()` now closes with
`WebSocketError::ProtocolError` instead of overwriting the in-progress
fragment. The continuation-as-first-frame sibling is also covered
(`connection.rs:1635-1636` closes ProtocolError when `fragmented_message` is
None). Control frames mid-fragment do not touch `fragmented_message`
(handlers at `connection.rs:1590-1605` only set `post_write_app` /
`awaiting_pong_generation`). Proof: covered by the existing strictness path;
suggested regression test `ws_unfragmented_text_mid_fragmentation_protocol_close`.

### D2 — chunked size-line overflow via split-feed truncation — SAFE

`chunked_decoder.rs:255-282`: when an accumulated size line plus new input
exceeds `size_buf` (64), `take = i.min(size_buf.len()-size_len)` truncates the
line to 64 hex chars before `parse_chunk_size_line`. A 64-hex-digit value
always overflows `usize` (max 16 hex digits), so `usize::from_str_radix`
returns `Err` → `BadChunkSize`. The truncated-prefix-is-a-valid-smaller-number
exploit is impossible because truncation only happens at >64 hex chars, which
always overflows. Proof: `max_size_line_rejected`,
`rejects_chunk_size_that_overflows_usize` pass; reasoning above closes the
split-feed variant.

### D3 — `serve_chunk_from_buffer` underflow (`inbound_total - inbound_delivered`)

`connection.rs:1038`: not reachable. Socket reads are capped to
`inbound_total.saturating_sub(inbound_received)` (`connection.rs:922-925`) and
the initial prefix is truncated to `body_end` (`connection.rs:818-826`), so
`inbound_delivered <= inbound_received <= inbound_total`. The extra
`.min(remaining_total)` is defensive. No underflow.

### D4 — Maintain close racing a pending read/deadline — SAFE

`keepalive.rs:595-643`: `handle_maintain` returns early when
`self.in_flight.is_some()` (line 604), so no `tcp_read`/`Wrote`/`Deadline`
continuation is outstanding when it closes the transport. A stale `Deadline`
from a prior request is already a no-op once `in_flight` is None
(`keepalive.rs:444-446`). `idle_since` lazy stamping is a documented
approximation (module docs), not a correctness bug.

### D5 — WS reserved/invalid opcodes — SAFE

`websocket.rs:815-827`: only 0x0,0x1,0x2,0x8,0x9,0xA produce a `Frame`; 0x3-0x7
and 0xB-0xF return `InvalidOpcode`. RSV bits rejected (`websocket.rs:732`),
unmasked client frame rejected (line 735), oversized/fragmented control frames
rejected (lines 782-787), non-minimal 126/127 lengths rejected (749, 776), high
bit on 64-bit length rejected (769). Close codes validated against RFC
allow-set (`valid_close_code`, 893-895: rejects 1004/1005/1006/1015). All
correct.

## Areas needing deeper review (other tracks)

- HTTP/2 content-length / pseudo-header truth (Track B) — F3 here is the HTTP/1
  analogue; confirm H2 retires on body-length mismatch.
- Pool-level handling of `must_retire=false` reuse after F3-style desync.

## Suggested tests

- Fuzz `ChunkedDecoder::feed` with arbitrary split points and adversarial
  size lines (property: never panics; total decoded <= max_body_bytes).
- Property test: `parse_transfer_encoding` rejects any TE list where `chunked`
  is not the final non-identity token (F1).
- Integration: keepalive client against a server that over-sends body bytes
  (F3) and against pipelined client requests (F2).
- Regression: `ws_unfragmented_data_mid_fragmentation_protocol_close` (D1
  guard).
