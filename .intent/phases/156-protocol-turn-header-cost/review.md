# Phase 156 Plan Review

Reviewer: Codex

## Findings

### [P2] Original scope could still pass as a harness phase

The user explicitly asked for actual performance work, not another benchmark
slice. The plan now says the PR must change named protocol/runtime hot-path
files and cannot pass with harness-only changes.

### [P2] Turn-count reduction can accidentally bypass Tina policy

The dangerous "fast" fix is direct-calling the gRPC handler from the HTTP/2
connection. That would be fast and wrong: mailbox capacity, timeout, request
context, and trace truth would be hidden. The plan now names policy boundaries
that must stay visible and forbids direct service-handler calls.

### [P2] "Prove the blocker" was too easy to use as an escape hatch

The first draft let the PR finish without any turn-count reduction if it proved
unary was already all policy boundaries. That is too soft for this phase. The
plan now requires at least one warmed protocol/app turn-count row to improve;
unary is preferred, streaming or HTTP/2 steady-state is acceptable if unary is
truly blocked by Tina policy boundaries.

### [P2] Compact HPACK can weaken validation

Skipping public header storage is good. Skipping validation is not. The plan now
lists every validation rule that compact and public paths must share, including
duplicate content length and forbidden connection-control names.

### [P2] Linux evidence must be required, not optional

Several previous perf phases found different behavior on Linux. The plan now
requires repeated Linux/x86 before/after rows and says the PR remains draft if
Linux cannot run.

### [P3] Path sharing could become an unbounded intern table

Avoiding per-request `String` allocation is good, but a hidden method-path cache
would violate Tina's boundedness story. The plan now allows a cache only if it is
explicitly bounded and reports overflow.

### [P2] Dynamic protobuf cost was named but underplanned

The first draft only covered stream decoder output reuse. Current code also
allocates fresh framed buffers in `GrpcUnaryTemplate` and server-side
`encode_grpc_message` paths. The plan now requires reusable dynamic framing
without cheating by using only preframed fixed-payload rows.

## Result

Plan updated. It is implementation-ready and grug enough: exact files, exact hot
spots, hard proof, and no planning/audit work left inside the phase.

## Plan Review 2

Reviewer: Codex

### [P2] Turn-count proof could be gamed by changing the definition

The plan required a lower turn count but did not pin what counted as a turn.
An implementation could add a new metric, count only the host thread, or change
the probe between before/after. The plan now requires stable runtime trace or
existing hotpath probe evidence, saved before/after timelines, and the same
definition on both sides. WebSocket turn wins do not count for this HTTP/2/gRPC
phase.

### [P2] Method-path allocation proof was too hand-wavy

"The test must fail if a String is rebuilt" is a wish unless the test observes
allocation or a hard seam. The plan now requires a focused warmed route-dispatch
allocation probe, not code inspection.

### [P2] Dynamic response buffer reuse could hide an unbounded pool

The first plan allowed a "bounded owned-buffer pool" phrase but did not require
the cap or failure path. The plan now says any reusable/pool storage must have
an explicit service-owned cap and visible `Full` / `ResourceExhausted` behavior.

### [P3] Compact gRPC client receive could weaken generic HTTP/2 outcomes

The client-header reduction could be implemented by deleting public headers from
generic `Http2ClientOutcome`. The plan now pins the compact receive path to
gRPC-shaped client calls such as `SubmitGrpcUnary`; generic HTTP/2 outcomes keep
their public headers.

### [P3] Linux evidence needed a concrete artifact home

The first plan said "existing workflow/Fly path" without naming where proof
lives. The plan now points at `examples/systems/perf_native/fly/` or the manual
Linux perf workflow and requires raw output plus parsed summaries under the
phase folder.

## Implementation Note 1 (Session A handoff — partial)

This phase is large (eight build items plus macOS+Linux 3x3 perf proof). This
note records the slice that is landed, tested, and measured, and names exactly
what is not done. The PR is a **draft**: per the plan it cannot be called
complete without the remaining items and the Linux evidence.

### What changed (real hot-path code)

- **Item 1 — compact HPACK decode is now actually compact.**
  `tina-http/src/http2/headers.rs`: `add_header_with_storage` no longer builds a
  public `HeaderName`/`HeaderValue` for ordinary headers in compact mode. The
  admission/gRPC facts (`content-type`, `grpc-encoding`, `content-length`,
  `host`, `te`, forbidden connection-control names, uppercase, byte cap) are now
  parsed by string. Name/value byte validation moved into `is_valid_header_name`
  / `is_valid_header_value`, run in **both** compact and public modes, so the two
  paths reject identical malformed input by construction; only the storing
  (public) path pays the typed-header allocation.
