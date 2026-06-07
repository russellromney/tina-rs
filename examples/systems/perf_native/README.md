# Native Perf Rows

Small release-mode performance rows for native Tina designs against bounded
Tokio designs.

This is not a production benchmark suite. It is alpha evidence:

- same op count
- same worker count
- bounded capacity on both sides
- pressure and leak truth printed beside timing
- median of five measured samples after warmup
- allocation counts for work done inside the load worker op
- semantic match labeled as `exact` or `partial`

Run:

```sh
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture
```

Or from the repo root:

```sh
make perf-compare
```

## Hot-path stage probes

The comparison rows say how slow a path is. The probes say *where the time
goes*. Three probes break a single op into stages, with a live trace observer
timestamping every worker turn:

```sh
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture
```

- `hotpath_try_send` — one bounded queue handoff.
- `hotpath_send_and_observe` — one observed admission.
- `hotpath_call_blocking` — one host call, every worker turn until `Replied`.

## The worker-loop sleep (found and fixed)

The first comparison run showed tiny same-shard work in milliseconds while a
bounded Tokio design did the same job in microseconds:

| row | before p50 | after p50 | Tokio p50 |
| --- | --- | --- | --- |
| `host_enqueue` | 84 ns | 84 ns | 83 ns |
| `observed_admission` | 1.34 ms | 53 µs | 20 µs |
| `host_request_reply` | 5.79 ms | 210 µs | 20 µs |
| `service_request_reply_chain` | 12.07 ms | 400 µs | 52 µs |
| `http1_close_request` | 13.35 ms | 0.80 ms | 0.70 ms |
| `http1_keepalive_sequential` | 38.88 ms | 2.16 ms | 1.99 ms |

(Local release run, Apple Silicon. HTTP rows vary run-to-run; the host
send/call rows are stable. Saved evidence lives beside the phase plan.)

The probes named the cause exactly. `hotpath_try_send` was 42 ns before and
after — the first queue handoff was never the problem. `hotpath_call_blocking`
showed three ~1.4 ms gaps between worker turns while every actual handler ran
in well under a microsecond:

```text
before: host_submit -> mailbox        1.17 ms   <- worker asleep
        ...handlers...                < 1 µs
        send_accepted -> mailbox       1.40 ms   <- worker asleep
        ...handlers...                < 1 µs
        effect -> mailbox              1.42 ms   <- worker asleep
```

The shard worker slept a flat 1 ms after every step that made progress. A tiny
call needs several turns, so each call paid one sleep per turn. The fix: loop
immediately while a step delivers work, and park on the command queue (with the
bounded `idle_wait`) only when there is nothing to deliver. The three gaps
collapsed to ~45 µs of cross-thread scheduling. HTTP fell ~10–14× as a free
downstream effect — it never needed its own tuning, it was waiting on the same
worker.

## Findings

What felt good:
- The probes turned "Tina is slow here" into "the worker sleeps 1 ms per turn"
  in one run. A live `TraceObserver` timestamping each turn is enough; no
  production instrumentation.
- The handoff/observe/call split isolated the cost: handoff was always cheap,
  so the fix belonged in the worker loop, not the ingress path.

What is still bad (named, not hidden):
- `host_request_reply` is still ~10× Tokio at roughly 200 µs p50. The per-call
  driver registration is gone, but the truthful Tina path still crosses from
  host thread -> shard worker -> target isolate -> dispatcher -> host.
- The Tokio comparison is partly architectural: Tokio's `current_thread`
  runtime has zero cross-thread host-call cost. Tina's `ThreadedRuntime` is
  shard-per-thread by design and pays worker-turn wakeups.
- The HTTP rows now have whole-process allocation rows. Tina HTTP still
  allocates roughly 1.45-1.8x the Axum comparison row on the same request.

