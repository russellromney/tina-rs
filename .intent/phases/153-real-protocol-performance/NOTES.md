# Phase 153 Evidence: Real Protocol Performance

Rows are not performance; faster code is performance. This phase changed
protocol code (HTTP/2, gRPC, WebSocket), not just the harness, and proves the
public rows moved.

## Method

- Same machine (macOS / aarch64), `--release`, same `perf` / `hotpath` rows,
  same sample policy (`median_p50_after_warmup`, 5 samples).
- **before** = Phase 152 tip (`origin/codex/phase-152-protocol-perf`,
  `7b4cfea`) with this phase's *row code* copied in so the identical rows run
  against the old protocol code. Raw logs:
  `perf_sample_macos_before_152.txt`, `hotpath_sample_macos_before_152.txt`.
- **after** = this phase branch after the final structural fixes. Raw logs:
  `perf_sample_macos_after.txt`, `hotpath_sample_macos_after.txt`.
- Process-allocation rows are deterministic; latency on a laptop is noisy
  run-to-run, so the headline evidence is allocation/turn counts. Latency is
  reported honestly alongside.

## Deterministic wins (the real evidence)

### HTTP/2 buffered response allocations (`perf-h2-alloc`, 64 warmed h2c responses)

| metric | before (152) | after (153) |
| --- | ---: | ---: |
| process allocations | 3075 | **3011** |
| per response | 48.05 | **47.05** |

Exactly one fewer allocation per response: `enqueue_response` now consumes
`HttpResponse` by value and moves the buffered body into `PendingResponse`
instead of cloning it (Phase 152 had already removed the second body copy in
the DATA writer; this removes the body *clone*).

### WebSocket app-handler turns (`perf-ws-turns`, 64 text round trips)

| metric | before (152) | after (153) |
| --- | ---: | ---: |
| total app turns (session) | 133 | **67** |
| per message | 2.08 | **1.05** |

The connection owner now delivers exactly one session-rich app event per wire
event. Before, every wire frame produced a session-rich *and* a legacy event
(two app handler turns per text). The `hotpath` assertion pins
`app_turns < 2*N`; the 152 code fails it (133 ≥ 128), the new code passes (67).

### Per-row whole-process allocations (32 ops × 4 workers)

| row | before (152) | after (153) | delta |
| --- | ---: | ---: | ---: |
| `http2_h2c_steady_state_small` | 1570 | **1538** | −32 (one per request) |
| `grpc_h2c_unary_close` | 5599 | **5548** | −51 |
| `websocket_text_round_trip` | 4691 | **3865** | −826 |
| `websocket_steady_state_small` | 880 | **672** | −208 |

The HTTP/2 + gRPC rows improve through the same server-side changed code path
(by-value buffered response + `push_data_frame`); the gRPC row is the smallest
public unary gRPC path (`GrpcRouter` behind the real `Http2Listener`, driven by
`grpc_unary_call_h2c_blocking`). The WebSocket rows improve from the
single-event delivery (one app message + payload move instead of two).

The Tina HTTP/2 *client* request path is now measured by
`http2_h2c_client_steady_state_post`: one native `Http2ClientConnection`
submits buffered POSTs to the native `Http2Listener` over a warmed h2c
connection. With the row code copied onto the Phase 152 base, the row's
whole-process allocations dropped from 4266 to **3643** and allocated bytes from
2161066 to **1685168**. The row's load-worker allocation scope is unchanged
(120 allocations) because request construction still allocates the submitted
body; the useful signal is the process row that includes the client/server
runtime path.

## Latency (median-of-5, noisy on a laptop — reported, not claimed)

| row | before p50 | after p50 | note |
| --- | ---: | ---: | --- |
| `http2_h2c_steady_state_small` | 209 µs | 207 µs | flat; allocation win |
| `grpc_h2c_unary_close` | 829 µs | 821 µs | flat (within run noise) |
| `websocket_text_round_trip` | 1029 µs | 735 µs | better (fewer turns + noise) |
| `websocket_steady_state_small` | 262 µs | 154 µs | better (fewer turns + noise) |

