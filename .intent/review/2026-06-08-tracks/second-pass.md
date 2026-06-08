# Phase 3 — Second-Pass Truth-Gap Review (2026-06-08)

Target: `origin/main` at `/Users/russellromney/Documents/Github/tina-rs-mainreview`, HEAD `6c897af`.
Read-only. Line numbers are on main `6c897af`.

This is the narrow truth-gap pass, not a re-scan. The first-pass tracks (A–I) were
read first; this pass only reports findings that those tracks did **not** already
own. Where a probed gap turned out closed, it is recorded under "Disproven".

The thread running through the three new findings: a **field name promises a cap or a
length, and the code reads/advertises it on one path but not the symmetric path.**
The buffered path checks it; the streamed path doesn't. The encode path enforces the
count; the decode path doesn't. The client advertises its SETTINGS; the server sends
an empty frame.

---

## Ranked new findings

1. `[High/High]` tina-http/src/http2/client.rs:2075-2083, 1542-1581 — streamed HTTP/2
   response never validates body length against declared `content-length`.
2. `[High/High]` tina-http/src/grpc.rs:1490-1500 (via :838, :873) — client-streaming
   gRPC decode materializes an unbounded `Vec<Req>`; `max_messages` is never enforced
   on the decode path (only on encode at :452).
3. `[Medium/High]` tina-http/src/http2/frame.rs:146-153 (used at server.rs:651,771) —
   server's initial SETTINGS is an empty frame: it never advertises
   `MAX_CONCURRENT_STREAMS`, `INITIAL_WINDOW_SIZE`, or `MAX_FRAME_SIZE`. The
   `max_concurrent_streams` cap is enforced reactively (RST) but never told to the peer;
   the client side advertises all of these (client.rs:964-981).

---

## F1 — Streamed HTTP/2 response body length is never checked against `content-length`

- **Severity:** High
- **Confidence:** High
- **File/lines:**
  - `tina-http/src/http2/client.rs:2075-2083` (DATA handler, `response_streamed` branch)
  - `tina-http/src/http2/client.rs:1542-1581` (`deliver_to_parked_pull` → `complete_streaming_stream`)
  - `tina-http/src/http2/client.rs:2655` (`apply_response_headers` captures `response_content_length` for *all* streams)
  - Contrast: `tina-http/src/http2/client.rs:2109-2114` (buffered path *does* check)

- **Invariant / protocol rule:** RFC 9113 §8.1.1 / RFC 9110 §8.6: a response whose
  `content-length` does not equal the actual body length on END_STREAM is malformed and
  must be treated as a stream error. Project invariant: "Protocol headers and body
  lengths tell the truth."

- **Concrete bug:** The HTTP/2 client captures `response_content_length` from the
  response HEADERS for every stream (`apply_response_headers`, :2655). The **buffered**
  response path enforces it at END_STREAM: `if declared != response_body.len() {
  return Err(ContentLengthMismatch) }` (:2110-2113). The **streamed** response path
  (`response_streamed == true`, :2075-2083) pushes each DATA payload into
  `response_chunks`, sets `response_eof` on END_STREAM, and delivers a clean terminal
  `End { trailers }` chunk via `complete_streaming_stream` (:1552-1581). It never
  accumulates a received-byte counter and never compares it to
  `response_content_length`. So a streamed response that declares `content-length:
  1000` but sends 500 bytes + END_STREAM (short body), or 1500 bytes (over-send), is
  delivered to the caller as a successful `End`. The declared length is a lie the
  streamed path never checks. (The comment at :2066-2074 deliberately drops the
  per-stream total cap in favor of window backpressure — fine — but that's orthogonal
  to the content-length truth check, which is simply absent.)

- **Why it happens in real use:** Any gRPC server-streaming or chunked-ish streamed
  HTTP/2 response that the caller pulls (`response_streamed`) instead of buffering.
  A buggy or hostile upstream that under-delivers a known-length body is reported as
  complete, so the caller treats a truncated body as authoritative. The buffered path
  catches this; the streamed path silently does not — exactly the buffered/streamed
  asymmetry the playbook warns about.

- **Repro / failing test:** Drive an `Http2ClientConnection` streamed-response stream
  (the path that sets `response_streamed = true`, client.rs:1467). Feed HEADERS with
  `content-length: 10`, then a DATA frame of 4 bytes with END_STREAM. Park a pull;
  assert the caller receives a `ProtocolError(ContentLengthMismatch)` (or a `Reset`),
  not `ResponseChunk::End`. Today it gets a clean `End`. No existing test covers this:
  `grep content_length tina-http/src/http2/client.rs` shows the check only at the
  buffered :2110; `grpc_live.rs:1445/1469` cover only the *request* (server-side) path.