**Rock 5 landed**: the per-call `HostCallDriver` registration is gone,
replaced by a pool of `HOST_CALL_DISPATCHER_POOL_SIZE = 8` long-lived
dispatcher isolates per worker, round-robin selected via a wrapping atomic
counter. Each `call_blocking` now pushes a `Box<dyn HostCallTaskBegin<S>>`
onto an already-registered dispatcher's mailbox instead of registering a
fresh isolate (mailbox + adapter + handler box + isolate entry + call-context
queue) per call.

DST replay parity was preserved by giving the simulator a
`reserved_system_isolates` knob so user-isolate ids stay equal in both live
and sim. The bugbox replay runner sets it to
`tina_runtime::HOST_CALL_DISPATCHER_POOL_SIZE` and the captured trace
replays bit-exact again. The bugbox's `SAVED_TRACE_HASH` was rebaked once
to reflect the shifted-but-now-deterministic ids.

Measured before/after the host-call dispatcher pool and reply channel pool:

| metric                              | before  | after   |
|-------------------------------------|---------|---------|
| `call_blocking` process allocs/call | 17      | **7**   |
| `call_blocking` host allocs/call    | 5       | **2**   |
| probe p50 (single host thread)      | ~190 µs | ~198 µs |
| perf row p50 (4 concurrent threads) | ~210 µs | ~205 µs |

## Phase 146 owned-buffer pass

The next pass added Tina-shaped owned-buffer calls:

- `tcp_read_buf` / `tls_read_buf`;
- `tcp_write_owned` / `tls_write_owned`.

Buffers move through effects and come back on success and on owned-helper
failure. Native HTTP/1 server, one-shot client, keepalive client, server
WebSocket, HTTP/2 server/client, and standalone WebSocket client paths use that
shape now. This removes the obvious "read fresh Vec, copy into HTTP buffer,
drop it" and "clone pending write bytes before every write" waste.

Current local truth is still not a production claim:

- HTTP/1 fixed-body close is closer to Axum in good runs.
- HTTP/1 close and keepalive are still slower/noisier than Axum.
- Whole-process allocation rows still show Tina HTTP around 1.4-1.8x Axum on
  these rows.
- Plain compatibility helpers still collapse owned-helper failures to
  `CallError` and discard their temporary buffers. Use the owned helpers when
  buffer reuse matters.

Suggested next follow-ups:
- Smaller HTTP request/response allocation shapes (`SmallVec` headers,
  response body reuse) before making any public HTTP performance claim.
- Reduce HTTP worker-turn/scheduling cost now that the obvious buffer waste is
  gone.
- Add more repeated-run history before any public performance claim.
- Verify Linux/x86_64 perf behavior; these rows were measured on macOS aarch64.

## Phase 147 HTTP turn/allocation pass

Phase 146 also left two evidence holes:

- `perf-process` rows counted allocations but not allocated bytes;
- checked-in history rows did not carry platform/arch/profile, so macOS and
  Linux could be mixed accidentally.

Phase 147 fixes those first. The default history now lives at
`.intent/phases/147-http-turn-allocation-cost/perf_history.jsonl`, and rows
carry `platform`, `arch`, and `profile`.

The hotpath test now also prints HTTP rows:

- `hotpath_http1_close_request`;
- `hotpath_http1_keepalive_sequential`;
- `hotpath_http1_fixed_body_close`.

These rows expose `stage_count`. On local macOS/aarch64 evidence, close and
fixed-body paths still take dozens of runtime-observed stages.

The first HTTP turn cleanup coalesces small no-metrics buffered responses into
one TCP write. Large buffered bodies still stay split so the server does not
copy a giant body into the head buffer, and metrics-enabled responses still
split so body-pressure accounting stays exact. Local hotpath evidence moved
fixed-body close from 33 to 28 observed stages and four-request keepalive from
111 to 91 observed stages. The generic close row is still noisy. That is
better, but the next real cost is still turn count plus remaining HTTP
allocation shape.

The perf test also prints an HTTP body-pressure probe. It sends requests that
exceed `max_body_bytes`, expects typed `full` pressure, projects the body
metrics into service-pressure surfaces, and asserts final current returns to
zero.