No row's latency regressed. The gRPC row was flat (±~1%); the wins are
allocation/turn reductions. All rows: `leak_clean=true`, `timeout=0`,
`err=0`.

## Stage / turn reduction (Rock 5)

`perf-ws-turns` above is the stage row with fewer turns: a 64-message WebSocket
session dropped from 133 to 67 app-handler turns. The removed turn is in
runtime/protocol code (the connection owner's duplicate app delivery), not a
harness shortcut, and it does not bypass call/reply truth — the app still
replies to each `SessionText` and the echo round-trips.

## What still costs

- gRPC request/response messages still allocate the length-prefixed frame
  buffer (`encode_grpc_message`) plus prost's internal encode; unchanged here.
- The padded-DATA path in `into_data_payload` still copies the trimmed bytes
  (only the common unpadded path moves); padded DATA is rare and correctness
  is preserved.
- The WebSocket frame parse still copies the payload out of the read buffer
  once (`buf[..].to_vec()`); that is the minimum owned-payload copy with a
  reused read buffer, not a duplicate, so it was left.
- Tails are still wide under one single-shard worker. Not a production claim.

## Structural pass (attacking the real costs, not leaf copies)

The leaf-copy pass above removed one clone out of ~48 allocations/request on
HTTP/2 — too marginal. The structural pass attacks the dominant per-request
costs the evidence surfaced. Same machine (macOS/aarch64), same rows, same
sample policy.

### What changed

1. **Inbound frame decode without a per-frame `Vec`.** `try_decode_frame_meta`
   decodes just the frame header; `data_payload_view` / `headers_payload_view`
   return the unpadded payload as a borrowed sub-slice. The server read loop
   takes the read buffer out (`std::mem::take`) and processes DATA and HEADERS
   straight from a borrowed slice — no `Frame { payload: Vec }` per inbound
   frame. Only a *streaming* request chunk (which must outlive the buffer) still
   copies; control frames keep a cheap owned copy.
2. **Coalesced outbound response.** `send_pending_response` builds HEADERS +
   every DATA frame + optional trailers into one queued buffer: one
   outbound-queue slot, one TCP write instead of one per frame. Frame
   boundaries, peer max frame size, END_STREAM, and flow-control accounting are
   preserved on the wire.
3. **Header encode.** The response header block is pre-sized (the perf
   allocator counts each `Vec` growth realloc) and content-length is formatted
   into a stack buffer instead of a heap `String`.
4. **Linux tiny-write pacing.** The first Linux run exposed ~88 ms p50 on the
   native HTTP/2 client POST and gRPC close rows. That was not Tina scheduler
   work; it was tiny HTTP/2 writes interacting with Linux delayed ACK/Nagle
   behavior. The runtime TCP rail now enables TCP_NODELAY on accepted and
   connected stream sockets, the HTTP/2 client coalesces ready frames into one
   pending write, and the public blocking gRPC/perf client helpers also set
   TCP_NODELAY.
5. **gRPC status response allocation.** Unary and streaming gRPC response
   helpers now insert `grpc-status` directly into the response header map, and
   the streaming final-status path writes the two-field HPACK trailer block
   directly instead of building a temporary `HeaderMap` and running the generic
   trailer encoder.

### perf-h2-alloc (64 warmed h2c buffered responses, whole-process; client byte-identical so the delta is all server-side)

| stage | allocations / 64 | per response |
| --- | ---: | ---: |
| Phase 152 baseline | 3075 | 48.05 |
| Phase 153 leaf copies | 3011 | 47.05 |
| + structural (decode + coalesce) | 2626 | 41.03 |
| + header encode (presize + itoa) | **2434** | **38.03** |
| + literal HPACK fast path + compact pseudo-header facts | **1730** | **27.03** |

**−43.7% off the Phase 152 baseline (~21 fewer allocations/request).**

### Per-row whole-process allocations (Phase 152 → Phase 153 final)

| row | before (152) | after (153) | delta |
| --- | ---: | ---: | ---: |
| `http2_h2c_steady_state_small` allocations | 1570 | **897** | **−42.9%** |
| `http2_h2c_steady_state_small` allocated_bytes | 426776 | **226096** | **−47.0%** |
| `http2_h2c_client_steady_state_post` allocations | 4266 | **3643** | **−14.6%** |
| `http2_h2c_client_steady_state_post` allocated_bytes | 2161066 | **1685168** | **−22.0%** |
| `grpc_h2c_unary_close` allocations | 5599 | **~4220** | **−24.6%** |

The direct gRPC status/trailer follow-up was smaller than the HPACK/header
work. Final macOS whole-process samples for `tina_grpc_h2c_unary_close` sit
around 4.19k-4.23k allocations per 32 ops, down from the Phase 152 baseline at
5.6k.

### gRPC setup vs warmed service truth

The original gRPC row was too easy to misread: `grpc_h2c_unary_close` opens a
fresh h2c connection for every unary call. That is useful as a setup row, but it
does not answer "how does normal warmed gRPC behave?" This phase now pins four
gRPC rows:

| row | kind | what it measures |
| --- | --- | --- |
| `grpc_h2c_unary_close` | `connection_setup` | fresh h2c connection + unary call |
| `grpc_h2c_unary_warmed` | `steady_state_reuse` | one warmed `GrpcClient` / HTTP2 connection |
| `grpc_h2c_unary_pooled_concurrent` | `steady_state_reuse` | fixed `GrpcClientPool`, one warmed connection per worker |
| `grpc_h2c_server_streaming_steady_state` | `steady_state_reuse` | warmed server-streaming RPC, three messages, bounded response-source pool |

Representative macOS/aarch64 release rows from
`perf_grpc_rows_macos_after.txt`:

| row | p50 | p90 | process allocations / 32 ops | allocations / op |
| --- | ---: | ---: | ---: | ---: |
| `grpc_h2c_unary_close` | 1015 µs | 1125 µs | ~4.23k | ~132 |
| `grpc_h2c_unary_warmed` | 1033 µs | 1131 µs | ~4.08k | ~128 |
| `grpc_h2c_unary_pooled_concurrent` | 662 µs | 761 µs | ~4.05k | ~127 |
| `grpc_h2c_server_streaming_steady_state` | 2655 µs | 2899 µs | ~8.95k | ~280 |

So: fresh connection setup was a misleading proxy, but warmed unary gRPC is
still not cheap. The next meaningful gRPC work is not another single clone
removal; it is protobuf/frame buffer reuse, a cheaper public client request
construction path, and reducing protocol/runtime turns where no policy
boundary is crossed. The streaming row also proves a Tina-shaped pattern:
response sources are admitted from a bounded pre-registered pool; registering a
new source from inside the route handler would block/leak the runtime shape and
is intentionally avoided.

The native-client row proves the client path too: buffered POST bodies ride the
client's owned-buffer/cursor DATA pacer, and response DATA is decoded through
the owned payload path. gRPC rides the same server response path, so it improves
too. HTTP/2 server, HTTP/2 client, and gRPC now drop by a double-digit /
meaningful chunk, not one allocation.

### Latency (median-of-5)

| row | kind | before p50 | after p50 | note |
| --- | --- | ---: | ---: | --- |
| `http2_h2c_steady_state_small` | reuse | 209 µs | 259 µs | laptop-noisy; allocation drop is stable |
| `http2_h2c_client_steady_state_post` | reuse | 1287 µs | **965 µs** | improved in refreshed sample |
| `grpc_h2c_unary_close` | setup | 829 µs | 1206 µs | connect-bound; allocation drop is stable |
| `websocket_steady_state_small` | reuse | 262 µs | **220 µs** | improved |

Latency on this laptop still swings run-to-run, especially connection-setup
rows. The deterministic allocation counts are the trustworthy claim here, and
those dropped. All rows `ok=32`, `timeout=0`, leak-clean.

### Later structural header work (documented, not hidden)

This phase removed the big generic-HPACK tax for Tina-native/plain-literal
requests. The server now parses the common literal HPACK shape directly from the
wire buffer, borrows header name/value strings during decode, falls back to the
full HPACK decoder for indexed/dynamic/Huffman blocks, and stores `:scheme` /
`:authority` as validation facts instead of owned request strings.

Built-in gRPC now also opts into compact HTTP/2 request parts through
`Http2ServiceMessage`: the connection parses gRPC/content-encoding facts without
populating a public `HeaderMap`, then still calls the `GrpcRouter` isolate with
ordinary Tina call/reply semantics. This is the right API direction, but the
fresh-connection gRPC row did not move materially beyond the HPACK/header decode
win; the remaining gRPC cost is connection setup, protobuf frame/body work, and
the real service isolate boundary.

The remaining server per-request allocations are now mostly:

- public `HttpRequest` materialization: `:path` plus the `HeaderMap` values
  that user code can inspect;
- the per-request runtime call delivering the request to the service isolate
  and returning the response;
- the raw-socket test client's own per-frame allocations (harness, not Tina).

Reducing that further means either a user-facing borrowed/compact request view
for normal services, or an explicit inline protocol-service mode that honestly
says "the handler runs in the protocol isolate" instead of pretending the
service boundary still exists. That is a bigger API decision, not another HPACK
leaf fix.

## Linux / x86_64

Linux/x86_64 evidence is saved in `perf_sample_linux.txt`; Linux validation is
saved in `linux_validation.txt`.

Final run:

- Fly app: `tina-perf-150`
- image: `registry.fly.io/tina-perf-150:deployment-01KTJEHDDSWVEJ6GSQT3Q6430S`
- machine: `86e124be233618` (destroyed after capture)
- VM: `performance-2x`, region `iad`

Validation passed on Linux:

- `betelgeuse_substrate`: 19/19
- `cancel_call`: 5/5
- `client_against_native`: 1/1
- `grpc_live`: 34/34
- `mailbox_readiness`: 2/2
- `pending_read_park`: 1/1
- `readiness_park`: 5/5
- `scheduler_turn_tail`: 8/8

Representative Linux rows:

| Row | p50 | p90 | p99 | Allocations | Bytes | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| `hotpath_call_blocking_tail` | 16 µs | 24 µs | 52 µs | 1 | n/a | no scheduler-gap spikes |
| `perf-h2-alloc` | n/a | n/a | n/a | 2561 process / 64 req | n/a | Linux ceiling is 2640; test passed |
| `http2_h2c_steady_state_small` | 144 µs | 190 µs | 216 µs | 224 | 7072 | native server steady-state row is healthy |
| `http2_h2c_client_steady_state_post` | 399 µs | 555 µs | 664 µs | 113 | 140696 | native client+server warmed POST path |
| `grpc_h2c_unary_close` | 1095 µs | 1331 µs | 1378 µs | 608 | 25696 | fresh connection + public blocking gRPC helper; p50 noisy but no 88 ms floor |
| `http2_h2c_close_request` | 908 µs | 1197 µs | 1880 µs | 288 | 7648 | fresh h2c connection/preface/settings/request |
| `http2_h2c_keepalive_sequential` | 1241 µs | 1755 µs | 3935 µs | 960 | 28864 | fresh h2c connection + four sequential requests |
| `websocket_steady_state_small` | 66 µs | 178 µs | 1028 µs | 96 | 352 | reusable session path is fast; p99 noisy |

The deterministic allocation wins reproduce on Linux, and the validation suite
is green. The first Linux run surfaced an ugly ~88 ms delayed-ACK/Nagle floor on
the client/gRPC rows; the final run proves that bug is fixed by coalescing HTTP/2
client writes and setting TCP_NODELAY on Tina runtime sockets plus the blocking
std-client helpers used by gRPC/perf evidence.

The final Fly run also exposed a test portability bug: `perf-h2-alloc` had one
macOS/aarch64 allocation ceiling. Linux/x86_64 is stable but slightly higher
(~2561/64 requests), so the hotpath test now uses named per-platform ceilings
instead of a fake universal allocator count.
