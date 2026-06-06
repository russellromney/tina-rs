# Phase 152 Review

## Plan Review 1

Findings:

- [P2] The first plan could have become "add more rows" without changing any
  code. That would not be enough. The plan now requires byte-path migration for
  real protocol paths where compatibility helpers still copy or clone.
- [P2] "Equivalent workload" can become dishonest if the baseline is not
  semantically equivalent. The plan now allows Tina-only rows with a clear
  shape when a fair external baseline would be too large, and forbids fake
  semantic equality.
- [P2] Connection setup could be mistaken for a regression after Phase 151 made
  it visible. The plan now requires explicit setup vs steady-state rows and
  stage naming.
- [P2] WebSocket perf rows could accidentally test only frame helpers. The plan
  now requires the normal public session/app path.
- [P3] Linux proof could be implied by old Phase 151 evidence. The plan now
  requires at least one Linux/x86 sample for this phase, or a named pre-merge
  gap.

Decision:

- Plan is implementation-ready. It is not a planning phase. It builds rows,
  migrates byte paths, records setup cost, and updates docs with honest
  non-claims.

## Plan Review 2

Findings:

- [P2] The plan still had a stale premise: HTTP/2 and standalone WebSocket
  already use `tcp_read_buf` / `tls_read_buf` and `tcp_write_owned` /
  `tls_write_owned` on current `main`. The actual remaining byte-path work is
  protocol-internal allocation/copy, not broad migration off plain
  `tcp_read`/`tcp_write`. The plan now includes a current inventory and changes
  Rock 3 to reduce measured protocol-internal byte cost.
- [P2] "Find protocol paths" was a planning/audit step inside an implementation
  phase. The plan now pins the known files and copy/allocation families:
  HTTP/2 frame payload copies, header/trailer churn, gRPC frame allocation, and
  WebSocket payload/close copies.
- [P2] Row requirements were too soft. A worker could add one vague line and
  call it done. The plan now names first-form row labels for HTTP/2 and
  WebSocket, requires stable schema shape, and requires the perf test's label
  assertions to be updated so rows cannot silently disappear.
- [P2] Byte-path changes had weak direct proof. The plan now requires allocation
  ceilings/evidence for changed rows plus adversarial protocol proof for the
  exact edge the optimization could break: partial frames, flow control,
  trailers/status, fragmented WebSocket messages, close frames, or slow-peer
  pressure.
- [P2] "Linux proof if possible" was too weak for a performance phase. The plan
  now says Linux/x86 sample is required before merge readiness; if the builder
  cannot run it, the PR must remain non-final until the orchestrator does.
- [P3] Non-change guarantees were too broad. The plan now names what must not
  change: HTTP/2 flow control/reset/GOAWAY/trailers, gRPC status truth,
  WebSocket close/ping/pressure/stale-session truth, TLS half-duplex rules,
  `Runtime::step()` nonblocking behavior, and Phase 151 worker park behavior.

Decision:

- Plan is stronger and more Tina-like now. It is still big enough to matter,
  but it no longer sends the implementer on an open-ended audit. The phase must
  produce rows, reduce or honestly localize byte cost, and prove protocol
  semantics did not regress.

## Implementation Review 1

### What changed

- `examples/systems/perf_native` gains six Tina-only native protocol rows
  (`run_native_rows`): `http2_h2c_close_request`,
  `http2_h2c_keepalive_sequential`, `http2_h2c_steady_state_small`,
  `websocket_open_close`, `websocket_text_round_trip`,
  `websocket_steady_state_small`. Each drives the real server isolate
  (`Http2Listener` / `HttpListener` + WebSocket gateway) over a raw socket
  client, exactly as the HTTP/1 rows drive the real server over a raw
  `TcpStream`. Rows carry a setup-vs-reuse `kind`.
- `websocket_capacity_fill_probe`: deterministic typed-pressure row.
- Byte-path reduction in `tina-http/src/http2/server.rs` +
  `tina-http/src/http2/frame.rs`: the buffered HTTP/2 response builds each DATA
  frame straight into the outbound queue via a new `push_frame_header`, removing
  one body-sized allocation per DATA frame. `Frame::encode` is refactored onto
  the same helper (no behaviour change).