Current verdict: keep the evidence, keep optimizing, do not make a production
performance claim yet.

## Phase 148 performance rows

Phase 148 moves the append-only perf history to
`.intent/phases/148-native-performance-linux-turn-soak/perf_history.jsonl`.
`scripts/perf_record.sh` now records three row families:

- `perf-compare` rows: Tina-vs-baseline p50/p90/p99 and allocation counts.
- `perf-process` rows: whole-process allocation count, allocated bytes, and
  RSS delta for HTTP rows.
- `hotpath` rows: p50, stage count, host allocations, and process allocations.

Run and append local rows:

```sh
make perf-record
```

Check the current run against matching platform/arch/profile history:

```sh
make perf-check
```

Linux/x86 evidence is opt-in and manual for now. Maintainers can run the
manual GitHub workflow named `perf` on Ubuntu, or run `make perf-record` on a
Linux/x86_64 machine and keep the resulting JSONL rows with the review.

Phase 148 also removes one small HTTP/1 allocation source: coalesced buffered
responses reserve head + body capacity once instead of encoding the head and
then growing the buffer when the body is appended. This is a real cleanup, not
a production-speed claim. HTTP close/keepalive still spend many runtime turns,
and Linux rows still need repeated evidence.

## Phase 149 structural rows

Phase 149 keeps the same humility but measures sharper things:

- hotpath rows now print `event_stage_count`, `handler_turn_count`,
  `runtime_call_count`, `service_call_count`, `completion_count`, and
  `rejected_completion_count`;
- compare rows now include warmed keepalive steady-state workloads:
  `http1_keepalive_steady_state_small` and
  `http1_keepalive_steady_state_fixed`;
- HTTP/1 close-after-write can use a runtime terminal completion action,
  removing the `WroteClose` handler turn only when the TCP rail reports a full
  successful write and close.

Local macOS/aarch64 sample from the Phase 149 branch:

| row | Tina p50 | Axum p50 | note |
| --- | --- | --- | --- |
| `http1_close_request` | 0.84 ms | 0.56 ms | includes connect/accept; Tina p90 still much worse |
| `http1_keepalive_sequential` | 1.98 ms | 1.98 ms | four requests per op; Tina p90 still about 2x |
| `http1_fixed_body_close` | 0.80 ms | 0.72 ms | includes connect/accept; Tina p90 still much worse |
| `http1_keepalive_steady_state_small` | 0.36 ms | 0.39 ms | warmed stream; Tina p90 still much worse |
| `http1_keepalive_steady_state_fixed` | 0.37 ms | 0.42 ms | warmed stream; Tina p90 still much worse |

Hotpath stage truth from the same branch:

| row | stages | handler turns | runtime calls | service calls |
| --- | ---: | ---: | ---: | ---: |
| `hotpath_http1_close_request` | 26 | 4 | 3 | 1 |
| `hotpath_http1_fixed_body_close` | 26 | 4 | 3 | 1 |
| `hotpath_http1_keepalive_steady_state_small` | 16 | 3 | 1 | 1 |

Current verdict:

- the terminal close path is a real structural win for close-after-body rows;
- steady-state rows make the remaining request cost easier to see;
- Tina HTTP/1 p50 is now in the same neighborhood as the Axum comparison on
  these local rows;
- Tina HTTP/1 tail latency is still poor/noisy enough that this is not a
  production performance claim;
- process allocation rows are better than before but still not production
  proof;
- Linux/x86 rows still need repeated evidence before public performance claims.

## Phase 150 scheduler/turn/tail rows

This pass measures distribution shape, not just the median, and chases
scheduler cost rather than one more buffer reserve.

