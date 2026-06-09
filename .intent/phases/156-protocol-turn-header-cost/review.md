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

## Implementation Note 4 (Session A — item 4 landed, biggest measured win)

### What changed

The HTTP/2 client no longer builds public `HeaderMap`s for a unary gRPC response.

- `tina-http/src/http2/headers.rs`: `HeaderBlock` gained `grpc_status: Option<u16>`
  and `grpc_message: Option<String>` facts, captured during decode for the
  `grpc-status` / `grpc-message` header names (requests never carry these, so the
  server path pays nothing; the message `String` is allocated only on errors).
- `tina-http/src/http2/client.rs`: a stream opened by `SubmitGrpcUnary` is marked
  `grpc_unary`. Its response head and trailers are decoded with
  `decode_headers_block_compact_with` and folded into compact facts
  (`apply_grpc_response_head` / `apply_grpc_response_trailers`) — no
  `response_headers` / `response_trailers` map, no per-header clone. It completes
  with a new `Http2ClientOutcome::GrpcUnaryReplied { status, grpc_status,
  grpc_message, body }`. The `GrpcFinalStatusReceived` protocol fact still fires
  (from the compact status). Generic streams are untouched: the variant is gated
  strictly on `grpc_unary`, and `Http2ClientOutcome::Replied` keeps its full
  headers/trailers for every non-gRPC-unary caller.
- `tina-http/src/grpc_client.rs`: `decode_unary` handles `GrpcUnaryReplied` via a
  shared `finish_unary` helper (status precedence + body decode), so the compact
  and public-header receive paths report identical status truth.
  `grpc_status_from_compact` percent-decodes the message the same way the
  header-map path does.

### Measured win

`grpc_h2c_unary_warmed` whole-process allocations: pristine `main` ~3949 →
~3313 after items 1/2/4/5/6 = **-16.1%**, of which item 4 contributes the largest
share (~3835 → ~3313). `http2_h2c_steady_state_small` unchanged (896 → 897):
generic HTTP/2 is untouched. See `perf_sample_macos_partial.txt`.

### What is directly proved

- Unit (`grpc_client`): `compact_grpc_unary_outcome_decodes_ok_message`,
  `..._surfaces_non_ok_status_with_message` (percent-decoded), `..._missing_status_on_200_is_malformed`,
  `..._non_200_without_status_synthesizes` — the compact path matches the
  public-header path's status truth on OK, non-OK+message, missing-status, and
  proxy-failure cases.
- The exhaustive in-crate `classify` test now covers `GrpcUnaryReplied`.
- e2e: `grpc_client_live` (10) and `grpc_live` (35) drive `SubmitGrpcUnary`
  through the real HTTP/2 client and stay green; `http2_client_adversarial`'s
  pre-connect-queue test was updated to expect the new compact outcome for a
  queued `SubmitGrpcUnary` (a correct behavior change, not a regression).
- Full `tina-http` suite green (592 tests); fmt + clippy clean (lib + perf
  example). The pool health classifier treats `GrpcUnaryReplied` as healthy, like
  `Replied`.

## Implementation Note 5 (Session A — item 7 diagnostic; reduction is partial)

### What changed

Added a warmed gRPC unary **turn probe**: `grpc_unary_warmed_turn_report()` in
the perf lib runs one steady-state unary call on a single runtime carrying the
gRPC server, the gRPC router, and the native client, with a live `TraceObserver`
armed only around the measured call. It counts `HandlerStarted` turns and
`CallKind::IsolateCall` policy crossings — the same "turn" definition every other
hotpath row uses. The `perf-grpc-unary-turns` hotpath row prints the count and
per-event timeline and guards `service_calls >= 4` (policy boundaries stay) and
`handler_turns <= 20` (regression ceiling on the measured baseline of 17). The
full timeline + analysis is saved in `grpc_unary_turn_timeline.txt`.

### What the timeline shows

Warmed unary = **17 handler turns, 4 service-isolate calls**. The 4 service calls
are the required policy boundaries (host->client connection, the I/O-hop calls,
server->gRPC router). The remaining turns are genuine cross-thread **I/O hops**:
the client (isolate 9) and server (isolate 12) connections alternate as TCP
segments cross loopback, each readiness wake one worker turn. After item 2
removed the public-`HttpRequest` rebuild, there is no shape-conversion
continuation left to delete on the buffered unary path — the request is
dispatched in the server read turn and the response framed in the router-reply
turn.

### Honest status: this item is NOT fully done

