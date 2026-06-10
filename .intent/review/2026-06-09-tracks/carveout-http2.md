# Carve-out: HTTP/2 deep-dive (client stream-drop + frame-size law)

HEAD `0cd6a31` (= origin/main). Read-only review. Completes two "areas
needing deeper review" from `adversarial-review-2026-06-08.md` not covered by
any fresh 2026-06-09 track. Cross-read first: `2026-06-09-tracks/track-B.md`
(B11), `second-pass.md` (SP-A/SP-C). Those are all **server**-side. This
carve-out is **client**-side (Q1) and shared `frame.rs` (Q2).

Each candidate was attacked to disprove before filing.

---

## Q1 — client streamed-response drop: window credit / RST / slot leak

`tina-http/src/http2/client.rs`. A caller of `OpenStream` receives a
`ResponseStreaming { status, headers }` head, then pulls the body with repeated
`ResponseNext { stream_id }` messages. There is **no owning handle and no
`impl Drop`** anywhere in the client — the caller holds a bare `u32 stream_id`.
"Drop the response without pulling all DATA" therefore means: stop sending
`ResponseNext`, and never send `Cancel`. (Explicit `Cancel { stream_id }`
exists and is correct: `handle_cancel` at `:1819` sends RST_STREAM(CANCEL),
removes the slot, settles the waiter, cancels the request source.)

### Disproof — connection recv-window is NOT starved by drop-without-pull

