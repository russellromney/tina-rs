# Track A — HTTP/1, chunked encoding, WebSocket parser strictness

Worktree: `/Users/russellromney/Documents/Github/tina-rs-adv`, HEAD `0cd6a31`
(= origin/main). Source treated read-only.

Scope swept: `tina-http/src/parse.rs`, `chunked_decoder.rs`, `keepalive.rs`,
`websocket.rs`, `connection.rs` (HTTP/1 + WS paths), `websocket_client.rs`.
Extra scrutiny on recent perf landings (Phases 146–156: buffered-response byte
path, protocol turn & header cost, keepalive over-send retire #228).

## Verdict

One fresh **High** bug: the native WebSocket **client** ignores the FIN bit and
has no continuation/reassembly, so any RFC-compliant server that fragments a
message corrupts client data and then forces a protocol close. The server side,
the chunked decoder, the HTTP/1 head parser, and keepalive reuse truth are all
solid — the prior review's A-F3 (keepalive over-send) is fixed and verified.

---

## Findings

### A1 — WebSocket client ignores FIN / no fragment reassembly (FRESH)

- **Severity:** High
- **Confidence:** High
- **File:line:** `tina-http/src/websocket_client.rs:531-568` (`drain_frames`);
  contrast the correct server path at `tina-http/src/connection.rs:1829-1890`
  (`handle_websocket_data_frame`).
- **Violated rule:** RFC 6455 §5.4 (fragmentation). A data message MAY arrive as
  an initial frame with `FIN=0` followed by one or more `opcode=0x0`
  continuation frames; only the final frame has `FIN=1`. A receiver must
  reassemble before surfacing the message, and must validate text UTF-8 over the
  *whole* reassembled payload.
- **Concrete bug:** The client's `match frame.opcode` has no `fin` guard and no
  continuation (`0x0`) handling:
  - `0x1` (text) at line 532 runs `String::from_utf8(frame.payload)` and pushes a
    complete `Text` event regardless of `frame.fin`. A non-final text fragment is
    surfaced to the app as if it were the entire message (truncated payload).
  - `0x2` (binary) at 536 likewise emits a partial fragment as a complete
    `Binary` event.
  - The subsequent continuation frame (`opcode=0x0`) falls through to the `_ =>`
    arm at line 567 and `close_with(ProtocolError)` — killing a connection the
    peer used legally.
  So a single fragmented server message both **corrupts data** (partial fragment
  delivered as a whole message, and for split UTF-8 a valid multi-byte char gets
  rejected mid-sequence) and **drops the connection**.
- **Why it happens in real use:** Tina's own server emits unfragmented frames, so
  Tina-client ↔ Tina-server never trips it and every existing test passes. But
  the client is a general HTTP/1 WS client (`websocket_client.rs`, and the
  outbound `connect/websocket_manager/isolate.rs` that drives it). Real servers
  fragment routinely: nginx/HAProxy proxied upstreams, Node `ws` with
  `fragmentOutgoingMessages`, large messages chunked by the sender, or any peer
  that interleaves a ping inside a long message. Against those, the client
  silently mis-frames.
- **Failing test idea:** in `tina-http/tests/websocket_live.rs`, mirror the
  existing server-side `websocket_fragmented_text_reassembles_with_interleaved_ping`
  but drive a *client* against a raw socket server that writes
  `frame(fin=false, 0x1, "hel")` then `frame(fin=true, 0x0, "lo")` (server frames
  are unmasked). Assert the client surfaces exactly one `Text("hello")` event and
  the connection stays open. Today it would emit `Text("hel")` then protocol-close
  on the `0x0` frame. Add a split-UTF-8 variant
  (`frame(false,0x1,[0xc3])` + `frame(true,0x0,[0x28])`) asserting a clean
  protocol close *after* reassembly, not a mid-fragment `from_utf8` failure.
- **Small fix:** give the client the same reassembly the server already has. Add
  `fragmented: Option<(u8 /*opcode*/, Vec<u8>)>` to the client state, then in
  `drain_frames`:
  - `0x1|0x2` with `fin` and no pending fragment → deliver as today.
  - `0x1|0x2` without `fin` → start `fragmented = Some((opcode, payload))`,
    continue reading. Reject if a fragment is already open (`ProtocolError`).
  - `0x0` → require an open fragment (else `ProtocolError`); append payload under
    a `max_message_bytes` check (use `checked_add`, same as
    `connection.rs:1865-1873`); on `fin`, take the buffer and dispatch via the
    text/binary delivery (running `from_utf8` once on the whole text payload).
  - `0x1|0x2` while a fragment is open → `ProtocolError` (no interleaved data).
  Control frames (`0x8/0x9/0xA`) already interleave correctly and need no change
  (the frame parser already rejects fragmented control frames).
- **LLM-pattern?** Yes — classic "happy-path against our own server" omission:
  the codec layer (`parse_server_frame`) faithfully carries `frame.fin`, the
  server consumer checks it, and the client consumer was written to the
  Tina-server's unfragmented behavior rather than to the protocol. The
  asymmetry (server reassembles, client doesn't) is the tell.

---

## Disproven / verified-safe suspicions (recorded with proof)

- **A-F3 keepalive over-send (prior review, fixed #228) — VERIFIED FIXED.**
  `keepalive.rs:806-818`: after a non-chunked body completes, `body_end =
  head_len + content_length`; if `read_buf.len() > body_end` the socket is marked
  `must_retire` and dropped via `close_transport_fire_and_forget`. Body slice is
  still `[head_len..body_end]` so pipelined trailing bytes are discarded.
  Test: `content_length_over_send_retires_connection`.

- **Chunked size-line smuggling / non-minimal / overflow — SAFE.**
  `chunked_decoder.rs`: `MAX_SIZE_LINE_LEN=64` cap enforced both in-feed
  (line 258) and across feeds (line 301); `parse_chunk_size_line` rejects leading
  whitespace, empty, non-hex, and `from_str_radix` overflow; `decoded_total`
  guarded with `checked_add` and `max_body_bytes` (line 96-107, 125-133).
  Trailers rejected (`TrailersNotSupported`), split CRLF/trailer terminators
  handled by the `has_trailer_partial`/`size_buf[..]==b'\r'` carry logic.
  Tests cover all of these.

- **Split-read state across feeds (size, data, DataCrlf, trailers) — SAFE.**
  `feed` returns `consumed` excluding any partial CRLF; the connection
  (`chunked_raw_buffer`) and keepalive (`read_buf` + `chunked_raw_consumed`)
  callers retain unconsumed bytes and re-feed. `DataCrlf` with 1 byte returns
  `(NeedMore, 0)` leaving the lone `\r` in the caller buffer. Verified by
  `split_crlf_after_data`, `split_trailers_terminator_across_single_byte`.

- **HTTP/1 request smuggling (CL+TE, dup CL, TE token list, request target) —
  SAFE.** `parse.rs:228-234` rejects `chunked` + `Content-Length` together;
  `190-198` rejects conflicting duplicate `Content-Length`; `parse_transfer_encoding`
  requires `chunked` to be the *final and only* token, rejecting `gzip, chunked`,
  `identity`, empty, and trailing junk; `is_valid_origin_form_request_target`
  rejects absolute/authority form, `//`, controls, `0x7f`, and whitespace, both
  inbound and before outbound encode (`encode_request_internal` asserts).

- **No-pipelining keepalive (server) — SAFE, intentional.**
  `connection.rs:633-659` `reset_for_next_request` clears `read_buf`,
  `chunked_raw_buffer`, parsed head, and re-issues a fresh `read_more`, so trailing
  bytes after a request body (including a smuggled second request) are dropped,
  not parsed. Buffered dispatch truncates to `body_end` (line 910-913).

- **WebSocket frame parser strictness — SAFE.** `websocket.rs:774-894`: RSV
  rejected; client-unmasked / server-masked rejected; non-minimal 126/127 lengths
  rejected (`ProtocolError`); 64-bit high-bit rejected; `checked_add` on every
  offset so a huge declared length rejects (`FrameTooLarge`) before any wrapped
  slice; control frames `>125` and fragmented control frames rejected;
  `max_frame_bytes`/`max_message_bytes` enforced. Tests cover the non-minimal and
  overflow cases.

- **WebSocket text UTF-8 fragmentation (server) — SAFE.** Server validates UTF-8
  on the *reassembled* payload (`connection.rs:1899` `String::from_utf8` after
  `handle_websocket_data_frame` reassembly), not per-fragment; `max_message_bytes`
  checked with `checked_add` during reassembly (1865-1873). (This is exactly what
  the client lacks — see A1.)

- **WS close-code validation — SAFE.** `valid_close_code` (websocket.rs:1016)
  admits `1000..=1003 | 1007..=1014 | 3000..=4999`, correctly excluding reserved
  1004/1005/1006/1015; `decode_close_payload` rejects a 1-byte payload and
  non-UTF-8 reason.

- **WS client read-buffer growth — SAFE (minor asymmetry, not a bug).** The
  client does not reference `read_buffer_high_water`, but `parse_server_frame`
  rejects any frame whose declared length exceeds `max_frame_bytes`, so `read_buf`
  is bounded ~`max_frame_bytes` before rejection. Consider wiring
  `read_buffer_high_water` for parity with the server, but there is no unbounded
  growth.

## Suggested tests to add

- Client-receives-fragments live test (the A1 failing test above), text + binary
  + split-UTF-8 + interleaved-ping variants.
- Property/fuzz: feed `parse_server_frame` and the client `drain_frames` a stream
  of randomly fragmented data messages; assert reassembled output equals the
  concatenation and the connection survives.