The plan requires an **actual** turn reduction (unary preferred, else streaming
or HTTP/2 steady-state). What is landed: the probe, the saved before timeline,
and the analysis showing warmed unary is policy/I-O bound, plus the exact future
runtime primitive that unary needs — a **same-worker co-located protocol-service
call** (deliver the decoded request to the router and pull its reply within one
worker turn when both live on the same shard, while still charging mailbox
capacity, timeout, and the `IsolateCall` trace fact). That is a runtime scheduler
primitive, not protocol code, so it is out of this phase's scope.

What is NOT landed: an actual turn drop in warmed gRPC streaming or HTTP/2
small steady-state. That is the remaining work for item 7, alongside the full
macOS+Linux 3x3 perf for item 8. The PR stays a draft.

## Implementation Note 6 (Session A — Linux/x86 evidence; item 8 partial)

Captured Linux/x86_64 before/after on a dedicated Fly `performance-2x` machine
(both images built on Fly's Depot builder; saved in `perf_sample_linux.txt`,
raw in `perf_sample_linux_before_raw.txt` / `perf_sample_linux_after.txt`).

The win reproduces off-macOS:

| row | Linux BEFORE | Linux AFTER | delta | (macOS delta) |
|---|--:|--:|--:|--:|
| grpc_h2c_unary_warmed | 4068 | 3417 | **-16.0%** | (-16.1%) |
| grpc_h2c_unary_pooled_concurrent | 4064 | 3421 | -15.8% | — |
| grpc_h2c_server_streaming_steady_state | 5472 | 5267 | -3.7% | (-3.7%) |
| http2_h2c_steady_state_small | 933 | 933 | 0 | (~0) |

Earlier phases warned Linux behaved differently from macOS; here the deterministic
allocation wins are the same shape on both. The Linux `hotpath` binary passed:
`perf-h2-alloc` 1858 (vs macOS 1730 — allocator/toolchain difference) and
`perf-grpc-unary-turns` 16 turns / 4 service calls (macOS 17/4 — platform-stable).

Also fixed a rustdoc intra-doc-link error (`[Replied]` -> `[Replied](Self::Replied)`)
that the PR's cross-platform `verify` CI caught on both runners — a real defect
from item 4's doc comment, now corrected.

Item 8 is still partial: this is one before/after batch (6 samples each) on one
machine, not the full 3x3 repeated runs with tabulated p50/p90/p99 + RSS per row.
The allocation wins are validated on both platforms; the latency distribution and
item 7's turn reduction remain. PR stays a draft.

## Implementation Note 7 (Session B — item 7 turn reduction landed)

Note 5 (Session A) showed warmed unary at 17 turns and called it policy/I-O
bound, deferring the actual reduction. That was incomplete: the second-largest
turn cost was not a policy boundary, it was a deferred flush. This note records
the real reduction.

### The removable turn

After the server writes a response, `handle_wrote` force-flushed the request's
accumulated receive-window credit as a *separate* connection `WINDOW_UPDATE`
write — a second `tcp_write_owned` and therefore a second write-completion turn,
plus the I/O wake it triggered. For a warmed small request the body credit sits
below `WINDOW_CREDIT_FLUSH_THRESHOLD`, so it is always deferred, so this extra
write happens every steady-state call, on both the client and the server
connection.

### What changed (`tina-http/src/http2/server.rs`)

- `handle_service_returned` flushes deferred request-window credit
  (`flush_deferred_request_window_credit`) right before issuing the response
  write, so the connection `WINDOW_UPDATE` is queued alongside the response.
- `write_more` coalesces every queued frame into one socket write, so the
  response and the window-update leave in a single syscall and a single
  completion turn.
- `handle_wrote` keeps the force-flush only as a fallback for credit that becomes
  pending *after* the response left (streamed bodies), and now re-issues the
  write of any short-write remainder before doing anything else — required
  because coalesced writes are larger and a partial write must keep draining.