- Proof: `http2_multi_frame_response_marks_end_stream_only_on_last_data_frame`
  (tina-http) and the `perf-h2-alloc` ceiling inside the hotpath test.
- `scripts/perf_record.sh` learns the `native` row family; the phase
  `perf_history.jsonl` is recorded.

### What did not change

- HTTP/1 rows and semantics. HTTP/2 flow-control / reset / GOAWAY / trailers /
  stream-close. gRPC status. WebSocket close/ping/pressure. TLS half-duplex.
  `Runtime::step()` nonblocking and the Phase 151 worker park. The HTTP/2 wire
  output is byte-identical (only the allocation that builds it changed), so no
  replay-visible fact moves; the 40-test `http2_live` suite and `proof-fast`
  protocol-regression corpus are the guard.

### Directly proved

- `cargo test --release --test perf` and `--test hotpath` green; all six native
  rows do 32/32 ok ops, zero err, zero timeout, leak-clean, with p50/p90/p99 and
  allocation evidence; labels and setup/reuse classes asserted.
- Byte-path win is exact and deterministic: 3139 -> 3075 process allocations
  over 64 warmed h2c responses (one fewer per response), pinned by a ceiling.
- `http2_live` 40/40 including the new multi-frame END_STREAM guard.

### Hostile findings and resolutions

- [FIXED, was P1] The allocation-ceiling test first lived in `perf.rs` and
  failed in the full `make perf` run: whole-process allocation counting is
  contaminated when cargo runs the other `perf.rs` tests on parallel threads. It
  passed alone (3075) but inflated in parallel. Moved into the single-test
  `hotpath.rs` binary, which runs sequentially in its own process, so the
  process counter is clean — the same reason the existing host/process
  allocation probes live there. Re-verified green inside `make perf`.
- [P2 -> accepted] The HTTP/2 perf row's success check verifies reassembled
  body length and absence of RST_STREAM, not a decoded `:status`. Decoding the
  HPACK status in the raw client would couple the row to a specific HPACK
  encoding and is fragile; full status/semantics are covered by the `http2_live`
  suite. The perf row asserts "the server produced the expected body bytes with
  no reset," which is sufficient for a throughput/allocation row and is the same
  convention the existing live `read_response_body` helper uses. Named, not
  hidden.
- [P2 -> resolved] The native rows are Tina-only (`comparison_baseline=none`).
  This is the plan-blessed honest form, not a benchmark-only shortcut: the rows
  exercise the real public server isolate over a standard external client (raw
  framing), the same shape as the HTTP/1 rows. The raw client is not a private
  Tina fast path.
- [P2 -> resolved] Allocation-ceiling portability. The counted value is the
  number of Rust-level `alloc` calls on the steady-state path (one per `Vec`),
  determined by the code, not the OS allocator; it is deterministic across runs.
  The ceiling is a regression guard with the before/after recorded. The Linux
  sample (below) will confirm the absolute value; if a platform legitimately
  differs the ceiling is updated with a recorded before/after, as the existing
  hotpath ceilings are.
- [P3 -> accepted] `ws_send_close` returns `Ok` on a read error during the
  close drain. That is the post-close handshake read only (EOF == clean close);
  connect/upgrade/send failures surface earlier and fail the op. The setup rows
  still report 32/32 ok with zero err.
- [P3 -> resolved] No hidden unbounded queue in the harness: steady-state rows
  reuse a bounded per-worker `Mutex<Option<stream>>`; the load runner is
  op-bounded; HTTP/2 stream ids increase monotonically within the op budget and
  stay under `max_concurrent_streams` because requests are sequential.
- [P3 -> resolved] The `native` row family in `perf_record.sh` first also
  matched the per-side `perf ` lines of the comparison rows. Restricted to the
  setup/reuse `kind` allowlist so only the six protocol rows are recorded.

### Remaining cost (named, not hidden)