Tail rows. Hotpath reports now carry `p90_ns`/`p99_ns`, the
`p90_over_p50` / `p99_over_p50` / `range_over_p50` per-mille ratios, a
`scheduler_gap_threshold_ns` / `scheduler_gap_count` / `max_scheduler_gap_ns`
trio (gaps are inter-event stages above the threshold), and a `traced` flag.
Each key path emits a traced and an untraced `*_tail` row, so the observer's
own cost is visible and never folded into the headline. JSON schema is
`tina.hotpath.v2`; every v1 field is retained.

Host-call fast lane. `call_blocking` now travels a typed `ThreadedCommand`
variant instead of a boxed worker closure, so warmed `hotpath_call_blocking`
drops from host=2/process=6 to **host=1/process=5** allocations. The one
remaining host allocation is the type-erased begin task for the shared
dispatcher. `CALL_HOST_ALLOCATIONS_CEILING` is pinned at 2 to catch a
regression.

Linux/x86 evidence (Fly performance-2x, dedicated CPU; saved in the phase dir
`perf_sample_linux.txt`). Warmed `hotpath_call_blocking_tail` p50 about
25.7 µs -> 13.5-15.1 µs across two runs, host alloc 2 -> 1; HTTP rows steady at
~1.17 ms. Local/alpha, single machine, not a production claim.

Phase 150 found the dominant old HTTP cost: a wakeup gap, not request work. The
`hotpath_http1_close_request` p50 (~1.17 ms) was almost entirely one stage,
`host_submit -> mbox_accepted` (~1.09 ms): the worker re-polled the I/O loop on
a timer instead of waking on socket readiness, so an incoming connection waited
up to the park interval. `hotpath_call_blocking`'s same stage was ~12 µs
because a host command woke the worker immediately. A controlled
single-machine sweep (the opt-in `TINA_PERF_IDLE_REPOLL_US` env knob on the
hotpath probe; rows in `idle_repoll_ab_linux.txt`) showed the gap tracking the
park interval almost exactly — 1 ms -> http close p50 1.16 ms, 100 µs ->
0.23 ms (~5x).

That diagnosis led directly to Phase 151's readiness-driven worker park below.

## The wakeup gap, removed (readiness-driven park)

The re-poll gap above is now gone: the single-shard worker blocks on the
Betelgeuse I/O loop plus a command doorbell instead of polling on a timer, so a
ready socket wakes it at kernel latency and a fully idle worker makes zero
wakeups. On Linux/x86 (Fly performance-2x; rows in `perf_sample_linux.txt`) the
`host_submit -> mbox` / `call_completed` stage dropped from ~1.1 ms of worker
sleep to ~0.03-0.13 ms of real connection round-trips, and
`hotpath_http1_close` / `keepalive_steady_state` p50 are ~0.15 ms.

This is the removal of idle sleep the timer park spent on the hot path, not a
speedup of real work: the ~0.15 ms that remains is the connect/accept/read
round-trips the sleep had hidden. The single-digit-microsecond host-submit
stretch is not met for HTTP and should not be — HTTP is multi-round-trip; that
floor applies to one in-process hop, which `call_blocking` roughly hits at
~12-20 µs. The `TINA_PERF_IDLE_REPOLL_US` knob is now vestigial for the
single-shard park. Local/alpha, single machine, not a production claim.

## Phase 152 protocol rows and byte-path cost

The worker is awake now (Phase 151). This pass measures HTTP/2 and WebSocket the
way HTTP/1 is measured, separates connection setup from steady-state service
cost, and removes one real protocol-internal copy.

### Native protocol rows (`run_native_rows`)

These are Tina-only rows (`comparison_baseline=none`). A fair hyper/tonic or
tungstenite baseline would dwarf the row and make "equivalent workload" a lie,
so the first form stays Tina-only and says so. Each row drives the *real* Tina
server isolate (`Http2Listener`, or `HttpListener` + a WebSocket gateway) over a
raw socket client, the same shape the HTTP/1 rows use. Allocation counts include
the raw client operation too, and process rows include both client and server
work, so treat them as whole-operation evidence rather than server-only
allocation proof.