This is same-turn protocol-local framing (the plan's named good target). No
service-handler is called from the connection isolate; mailbox capacity, request
caps, flow-control, timeout/cancel, and final gRPC status facts are untouched.
The flow-control change is strictly more permissive: the peer's send window is
replenished slightly sooner, never later.

### Proof (warmed turn probes, macOS/aarch64)

Extended the turn probe — reusing the Session-A `GrpcUnaryTurnObserver` (renamed
`ProtocolTurnObserver`, same `HandlerStarted` turn definition) — to HTTP/2 small
steady-state and gRPC server-streaming. Before/after, same probe, same
definition:

| row                          | turns BEFORE | turns AFTER | svc-calls |
|------------------------------|-------------:|------------:|----------:|
| grpc_h2c_unary_warmed        |           17 |          13 |  4 (kept) |
| http2_h2c_steady_state_small |           13 |          11 |  2 (kept) |
| grpc_h2c_server_streaming    |           25 |          21 |  6 (kept) |

All three warmed protocol rows drop, including the plan's preferred warmed gRPC
unary. Every policy-boundary `IsolateCall` is preserved. Full before/after
per-event timelines saved in `turn_reduction_timelines.txt`. The hotpath
regression guards were tightened to lock the wins in: warmed unary
`handler_turns <= 15` (was 20), new `http2 steady <= 13`, new
`server-streaming <= 23`, with the `service_calls` floors unchanged so a future
hop re-add fails the probe.

Full `tina-http` suite green (incl. flow-control, large-upload, streaming, and
all gRPC suites); fmt + clippy (lib + perf example) clean; rustdoc clean.

## Implementation Note 8 (Session B — item 3 landed; method-path interning)

Note 1 (Session A) deferred item 3, correctly observing that converting the path
to `Arc<str>` is a non-win on its own: the warmed path is already a single owned
`String` moved (not cloned) to the handler, and `Arc::from(&str)` allocates the
same as `String`. The missing piece was the bounded intern cache the plan
permits — that is what turns the per-request allocation into a refcount bump.

### What changed

- `tina-http/src/http2/headers.rs`: new bounded `PathInternCache` (one per server
  connection). `intern(&str) -> Arc<str>` returns a cloned cached `Arc` on a hit
  (a refcount bump, no allocation), inserts on a miss while under cap, and on a
  full cache serves a fresh non-retained `Arc` while counting `overflow` — an
  explicit cap with a visible, never-silent overflow, not a growing path map.
  `HeaderBlock.path` is now `Option<Arc<str>>`; the decode functions take an
  `Option<&mut PathInternCache>` and intern `:path` directly from the borrowed
  decode slice, so the compact gRPC path no longer allocates a per-request
  `String` at decode time.
- `tina-http/src/http2/server.rs`: `Http2Connection` owns a `PathInternCache`
  (cap 256) and passes it to both the compact and public request decodes;
  `Http2RequestParts.path` is `Arc<str>`. The generic request shape still owns a
  `String`, paid as one `to_string()` in `into_http_request` — so generic HTTP/2
  is unchanged. `Http2ConnectionReport` gained `path_intern_overflow`, folded
  from the live cache when a report is requested.
- `tina-http/src/grpc.rs`: `GrpcHttp2Request.path`, the public `GrpcRequest.path`,
  `GrpcStreamingCall.path`, `GrpcRawStreamingRequest.path`, and
  `GrpcClientStreamingRequest.path` are all `Arc<str>`, so the interned path
  flows unmodified from decode to the handler on the compact path. The generic
  (`HttpRequest`) entry points pay one `Arc::from` copy, matching the old
  `String` cost. Route lookups use `&*request.path` (a `&str`), unchanged
  matching against the `BTreeMap<String, _>` registries.
- The native client passes `None` (responses never carry `:path`).

### Why generic HTTP/2 does not regress

The warmed `http2_h2c_steady_state_small` process-allocation count is unchanged
(896-897 before and after) and `perf-h2-alloc` is unchanged at 1730. Generic
HTTP/2 uses the public decode (interned hit = 0 alloc) plus one `to_string()` at
`into_http_request` = one allocation per warmed request, the same as the old
`to_owned()`. Only the compact gRPC path reaches the handler as `Arc<str>` with
zero path allocation on a warmed route.

### Proof (hard seam, not code inspection)

- `path_intern_cache_reuses_one_arc_for_a_repeated_path`: two interns of the same
  path are `Arc::ptr_eq` — fails if a fresh owned path is rebuilt per call.
- `compact_decode_interns_a_repeated_request_path`: decoding the same gRPC request
  block twice through one connection's cache yields `Arc::ptr_eq` paths — the
  whole decode→intern seam reuses the allocation across warmed calls.
- `path_intern_cache_bounds_distinct_paths_and_counts_overflow`: a cap-2 cache
  serving 3 distinct paths counts the overflow, keeps serving correct paths, and
  reuses cached slots — proving the bound and the visible-overflow contract.
- `decode_without_cache_still_owns_the_path`: the `None` (client) fallback still
  produces a valid owned path.

### Measured (macOS/aarch64, native_protocol_rows process allocations)

Warmed gRPC unary dropped to ~2870 (from ~3313 after items 1/2/4/5/6 + item 7),
pooled-concurrent to ~2736, server-streaming to ~4690;
`http2_h2c_steady_state_small` unchanged at 896-897. Warmed turn counts unchanged
(13 / 11 / 21) — interning is an allocation change, not a turn change. Full
`tina-http` suite green (incl. `grpc_live` 35, `grpc_client_live` 10,
`http2_live` 41); fmt + clippy (lib + perf example) clean; rustdoc clean.