- Buffered response body is still `clone()`d once into `PendingResponse`
  (`enqueue_response` borrows it); moving it out is a wider signature change.
- `data_payload` still clones each inbound DATA payload on the unpadded path.
- HTTP/2 streaming/chunked response framing and the gRPC client request body
  still go through `data_frame` + `encode`.
- WebSocket control-frame payloads (ping/pong/close) are still cloned (control
  path, not the data hot path).
- Native protocol rows have wide tails: four raw clients share one single-shard
  server worker. These are local/alpha numbers, not a production claim.

### Linux proof

- Linux sample missing. This session runs on macOS/aarch64 and cannot produce a
  Linux/x86 release sample without an outward Fly deploy. The PR is therefore
  NOT final: the orchestrator (or a follow-up) must run the Fly/Ubuntu perf
  workflow and save the sample beside this plan, and confirm the
  `H2_BUFFERED_RESPONSE_ALLOC_CEILING` value holds on Linux/x86, before merge.

## Implementation Review 2 (adversarial pass + fixes)

Two independent hostile reviewers re-read `origin/main..HEAD` (one on protocol
correctness, one on benchmark honesty). Both confirmed the byte-path rewrite is
byte-identical to the old path, the multi-frame END_STREAM test is a real guard,
and there is no hidden unbounded queue. Findings fixed in this wave:

- [P1] README claimed a "median of five samples after warmup" methodology, but
  the native rows ran once, and the documented absolute latencies were an
  unreproducible single-machine snapshot (a reviewer measured up to ~12x
  different). Fixed: `run_native_rows` now warms once and takes the
  median-of-`SAMPLES` per row via `native_sampled`, matching the comparison
  rows. The README latency table is replaced with a shape description plus a
  pointer to `perf_history.jsonl` and an explicit "varies heavily, not a stable
  figure" caveat.
- [P2] CHANGELOG/README named a proof `http2_buffered_response_allocation_ceiling`
  that does not exist (the guard is the `perf-h2-alloc` assertion inside
  `hotpath_probes_report_and_stay_bounded`). Docs now name the real location.
- [P2] The WebSocket pressure proof counted both `SessionPressure` and the legacy
  `Pressure(_)` spelling and used `>=`, so it did not specifically prove the
  typed surface and could not catch a double-fire. Fixed: count only the typed
  `SessionPressure`; `leak_clean` now requires `== PRESSURE_OPS` (exactly one
  typed event per op).
- [P2] `ws_overfill_op` mapped every read error — including a 2s read timeout /
  hang — to the "pressure happened" outcome. Fixed: `ws_read_frame` now returns
  `io::Result`, and only a clean `UnexpectedEof` (or a CLOSE frame) counts as
  pressure; a timeout or other I/O error surfaces as a real failure.
- [P2] `ops_err == 0` / `ops_timeout == 0` on a timing-bound path with 2s socket
  timeouts could flake on a contended CI runner. Fixed: native protocol clients
  now use a 5s `PROTOCOL_CLIENT_TIMEOUT`, well above the worst observed tail; the
  zero-shed/zero-timeout assertions (the plan's requirement, and the existing
  HTTP/1 convention) are kept.
- [P3] `h2c_get` checked only reassembled body length. Now it also requires a
  HEADERS frame and an END_STREAM terminator, so a headerless/truncated response
  cannot pass. Decoding `:status` was deliberately left out (couples the row to
  an HPACK encoding); full HTTP/2 semantics remain covered by `http2_live`.
- [P3] The allocation-ceiling comment overstated "platform-stable". Reworded to
  "stable across runs on this toolchain; regression guard, recalibrate
  elsewhere" and the ceiling widened to 3130 (still inside the (3075, 3139)
  window that catches the +64 regression) for toolchain/std headroom.

Re-verified after the fixes: `make perf` (perf + hotpath, incl. median-of-five
rows and the alloc ceiling), `cargo test -p tina-http --all-targets`,
`cargo fmt --all --check`, `clippy -p tina-http -p tina-runtime`, `make proof-fast`
all green on macOS/aarch64. Linux sample still missing — PR remains non-final.