`kind` names the setup-vs-reuse class so connection setup is never silently mixed
with steady-state service cost:

| row | kind | what it includes |
| --- | --- | --- |
| `http2_h2c_close_request` | `connection_setup` | fresh h2c connection + preface/SETTINGS handshake + one request per op |
| `http2_h2c_keepalive_sequential` | `connection_setup_amortized` | one fresh connection, four sequential requests per op |
| `http2_h2c_steady_state_small` | `steady_state_reuse` | warmed reused connection, one request per op |
| `websocket_open_close` | `connection_setup` | TCP connect + HTTP/1.1 upgrade + close handshake per op |
| `websocket_text_round_trip` | `connection_setup` | connect + upgrade + one text echo + close per op |
| `websocket_steady_state_small` | `steady_state_reuse` | warmed open session, one text echo per op |

Each row is sampled like the comparison rows: one warmup run discarded, then the
median-of-five by p50 (4 load workers, 32 ops per run). The absolute latencies
vary heavily run-to-run and machine-to-machine — four raw clients share one
single-shard server worker, so the tails are wide and a busy machine can shift
the median several-fold. The recorded numbers live in
`.intent/phases/152-protocol-perf-byte-path/perf_history.jsonl`; do not treat any
single sample as a stable figure.

What the rows are *for* is the shape, not a speed claim:

- the `*_close` / `*_open_close` / `*_round_trip` rows (`connection_setup`) pay
  connect/accept/handshake on every op;
- the `*_steady_state_*` rows (`steady_state_reuse`) reuse a warmed connection
  and are the closest thing here to per-request service cost;
- so a steady-state row's p50 being well under its setup sibling's is the
  expected, honest result — connection setup is real kernel work, now measured
  separately instead of mixed into service cost.

This is local/alpha evidence, not a production performance claim.

### Deterministic WebSocket pressure row

`websocket_capacity_fill_probe` replaces the timing-sensitive slow-peer row with
a deterministic capacity-fill that uses the public send path and proves *typed*
pressure without sleeping on a slow client. Each op opens a session and sends
one `overfill` text; the echo reply is larger than the session's bounded
`max_queued_outbound_bytes`, so the connection raises a typed `SessionPressure`
to the app and closes without writing the over-cap frame. The row asserts two
independent facts: the client sees the no-echo/closed signal (counted as `full`
pressure), and the app's `SessionPressure` counter reaches one per op — the
server-side typed pressure surface, proving the pressure was real and not a
silently dropped frame.

### Byte-path reduction: buffered HTTP/2 response framing

The buffered HTTP/2 response path used to copy each body chunk twice: once into a
`Frame`'s `payload` `Vec` (`chunk.to_vec()`), then again inside `Frame::encode`
when it spliced the 9-byte header in front. The server now builds each DATA frame
straight into the queued buffer — header bytes via a new `push_frame_header`
helper, then `extend_from_slice(chunk)` — so a body chunk is copied once. At this
(Phase 152) step the per-frame `ensure_outbound_slots(1)` admission was kept, so
the bounded outbound-queue cap and the `connection_full` accounting were
byte-for-byte identical and the wire output unchanged. (Phase 153 below then
coalesces the whole buffered response into a single queued write, so a buffered
response now takes one outbound slot instead of one per frame — the queue-full
guard still applies, but the per-frame-count admission bound is gone. See the
Phase 153 section.)

Measured by the `perf-h2-alloc` check inside `hotpath_probes_report_and_stay_bounded`
(it calls `http2_steady_state_response_process_allocations`; the ceiling lives in
the single-test hotpath binary so whole-process counting is not contaminated by a
parallel test thread). 64 warmed h2c responses on one reused connection, stable
across runs on macOS/aarch64:

| | allocations / 64 responses | per response |
| --- | --- | --- |
| before | 3139 | 49.05 |
| after | 3075 | 48.05 |