- **Fix (small, idiomatic):** Track received body bytes on streamed responses and
  validate on EOF. Add a `response_body_received: usize` (or reuse a counter) bumped by
  `payload_len` in the streamed branch (:2076), and in `complete_streaming_stream`
  (before delivering `End`, :1556) do:
  ```rust
  if let Some(declared) = stream.response_content_length {
      if declared != stream.response_body_received {
          // settle the parked pull as ContentLengthMismatch, RST_STREAM the peer
          return self.fail_streamed_content_length(idx, effects);
      }
  }
  ```
  Mirror the buffered branch's terminal cause so streamed and buffered agree.

- **LLM-pattern?** Yes. Classic "implemented the check on the path I tested (buffered)
  and forgot the symmetric path (streamed)." The field is even captured for the
  streamed stream — the data is right there, unused.

---

## F2 — Client-streaming gRPC decode ignores `max_messages`: unbounded `Vec<Req>`

- **Severity:** High
- **Confidence:** High
- **File/lines:**
  - `tina-http/src/grpc.rs:1490-1500` (`decode_streaming_body` — the unbounded loop)
  - reached via `tina-http/src/grpc.rs:838` (`call`, HTTP/1) and `:873`
    (`call_http2`) — both client-streaming handler entry points
  - public surface: `decode_streaming_request` exported at `tina-http/src/lib.rs:215`
  - Contrast: the *encode* side enforces the cap at `tina-http/src/grpc.rs:450-457`
    (`from_messages`: `if count > limits.max_messages { return TooManyMessages }`)
  - cap definition: `tina-http/src/grpc.rs:65` (`max_messages`, default 64 at :84)

- **Invariant / protocol rule:** "Bounded capacity bounds the real thing, not just a
  visible handle." `GrpcLimits.max_messages` names a cap on the number of messages.

- **Concrete bug:** `decode_streaming_body` loops `while cursor < body.len()` and
  `messages.push(decode_one_grpc_message(...))` with no count check (:1496-1498). Each
  gRPC frame header is 5 bytes (1 compression byte + 4 length bytes), so a body of
  zero-length messages costs 5 bytes per `Vec<Req>` entry. The body is bounded by
  `max_body_bytes` (large by default), but the **message count** is bounded only by
  `max_body_bytes / 5`. The `max_messages` cap that the streaming *reader*
  (`next_buffered_message`) and the *encoder* (`from_messages`, :452) both honor is
  simply not applied here. The handler then receives, and iterates,
  millions of decoded `Req` values in one turn.

- **Why it happens in real use:** A client-streaming gRPC RPC (`call`/`call_http2`,
  :831/:859) receives the whole request body buffered, then decodes it eagerly into
  `Vec<Req>` before invoking the user handler. A peer (or a fuzzer) sends one large
  buffered body packed with tiny/empty length-prefixed messages. `max_body_bytes`
  caps bytes but not the allocation count or the per-turn handler work — a single
  request can pin O(max_body_bytes/5) `Req` structs and run the handler's per-message
  loop that many times, blocking the shard turn.

- **Repro / failing test:** Build a buffered body of N = (max_body_bytes/5)+something
  empty gRPC frames (`[0x00, 0,0,0,0]` repeated). Call `decode_streaming_request`
  (or drive `call`) with default limits (`max_messages = 64`). Assert it returns
  `GrpcError::TooManyMessages { max: 64, .. }`. Today it returns `Ok(Vec)` with N
  entries.

- **Fix (small, idiomatic):** Enforce the count in the decode loop, matching the
  encode side:
  ```rust
  fn decode_streaming_body<T>(body, limits) -> Result<Vec<T>, GrpcError> {
      let mut cursor = 0;
      let mut messages = Vec::new();
      while cursor < body.len() {
          if messages.len() >= limits.max_messages {
              return Err(GrpcError::TooManyMessages {
                  count: messages.len() + 1, max: limits.max_messages,
              });
          }
          messages.push(decode_one_grpc_message::<T>(body, &mut cursor, limits)?);
      }
      Ok(messages)
  }
  ```

- **LLM-pattern?** Yes. The cap exists, is documented, is enforced on the encode and
  reader paths, and is plumbed into `GrpcLimits` here — but the eager-decode helper
  that one of the two streaming entry points actually uses forgot it. "Named bound,
  unenforced on one path."

