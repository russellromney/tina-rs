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
- **after** = this phase (`af5043f`). Raw logs:
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

### perf-h2-alloc (64 warmed h2c buffered responses, whole-process; client byte-identical so the delta is all server-side)

| stage | allocations / 64 | per response |
| --- | ---: | ---: |
| Phase 152 baseline | 3075 | 48.05 |
| Phase 153 leaf copies | 3011 | 47.05 |
| + structural (decode + coalesce) | 2626 | 41.03 |
| + header encode (presize + itoa) | **2434** | **38.03** |

**−20.8% off the Phase 152 baseline (~10 fewer allocations/request).**

### Per-row whole-process allocations (Phase 152 → Phase 153 final)

| row | before (152) | after (153) | delta |
| --- | ---: | ---: | ---: |
| `http2_h2c_steady_state_small` allocations | 1570 | **1249** | **−20.4%** |
| `http2_h2c_steady_state_small` allocated_bytes | 426776 | **234072** | **−45.1%** |
| `http2_h2c_client_steady_state_post` allocations | 4266 | **3643** | **−14.6%** |
| `http2_h2c_client_steady_state_post` allocated_bytes | 2161066 | **1685168** | **−22.0%** |
| `grpc_h2c_unary_close` allocations | 5599 | **4964** | **−11.3%** |

The native-client row proves the client path too: buffered POST bodies ride the
client's owned-buffer/cursor DATA pacer, and response DATA is decoded through
the owned payload path. gRPC rides the same server response path, so it improves
too. HTTP/2 server, HTTP/2 client, and gRPC now drop by a double-digit /
meaningful chunk, not one allocation.

### Latency (median-of-5)

| row | kind | before p50 | after p50 | note |
| --- | --- | ---: | ---: | --- |
| `http2_h2c_steady_state_small` | reuse | 209 µs | **182 µs** | improved (warmed per-request signal) |
| `http2_h2c_client_steady_state_post` | reuse | 1287 µs | **1051 µs** | improved; public native client + server path |
| `grpc_h2c_unary_close` | setup | 829 µs | 911 µs | connect-bound; this is a per-op-connect row, so p50 swings 821–939 µs run-to-run on kernel connect/accept, not the changed code |
| `websocket_steady_state_small` | reuse | 262 µs | 180 µs | improved |

The steady-state (reused-connection) rows — the clean per-request signal —
improved or held. The `connection_setup` rows (gRPC, ws text) are dominated by
TCP connect/accept latency and swing run-to-run; their deterministic allocation
counts are the trustworthy signal, and those dropped. All rows `ok=32`,
`timeout=0`, leak-clean.

### What still dominates the residual (documented, not hidden)

After framing was made lean, the remaining server per-request allocations are
**not framing**:

- The inbound HPACK decode that builds the typed `HttpRequest`: the hpack
  crate's per-header name/value allocations, plus the `:path`/`:scheme`/
  `:authority` strings and the `HeaderMap`, all consumed by the app handler.
  The decoder itself is already reused per connection; the per-request header
  *model* is intrinsic to handing a typed request to user code.
- The per-request runtime call delivering the request to the service isolate
  and returning the response (out of scope: no scheduler/runtime changes).
- The raw-socket test client's own per-frame allocations (harness, not Tina).

These are why the whole-process number is ~38/request rather than near zero:
framing is no longer the cost. Reducing them further means a different HPACK
header model or a borrowed request view — a separate, larger change, not more
framing work.

## Linux / x86_64

Linux/x86_64 evidence is saved in `perf_sample_linux.txt`; Linux validation is
saved in `linux_validation.txt`.

Run:

- Fly app: `tina-perf-150`
- image: `registry.fly.io/tina-perf-150:deployment-01KTJB2YV7FC2KZGDV9DAW07KK`
- machine: `2870667a3092e8` (destroyed after capture)
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
| `hotpath_call_blocking_tail` | 28 µs | 31 µs | 76 µs | 1 | n/a | no scheduler-gap spikes |
| `http2_h2c_steady_state_small` | 80 µs | 163 µs | 1518 µs | 224 | 7072 | native server steady-state row is healthy; p99 noisy |
| `http2_h2c_client_steady_state_post` | 434 µs | 535 µs | 591 µs | 120 | 142768 | native client+server warmed POST path |
| `grpc_h2c_unary_close` | 949 µs | 1465 µs | 4832 µs | 608 | 25696 | fresh connection + public blocking gRPC helper |
| `http2_h2c_close_request` | 859 µs | 1126 µs | 1543 µs | 288 | 7648 | fresh h2c connection/preface/settings/request |
| `http2_h2c_keepalive_sequential` | 1397 µs | 1759 µs | 2130 µs | 960 | 28864 | fresh h2c connection + four sequential requests |
| `websocket_steady_state_small` | 129 µs | 212 µs | 356 µs | 96 | 352 | reusable session path is fast |

The deterministic allocation wins reproduce on Linux, and the validation suite
is green. The first Linux run surfaced an ugly ~88 ms delayed-ACK/Nagle floor on
the client/gRPC rows; the final run proves that bug is fixed by coalescing HTTP/2
client writes and setting TCP_NODELAY on Tina runtime sockets plus the blocking
std-client helpers used by gRPC/perf evidence.