Exactly one fewer allocation per buffered response. The ceiling test pins the
post-rewrite value with headroom and fails if the copy is re-added.
`http2_multi_frame_response_marks_end_stream_only_on_last_data_frame` is the
adversarial guard for the exact edges the rewrite touches: a patterned body must
reassemble byte-for-byte across several DATA frames, exactly one DATA frame (the
last) carries `END_STREAM`, and the HEADERS frame does not claim `END_STREAM`
while a body follows.

### What still copies (named, not hidden)

- The buffered response body is still `clone()`d once into `PendingResponse`
  (`enqueue_response` borrows the response); moving it out is a wider
  signature change left for a follow-up.
- `data_payload` still clones each inbound DATA payload on the unpadded path.
- The HTTP/2 streaming/chunked response path and the gRPC client request body
  still go through `data_frame` + `encode`.
- WebSocket control-frame payloads (ping/pong/close) are still cloned; that is
  the control path, not the data hot path.

### Rows that are platform-specific

All Phase 152 numbers above are macOS/aarch64 local/alpha. The
`H2_BUFFERED_RESPONSE_ALLOC_CEILING` in the hotpath test is calibrated on
macOS/aarch64 and is a regression guard (the regression is +64 over 64
responses), not a cross-platform constant. Linux/x86 evidence for this phase is
collected separately via the Fly/Ubuntu workflow and saved beside the phase
plan.

## Phase 153 real protocol performance

Phase 152 measured the problem. Phase 153 changed the protocol code: move
bytes instead of cloning them, allocate fewer things, take fewer turns — on the
public paths users call. Before/after rows are same-machine (macOS/aarch64),
same `--release` build, same rows, same sample policy; the full table and raw
logs live in `.intent/phases/153-real-protocol-performance/`
(`NOTES.md`, `perf_sample_macos_*`, `hotpath_sample_macos_*`).

This landed in two passes: a leaf-copy pass (move payloads instead of cloning
at named sites), then a structural pass that attacks the real per-request costs
the evidence surfaced (the leaf copies alone were ~one allocation out of ~48 —
too marginal). Numbers below are the Phase 152 baseline → Phase 153 final, same
machine.

### What got cheaper

- **HTTP/2 buffered response** (`perf-h2-alloc`, 64 warmed h2c responses):
  process allocations 3075 → **2434** (48.05 → **38.03**/response, **−20.8%**).
  The arc is 3075 → 3011 (body clone gone) → 2626 (borrowed inbound decode +
  coalesced response write) → 2434 (pre-sized header block + stack-formatted
  content-length). The client is byte-identical across the comparison, so the
  whole drop is server-side.
- **HTTP/2 steady-state row** (`http2_h2c_steady_state_small`): whole-process
  allocations 1570 → **1249** (**−20.4%**), allocated bytes 426776 → **234072**
  (**−45%**), p50 209 → **182 µs**.
- **Native HTTP/2 client row** (`http2_h2c_client_steady_state_post`): one
  native `Http2ClientConnection` submits buffered POSTs to the native server over
  a warmed h2c connection. With the row code copied onto the Phase 152 base,
  whole-process allocations 4266 → **3643** (**−14.6%**), allocated bytes
  2161066 → **1685168** (**−22%**), p50 1287 → **1051 µs**. The row's
  load-worker allocation scope is unchanged because request construction still
  allocates the submitted body; the process row is the useful client/server
  signal.
- **gRPC unary** (`grpc_h2c_unary_close`): the smallest public unary gRPC path
  (`GrpcRouter` behind the real `Http2Listener`, driven by
  `grpc_unary_call_h2c_blocking`). Whole-process allocations 5599 → **4964**
  (**−11.3%**) via the same server response path.
- **WebSocket** (`websocket_text_round_trip` 4691 → **3813**,
  `websocket_steady_state_small` 880 → **672** process allocations): the
  connection owner now delivers exactly one session-rich app event per wire
  event. It no longer also emits the legacy `Text`/`Binary`/`Close`/`Open`/
  `Closed`/`Pressure` duplicate, so the payload is moved into a single delivery
  instead of cloned.
