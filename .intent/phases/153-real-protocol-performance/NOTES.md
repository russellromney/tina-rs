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

The Tina HTTP/2 *client* request path also dropped its per-byte
`VecDeque<u8>` outbound-body drain for an owned `Vec` + cursor with direct DATA
framing, and the inbound DATA clone is gone via `into_data_payload`. No current
perf row drives the Tina HTTP/2 client (every row uses a raw-socket client), so
that win is covered by the `tina-http` correctness suite (41 binaries green),
not a headline row — stated plainly rather than implied.

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

## Linux / x86_64

**MISSING in this session.** No Linux/x86_64 sample was collected (this was a
macOS/aarch64 build session). The PR is non-final until the Linux/Fly perf
bundle is run and saved here. Expected: the deterministic allocation/turn wins
(`perf-h2-alloc` 3075→3011, `perf-ws-turns` 133→67, per-row process-allocation
deltas) are platform-independent and should reproduce; the latency rows will
differ in absolute value.