The headline worry ("repeated drop-without-pull permanently shrinks the shared
connection recv window and starves other streams") is **DISPROVEN**.

Proof: `handle_data` (`:2172-2175`) debits the **connection** window and
accumulates `self.pending_recv_window_credit` (a connection-global field) on
*receipt* of every DATA frame, before any stream lookup and independent of
whether the caller ever consumes:
```
self.recv_window -= flow_i32;
self.pending_recv_window_credit = self.pending_recv_window_credit
    .saturating_add(flow_len as u32);
```
That credit is flushed back to the peer as a stream-0 WINDOW_UPDATE once it
crosses `WINDOW_CREDIT_FLUSH_THRESHOLD` (16 KiB) in `handle_read` (`:1892-1897`).
The flush is keyed only off the connection-global pending counter, not off any
stream, so removing/abandoning a stream cannot strand connection credit. The
**per-stream** window is the deliberate backpressure lever and is the only
window held on the consume path (`:2204-2212` comment is explicit and correct).
A slow/abandoned consumer stalls *its own* stream (the peer stops sending on it
once the stream window hits zero) but the connection window keeps cycling for
every other stream. No connection-level starvation. Comment at `:2169-2172`
states this invariant; the code matches it.

Residual nit (not filed): the last sub-threshold connection credit on an
otherwise idle connection is held in `pending_recv_window_credit` until the
*next* DATA frame pushes it over threshold. This is the standard batched-
WINDOW_UPDATE tradeoff, peer send is still bounded by per-stream windows, and
it self-heals on any further DATA. Not a leak.

### Finding CH-1 — abandoned streamed response is never reaped: no RST_STREAM, slot + buffered body + per-stream window held until connection close

1. **Severity:** Medium
2. **Confidence:** High (mechanism code-proven; impact bounded but real)
3. **File:line:**
   - No `impl Drop` / no idle reaper for client streams anywhere in
     `client.rs` (grep clean). Comment at `:176-177` confirms the design:
     "Today parked streams wait indefinitely."
   - Admission hard cap that the leaked slot eats: `admit_stream`
     `client.rs:1202` —
     `if self.streams.len() >= self.limits.max_concurrent_streams { ... Full }`
     (default `max_concurrent_streams = 64`, `:127`).
   - Streamed DATA buffers chunks and holds the per-stream window with no
     total-body cap: `:2213-2222` (`response_chunks.push_back`, per-stream
     recv-window returned *only* on consume in `deliver_to_parked_pull`
     `:1589-1595`).
   - Stream is removed only on EOF (`complete_stream` `:1637`), peer RST
     (`handle_rst_stream` `:2352`), local `Cancel` (`:1824`), or whole-
     connection teardown. None of these fire for a silently-abandoned stream.
4. **Violated invariant:** a streamed response the caller stops consuming and
   never cancels must not pin a connection stream slot or unbounded resident
   memory indefinitely. The explicit `Cancel` path keeps the contract; the
   *implicit drop* path has no owner and keeps nothing. (Direct analogue of
   SP-A/SP-B on the server: "the success-path / abandon-path source lifecycle
   has no owner at all" — same shape, client side.)
5. **Concrete bug:** a caller that obtains the `ResponseStreaming` head and
   then drops its intent (task cancelled, early `return`, error unwinds the
   caller before it loops `ResponseNext` to `End`/`Closed`) leaves
   `ActiveClientStream` resident forever:
   - one of 64 `max_concurrent_streams` slots is consumed permanently;
   - any already-buffered `response_chunks` stay resident (bounded by the
     per-stream window ~ initial-window bytes, but never freed);
   - the per-stream recv-window stays debited, so the peer is told (correctly)
     to stop — but the client never RST_STREAMs, so on the peer side the
     stream also lingers half-open.
   After 64 such abandonments on one pooled connection, `admit_stream` returns
   `Full` for every subsequent request on that connection even though zero
   streams are actually doing work — the connection is bricked until it is torn
   down. This is the client twin of server SP-A's "sustained churn = unbounded
   growth"; here it is bounded per-connection (64 slots) but still a hard
   denial of further work on a long-lived pooled connection.
6. **Real-use scenario:** pooled HTTP/2 client doing server-streaming gRPC
   subscriptions or large downloads where the *application* aborts mid-body
   (user navigates away, deadline elapses, upstream error). If the abort path
   forgets to send `Cancel` — and nothing in the type system forces it,
   because the caller holds only a `u32` — each abort burns a slot. A handful
   of buggy/abandoning call sites slowly brick every connection in the pool.
7. **Failing-test idea:** open N = `max_concurrent_streams` streamed responses
   against a peer that sends HEADERS + a partial body (no END_STREAM); for each,
   deliver the `ResponseStreaming` head and then **never** send `ResponseNext`
   or `Cancel`; assert that a fresh `OpenStream`/`Submit` now reports `Full`
   and that no RST_STREAM was ever enqueued for the abandoned ids. Today both
   assertions hold (the bug). Contrast: the same test issuing `Cancel` on each
   frees the slot and emits RST_STREAM.
8. **Fix sketch:** give the abandon path an owner. Options, cheapest first:
   (a) make the caller-facing streamed-response a guard type whose `Drop`
   enqueues `Http2ClientMsg::Cancel { stream_id }` (mirrors how the server
   contract expects `ResponseChunkMsg::Cancel`); or (b) a per-stream
   response-pull deadline (the report already reserves `Timeout` /
   `flow_control_parks` plumbing at `:171-177`): a stream whose parked-pull or
   last-consume age exceeds a bound is RST_STREAM(CANCEL)'d and removed.
   Either reclaims the slot, frees `response_chunks`, and sends the RST the peer
   needs. (a) is the structural fix and closes the "no owner" gap directly.

### Disproof — explicit `Cancel` path is correct

For completeness: when the caller *does* send `Cancel`, `handle_cancel`
(`:1819-1838`) sends RST_STREAM(CANCEL), removes the slot via
`swap_remove_stream_at`, cancels the request source, emits the reset fact, and
settles the parked pull/waiter. Connection window is unaffected (already
credited on receipt). No leak on this path. The gap is strictly the *implicit*
drop with no Cancel.

---

## Q2 — frame-size law (frame.rs + server.rs + client.rs): CLEAN

**Verdict: clean, disproven.** Both peers advertise
`SETTINGS_MAX_FRAME_SIZE = limits.max_frame_size` (default 16384) and validate
every inbound frame's declared length against that same value **before any
payload is copied or buffered**. Oversized frames become a `FrameTooLarge`
error mapped to a connection-level `FRAME_SIZE_ERROR` GOAWAY (RFC 9113 §4.2).
No unbounded buffer growth from a huge declared length.

Proof:
- `try_decode_frame_meta` (`frame.rs:93-125`) reads only the 9-byte header,
  then `if len > max_frame_size { return Err(FrameTooLarge { len, max }) }`
  at `:101-106` — **before** the payload slice exists, and uses `checked_add`
  for the header+len total (`:107-112`) so no usize overflow. The 24-bit
  length field caps `len` at `0xFFFFFF` regardless.
- **Client:** read loop calls `try_decode_frame_meta(&self.read_buf,
  self.limits.max_frame_size)` (`client.rs:1853`). The DATA payload `Vec` is
  only allocated *after* a successful meta decode (`client.rs:1862`). An
  oversized declared length errors at `:1856` → `protocol_error` →
  `FrameTooLarge` mapped to `ERR_FRAME_SIZE_ERROR` (`client.rs:2486`). The
  advertised value is written into the preface SETTINGS at `:1014-1015`.
- **Server:** `process_frames` calls the same `try_decode_frame_meta(...,
  self.limits.max_frame_size)` (`server.rs:714`); payload slice formed only
  after Ok meta (`:716-718`). Oversized → error → GOAWAY with
  `ERR_FRAME_SIZE_ERROR` (`server.rs:646`). Advertised in initial SETTINGS at
  `server.rs:2241-2242`. Unit test already pins this: `server.rs:2495-2501`
  (`max_frame_size: 1`, a 3-byte payload → `FrameTooLarge { len: 3, max: 1 }`).
- **No unbounded buffer growth:** `read_buf` accumulates only what
  `tcp_read_buf` delivered (each read bounded by `READ_CHUNK = 16 KiB`,
  `frame.rs:11`). A single frame is bounded by `max_frame_size`; the decode
  errors on the first 9 bytes of an oversized declared frame, so the buffer
  cannot be coerced to grow toward the declared size. `read_buf` is bounded by
  ~`max_frame_size + READ_CHUNK`.
- **Peer's own max_frame_size is respected on send:** both sides clamp
  outbound DATA chunking to `peer_max_frame_size` (client `:1743`, server
  `:1636`) and refuse outbound HEADERS larger than it (client `:1220` etc.).

One adjacent observation (not a frame-size-law violation, recorded only): the
SETTINGS_MAX_FRAME_SIZE value a peer advertises is range-validated to
`MIN_MAX_FRAME_SIZE..=MAX_MAX_FRAME_SIZE` on receipt (client `:2037-2041`,
server `:838-842`) before updating `peer_max_frame_size`. Correct per RFC.

---

## Condensed verdict

- **CH-1** — Medium / High — `client.rs` (no `impl Drop`/reaper; admission cap
  in `admit_stream`; buffering at `:2213-2222`) — a streamed response the
  caller abandons without `Cancel` is never reaped: no RST_STREAM, slot +
  buffered body + per-stream window held until connection close; 64
  abandonments brick a pooled connection (`admit_stream` → `Full`). Client
  twin of server SP-A/SP-B "no owner on the abandon path."
- **Q1 connection-window starvation** — DISPROVEN. Connection recv-window is
  credited on DATA receipt independent of consume/stream lifecycle
  (`client.rs:2172-2175`, flushed `:1892-1897`); only the per-stream window is
  the held backpressure lever. No cross-stream starvation.
- **Q2 frame-size law** — CLEAN / DISPROVEN. Server and client both validate
  declared length against advertised `SETTINGS_MAX_FRAME_SIZE` (16384) before
  copying any payload (`frame.rs:101`), erroring to a connection-level
  `FRAME_SIZE_ERROR` GOAWAY; no unbounded buffer growth.