- **Fewer turns** (`perf-ws-turns`, 64 text round trips): app-handler turns for
  the whole session 133 → **67** (2.08 → **1.05**/message). Removing the
  duplicate delivery removes one app turn per wire event; the coalesced HTTP/2
  response likewise drops a write turn (one write per response, not one per
  frame). The hotpath assertion pins `app_turns < 2*N`; the Phase 152 code fails
  it, the new code passes.

Steady-state (reused-connection) rows improved or held. The `connection_setup`
rows (gRPC, ws text) are dominated by TCP connect/accept latency and swing
run-to-run, so their deterministic allocation counts are the trustworthy signal
(and those dropped); per-op latency is reported in `NOTES.md`, not claimed. All
rows `ok=32`, `timeout=0`, leak-clean. Still local/alpha, not a production
performance claim.

### Byte-path changes (the code, not the harness)

- **Inbound frames decode with a borrowed view.** `try_decode_frame_meta`
  decodes just the header; `data_payload_view` / `headers_payload_view` return
  the unpadded payload as a sub-slice. The server read loop takes its buffer out
  (`std::mem::take`) and handles DATA and HEADERS straight from a borrowed slice
  — no `Frame { payload: Vec }` per inbound frame. Only a streaming request
  chunk (which must outlive the buffer) copies; control frames keep a cheap
  owned copy.
- **Coalesced outbound response.** `send_pending_response` frames HEADERS +
  every DATA frame + trailers into one queued buffer — one outbound slot, one
  TCP write — instead of one `Vec`/write per frame. Wire frame boundaries, peer
  max frame size, END_STREAM, and flow control are unchanged.
- **Header encode.** The response header block is pre-sized (each `Vec` growth
  realloc is a counted allocation) and content-length is formatted into a stack
  buffer, not a heap `String`.
- `into_data_payload(frame)` moves the unpadded DATA payload out of an owned
  frame (used by the client DATA handler); the old cloning `data_payload` is
  gone. Padded DATA still validates padding and preserves the flow-control wire
  length.
- The HTTP/2 *client* request body is an owned `Vec` + cursor with direct DATA
  framing, replacing the per-byte `VecDeque<u8>` drain; consumed/finished
  buffers are compacted/dropped. The
  `http2_h2c_client_steady_state_post` row drives this public native-client path
  directly, while the `tina-http` correctness suite still guards the protocol
  edges (incl. the 128 KB `large_upload_paces_through_real_window_updates`
  flow-control test).
- WebSocket ping echoes its payload into the pong from a borrowed slice
  (`encode_server_frame_from`) and moves the owned payload into the app `Ping`
  notification — no clone.

### What still dominates the residual (named, not hidden)

Framing is no longer the cost; the whole-process ~38 allocations/request are now
dominated by paths that are *not* framing:

- The inbound HPACK decode that builds the typed `HttpRequest`: the hpack
  crate's per-header name/value allocations, plus the `:path`/`:scheme`/
  `:authority` strings and the `HeaderMap`, all consumed by the app handler. The
  decoder is already reused per connection; the per-request header model is
  intrinsic to handing a typed request to user code.
- The per-request runtime call delivering the request to the service isolate and
  returning the response (out of scope: no scheduler/runtime change).
- The raw-socket test client's own per-frame allocations (harness, not Tina).
- gRPC messages still allocate the length-prefixed frame buffer
  (`encode_grpc_message`) plus prost's internal encode.

Reducing the first two means a different HPACK header model or a borrowed
request view — a separate, larger change, not more framing work.

### Platform

macOS/aarch64 local/alpha. Linux/x86_64 evidence for this phase is **not yet
collected** (see `NOTES.md`; local Fly was blocked by a missing access token);
the deterministic allocation/turn wins are expected to reproduce, the absolute
latencies will differ.