---

## F3 — Server initial SETTINGS is empty: configured HTTP/2 limits are never advertised

- **Severity:** Medium
- **Confidence:** High
- **File/lines:**
  - `tina-http/src/http2/frame.rs:146-153` (`settings_frame` always builds an empty
    `Vec::new()` payload)
  - used at `tina-http/src/http2/server.rs:651` (initial, non-ack) and `:771` (ack)
  - server *does* enforce the cap reactively: `server.rs:875` (`if self.streams.len()
    >= self.limits.max_concurrent_streams { RST_STREAM(REFUSED_STREAM) }`)
  - Contrast: client advertises real values — `tina-http/src/http2/client.rs:964-981`
    (`INITIAL_WINDOW_SIZE`, `MAX_FRAME_SIZE`, `MAX_CONCURRENT_STREAMS`, `ENABLE_PUSH=0`)

- **Invariant / protocol rule:** RFC 9113 §6.5: a peer learns your limits only from
  the SETTINGS you send; until then it uses protocol defaults
  (`MAX_CONCURRENT_STREAMS` = unlimited, `INITIAL_WINDOW_SIZE` = 65535,
  `MAX_FRAME_SIZE` = 16384). Project invariant: a named cap should be the cap the peer
  is actually told about, not a local secret.

- **Concrete bug:** The server's initial SETTINGS frame carries an **empty** payload
  (`settings_frame(false)` → `Vec::new()`, frame.rs:151). So the server advertises
  *nothing*: not its `max_concurrent_streams` (default 64), not its
  `initial_stream_window`, not its `max_frame_size`. A conforming client therefore
  assumes "unlimited concurrent streams" and the 65535 / 16384 defaults. The server
  enforces `max_concurrent_streams` only reactively at :875 by RST_STREAM'ing the
  (N+1)th stream with REFUSED_STREAM. The cap *name* implies the peer is bounded; the
  wire never tells the peer, so the peer overshoots and eats avoidable resets.

  Worse on the window/frame side: if `limits.initial_stream_window` or
  `max_frame_size` are configured **above** the protocol defaults, the server silently
  fails to grant the larger window / permit larger frames to the peer — the peer keeps
  to 65535 / 16384 — so the operator's configured tuning is invisible and ineffective
  on the inbound direction. The client side does this correctly (client.rs:964-981),
  so this is a server-only omission and an asymmetry between the two halves of the
  same crate.

- **Why it happens in real use:** Any real HTTP/2 client (curl, grpc-go, hyper,
  browsers) talking to the Tina server. Concurrent-stream-heavy clients (e.g. gRPC
  multiplexing) open more than 64 streams, get spurious RST_STREAM(REFUSED) storms
  they could have avoided, and never benefit from a tuned-up window/frame size. The
  local enforcement prevents resource exhaustion, which is why this is Medium not
  High — but the protocol contract the field names imply is not on the wire.

- **Repro / failing test:** Decode the bytes the server emits after preface
  (`server.rs:651` path) and assert the SETTINGS payload contains
  `SETTINGS_MAX_CONCURRENT_STREAMS = max_concurrent_streams` (and the configured
  window / frame size). Today the payload length is 0.

- **Fix (small, idiomatic):** Give `settings_frame` (or a server-specific builder) the
  real payload, mirroring the client:
  ```rust
  fn server_settings_frame(limits: &Http2Limits) -> Frame {
      let mut p = Vec::with_capacity(24);
      push_setting(&mut p, SETTINGS_MAX_CONCURRENT_STREAMS, limits.max_concurrent_streams as u32);
      push_setting(&mut p, SETTINGS_INITIAL_WINDOW_SIZE, limits.initial_stream_window as u32);
      push_setting(&mut p, SETTINGS_MAX_FRAME_SIZE, limits.max_frame_size as u32);
      push_setting(&mut p, SETTINGS_ENABLE_PUSH, 0);
      Frame::new(FRAME_SETTINGS, 0, 0, p)
  }
  ```
  Keep the ACK frame empty (that part is correct).

- **LLM-pattern?** Partly. The reactive enforcement at :875 looks complete in
  isolation ("we cap concurrent streams"), so the missing *advertisement* is easy to
  miss — the classic "enforced one side, never told the peer" gap. The client half
  being fully correct makes the server omission stand out.

---

## Disproven / probed-and-closed (recorded with proof)

These are truth gaps I specifically chased this pass and found **already enforced**.
Recording them so a future pass doesn't re-spend time.