- **Item 5 — reusable gRPC stream decoder output.**
  `tina-http/src/grpc_client.rs`: added `GrpcStreamDecoder::push_into(&mut
  Vec<Resp>, bytes)`. `push` is now a thin wrapper that preserves its old
  all-or-nothing shape. The perf server-streaming loop
  (`examples/systems/perf_native/src/lib.rs`) drives `push_into` with one reused
  output `Vec` across all response chunks.

### What did NOT change

- The plain HTTP/2 service path keeps the public decode and its public
  `HeaderMap`; generic HTTP/2 services still receive custom headers. The
  `http2_h2c_steady_state_small` allocation count is unchanged (896 → 897), which
  is the intended "no silent header drop" guarantee.
- No isolation boundary was bypassed; no caps/timeouts/status truth changed.

### What is directly proved

- Unit (`http2::headers`): `compact_decode_builds_no_public_headers_regardless_of_metadata_count`
  uses a test seam to show the compact path builds **zero** public headers for
  0/4/16 metadata headers, while the public path builds exactly one per stored
  header. `compact_and_public_reject_identical_malformed_inputs` and the
  oversized-block test prove validation parity (uppercase, bad token byte, bad
  value byte, forbidden header, invalid `te`, duplicate/invalid content-length,
  over-cap).
- Unit (`grpc_client`): `push_into` reuse, partial frame across chunks,
  multi-message chunk, compressed rejected, over-cap rejected before decode,
  truncated-finish rejected.
- Blast radius: full `tina-http` suite green (lib + every integration suite,
  incl. `grpc_live` 35, `http2_live` 41, `grpc_client_live` 10). `cargo fmt
  --check`, `clippy -p tina-http`, and `clippy` on the perf example are clean.
  `perf-h2-alloc` ceiling unchanged at 1730 (27.03/req).
- Measured movement (macOS/aarch64, representative single batch, saved in
  `perf_sample_macos_partial.txt`): warmed gRPC unary process allocations
  ~3949 → ~3889 (−1.5%), gRPC server-streaming ~5296 → ~5170 (−2.4%), HTTP/2
  steady-state unchanged. The warmed gRPC request only carries two ordinary
  inbound headers, so the absolute win is small; the unit test shows the win
  scales with metadata count.

### What still needs proof / is not done

- **Item 2** (stop rebuilding public `HttpRequest` for native gRPC streaming) —
  not started. Streaming still routes through `into_http_request()`.
- **Item 3** (shared/compact `GrpcMethodPath`) — investigated, deferred. The
  warmed unary path today moves a **single** owned path `String` from HPACK
  decode → parts → `GrpcHttp2Request` → `GrpcRequest` (no clone, no rebuild).
  Converting to `Arc<str>` does not lower the allocation count, because the path
  must be owned once when it crosses the HTTP/2-isolate ↔ gRPC-router-isolate
  boundary and `Arc::from(&str)` allocates the same as `String`. A real win
  needs the decoder to intern the path against the router's registered route set
  (the natural bounded cache), which is a cross-isolate structural change. This
  is recorded so the next session does not "convert to Arc" expecting a win that
  is not there.
- **Item 4** (compact gRPC client response-head/trailer facts) — not started.
- **Item 6** (reusable dynamic gRPC encode/decode framing) — not started.
- **Item 7** (reduce real protocol turns; warmed gRPC turn probe) — not started.
- **Item 8** (hard perf proof): only a partial macOS single-batch sample exists.
  The full macOS 3x3 and the Linux/x86 3x3 are not collected. Targets (≥20%
  warmed unary, ≥15% streaming) are not met and will not be until items 2/4/6/7
  land. PR stays draft.

## Implementation Note 2 (Session A — item 2 landed)

Item 2 is now done; it was listed as "not started" in Note 1 above. Append-only,
so this corrects the record rather than editing Note 1.

### What changed

`tina-http/src/grpc.rs`: native gRPC streaming no longer falls back through a
public `HttpRequest`.

- `GrpcHttp2Request::into_http_request` is **deleted** — both callers are gone.
- `ErasedStreaming` and `ErasedStreamingRaw` gained `call_http2`, taking the
  HTTP/2 request stream straight from `GrpcHttp2Request` (no `HeaderMap`, no
  `HttpRequest`).
- `start_or_reply_http2_request` dispatches streaming/raw routes synchronously
  through the compact path, and accumulates other streamed bodies into a new
  `PendingGrpcRequest::Http2` enum variant holding compact gRPC state (method,
  path, content-type/encoding flags, the request stream, accumulated body) —
  not a public `HttpRequest`. At EOF it rebuilds a `GrpcHttp2Request` with a
  buffered body and dispatches via `response_for_http2`.
- `response_for_http2` now routes all six route kinds (unary, client-streaming,
  streaming, streaming_raw, buffered-server-streaming, server-streaming) via
  `call_http2`.
- Body-pull outcome handling is shared via `classify_request_chunk`
  (More / Eof / Failed) so the bounded, cancelable pull and the reply obligation
  behave identically for both pending shapes. The generic `HttpRequest` path
  (`PendingGrpcRequest::Public`) is unchanged for non-compact/direct-API use.

### What is directly proved

- All gRPC integration suites green, including the shapes whose dispatch changed:
  `grpc_client_streaming_reads_multiple_request_messages` /
  `..._handles_many_small_messages` (compact pending accumulation + EOF),
  `grpc_streaming_sends_response_before_request_eof` /
  `..._concurrent_streams_do_not_cross_talk` / `..._malformed_frame_sets_final_status`
  (compact sync `call_http2`), `grpc_streaming_raw_sends_response_before_request_eof`
  (raw compact path), `bidi_request_and_response_streams_progress_independently`,
  and the peer-reset cancellation tests (bounded/cancelable pull preserved).
- `grpc.rs` lib unit tests (26) green; full `tina-http` suite green; fmt + clippy
  (`tina-http` and perf example) clean. The structural deletion of
  `into_http_request` means the old rebuild cannot silently come back — it would
  not compile.

### What item 2 does NOT show

The existing perf rows (`native_protocol_rows_are_printable_and_bounded`)
exercise unary and buffered-server-streaming, not client-streaming / bidi / raw /
true-streaming, so they do not quantify item 2's allocation win. The combined
items-1+2+5 macOS sample in `perf_sample_macos_partial.txt` shows the row
movement that items 1+5 produce, with no regression from item 2. A dedicated
client-streaming/bidi allocation row would be needed to put a number on item 2;
that is left for the perf-proof pass (item 8).

### Pre-existing test failure observed (not from this work)

On macOS, `native_protocol_rows_are_printable_and_bounded` panics at the
`leak_clean` assertion (`tests/perf.rs:223`) for the `http2_h2c_close_request`
connection-setup row, which reports `leak=unchecked`. This reproduces on pristine
`main` (`f96160c`) with all Phase 156 changes stashed, so it predates this work
and is unrelated to it (that row never touches `GrpcRouter`). The row data still
prints before the panic. Flagged here so the perf-proof pass can decide whether
to fix the macOS leak check separately.

## Implementation Note 3 (Session A — item 6, with an architectural limit)

### What changed

`tina-http/src/grpc.rs`: `encode_grpc_message_into(&mut Vec<u8>, message,
limits)` is now the public, canonical reusable framing primitive (the old
private `append_grpc_message`). `encode_grpc_message` is a thin wrapper that
sizes one exact `Vec`. `GrpcBufferedServerStreamingResponse::from_messages`
frames every message into one body through it (with explicit
`max_messages` / `max_body_bytes` caps and `TooManyMessages` /
`EncodeTooLarge` truth). `tina-http/src/grpc_client.rs`: added
`GrpcClient::frame_into(&mut Vec<u8>, message)` — the reusable form of `frame`,
for packing several messages into one client-streaming body. Re-exported
`encode_grpc_message_into`.

### What is directly proved

- `frame_into_packs_multiple_messages_into_one_buffer`: three dynamic messages
  framed into one buffer decode back to exactly those three (valid concatenated
  body).
- `frame_into_into_empty_buffer_matches_frame`: reusable form byte-matches the
  one-`Vec` form.
- `frame_into_rejects_over_cap_before_writing`: an over-cap message returns
  `EncodeTooLarge` and leaves the buffer's existing bytes untouched (cap enforced
  before any framing byte is written).
- grpc lib units (29) green; gRPC integration suites green; fmt + clippy clean.

### Architectural limit (honest, like item 3)

The plan also named `GrpcUnaryTemplate::request_into` and a reusable server
response buffer pool. A single request/response body that is **moved into a
message crossing an isolate boundary** cannot be pool-reused — it travels with
the message, and `std::mem::take`-ing a scratch buffer into it leaves the scratch
empty, so there is no allocation-count win (the per-call body Vec is already
sized exactly, one allocation). This is the same owned-body-ships wall as item 3.
So `request_into` for a single shipped unary body is a non-win and was omitted on
purpose; the reusable primitive is provided where it genuinely reduces
allocations: multi-message framing into one buffer (buffered server-streaming
response — a perf-exercised path — and caller-built client-streaming bodies).
A true per-call response pool would need the framed body to be written to the
wire from router-owned scratch without crossing back as an owned `Vec`, which is
a runtime/protocol change out of this phase's scope.