- **HTTP/2 request content-length truth (server inbound):** ENFORCED. DATA overrun vs
  declared CL → RST PROTOCOL_ERROR (`server.rs:1028-1047`); END_STREAM with
  received != declared → RST (`server.rs:1103-1123`). Response side: overrun reset
  (`server.rs:1748-1758`), short-source-on-EOF reset (`server.rs:1816-1831`). Only the
  *streamed client response* path (F1) is unguarded.

- **HTTP/2 buffered client response content-length:** ENFORCED at
  `client.rs:2109-2114` (`declared != response_body.len()` → `ContentLengthMismatch`).

- **HTTP/1 duplicate / split Content-Length & TE smuggling:** SAFE. Two differing CL
  header lines → `InvalidContentLength` (`parse.rs:193-197`); non-digit CL value
  (covers `5, 6` comma forms) rejected (`parse.rs:184`); `TE: chunked` + CL together
  rejected (`parse.rs:228-230`); non-final/unsupported transfer-codings rejected
  (`parse.rs:224-234`, `parse_transfer_encoding` requires `chunked` as the final
  token).

- **WebSocket framing strictness:** SAFE. RSV bits (`websocket.rs:784`), mask-direction
  (`:787-790`), control-frame >125 (`:837`), fragmented control (`:840`),
  data-while-fragmented and continuation-without-start (`connection.rs:1834-1856`),
  whole-message UTF-8 on Text incl. fragment reassembly under the start opcode
  (`connection.rs:1899`), close-code validity excluding 1004/1005/1006/1015/>4999 and
  reason UTF-8 (`websocket.rs:1000-1018`). All present.

- **gRPC streaming frame decode (the reader, not the buffered helper):** length prefix
  is checked against `max_message_bytes` *before* reading the body
  (`grpc.rs:601-620`), so per-message resident bytes are bounded. Compression byte
  rejected (`:594`, `:1548`). Only the *count* on the buffered `decode_streaming_body`
  helper (F2) is unbounded.

- **RPC frame length prefix:** SAFE. `parse_length_prefix` bounds body by
  `max_frame_size` before allocation (`tina-rpc/src/frame.rs:20`, `connection.rs:580`),
  inbound buffer bounded by `max_frame_size + read_chunk`.

- **Durable outbox exactly-once / torn tail:** SAFE and honestly at-least-once by
  design. `fold` rejects duplicate enqueues, dangling completes, and over-capacity
  backlogs, and treats any unframe failure as `CorruptTail`
  (`durable_outbox.rs:707-773`). Completed-id watermark keeps the set bounded
  (`:788-797`).

- **Snapshot atomicity (storage):** SAFE. WriteTemp(+fsync) → Rename → SyncParent
  ordering with cleanup-on-failure and `CommitUncertain` on parent-sync failure
  (`storage.rs:1364-1408`). Correct crash-safe sequence.

- **HTTP/2 `:authority` duplication & max-concurrent enforcement:** duplicate
  `:authority` rejected (`headers.rs:243-248`), authority/Host presence required
  (`headers.rs:427-428`), concurrent-stream cap enforced (`server.rs:875`). The only
  gap is *advertising* the cap (F3), not enforcing it.

- **HTTP/2 per-stream response scheduling fairness:** SAFE. One chunk in flight per
  stream via `response_pull_in_flight` (`server.rs:1683-1686`); no single stream
  monopolizes the pull loop.

- **Bridge terminal-cause classification (sqlite, reqwest):** typed and distinct;
  `Full`/`Closed`/`Timeout`/`Internal` are not collapsed into each other
  (`tina-sqlite-bridge/src/helpers.rs:474-516`). (The sqlite timeout-frees-slot and
  reqwest retry-on-timeout concerns are owned by first-pass D3/D4.)

---

## Truth gaps probed but not closed (out of time)

- Whether the HTTP/2 server *validates inbound frame size* against the 16384 default
  it (silently) advertises — i.e. does it accept a DATA/HEADERS frame larger than
  16384 even though it told the peer nothing? If it accepts >16384 while advertising
  the default, that's a latent corollary of F3. Did not fully trace the inbound frame
  length check vs `max_frame_size`.
- Streamed HTTP/2 response *over-send* relative to `content-length` (F1 covers both
  under and over, but I only confirmed the absence of any counter, not constructed the
  over-send repro end to end).
- Connection-window credit accounting for a streamed response whose caller *never*
  pulls (drops the stream mid-body): looked correct (credit is batched on DATA receipt
  in `handle_read`, not gated on consume) but not exhaustively traced through every
  teardown path.
