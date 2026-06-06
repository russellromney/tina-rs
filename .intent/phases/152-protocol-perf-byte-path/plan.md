# Phase 152: Protocol Perf Rows And Byte Path Cost

## Status

- Follows Phase 151.
- Phase 151 removed the worker wakeup gap. HTTP/1 no longer spends about 1ms
  asleep between kernel readiness events.
- The next costs are now visible: protocol workload rows are uneven, protocol
  internals still allocate/copy more than they should, and connection setup
  still costs real kernel round trips.

## Grug Truth

The worker is awake now. Stop blaming sleep.

Measure HTTP/2 and WebSocket like we measure HTTP/1. Remove the obvious byte
copies that still remain. Name the connection setup cost instead of hiding it
inside "HTTP is slow."

## Goal

Build the next native performance pass:

1. HTTP/2 and WebSocket equivalent workload rows.
2. Fewer-copy protocol internals beyond the migrated owned read/write rails.
3. Connection setup stage rows now that the old idle wait no longer hides them.

Done means:

- `examples/systems/perf_native` prints HTTP/2 and WebSocket rows with the same
  honesty as the HTTP/1 rows: same-work baseline where possible, p50/p90/p99,
  allocation counts, process rows, pressure/timeout truth, and leak-clean proof.
- HTTP/2/WebSocket/gRPC paths prove they use owned/reusable runtime I/O and
  reduce at least one measured protocol-internal allocation/copy source, or
  explain with evidence why none is safely removable in this phase.
- connection setup is measured separately from steady-state request work.
- no public production performance claim is made.

## Non-Goals

- no new scheduler;
- no new runtime park policy;
- no fake zero-copy claim if bytes are still copied;
- no HTTP/2 or WebSocket feature expansion unless needed to run the perf row;
- no benchmark-only code path that bypasses normal public service/client APIs;
- no broad Linux performance claim from one machine.

## Current Inventory

This phase is not allowed to spend its first week "discovering what exists."
The starting inventory on `main` is:

- `tina-http/src/connection.rs` uses `tcp_read_buf` / `tls_read_buf` and
  `tcp_write_owned` / `tls_write_owned` for HTTP/1 and server WebSocket.
- `tina-http/src/client.rs` and `tina-http/src/keepalive.rs` use the same
  owned/reusable helpers for HTTP/1 client paths.
- `tina-http/src/http2/server.rs` and `tina-http/src/http2/client.rs` already
  use `tcp_read_buf` / `tls_read_buf` and `tcp_write_owned` /
  `tls_write_owned`.
- `tina-http/src/websocket_client.rs` already uses `tcp_read_buf` /
  `tls_read_buf` and `tcp_write_owned` / `tls_write_owned`.
- Remaining byte cost is therefore mostly protocol-internal allocation/copy:
  HTTP/2 frame payload `to_vec`/`clone`, header/trailer `HeaderMap` churn,
  gRPC frame/message allocation, WebSocket frame payload/close reason copies,
  and response construction.

If implementation finds an old plain `tcp_read` / `tcp_write` path in
`tina-http`, fixing it is required. Otherwise do not claim this phase "migrated
compatibility helpers"; claim the narrower truth: owned runtime I/O is already
in use and this phase reduced protocol-internal byte cost.

## What Must Not Change

- HTTP/1 rows and semantics stay intact.
- HTTP/2 flow-control, reset, GOAWAY, trailers, and stream-close truth stay
  intact.
- gRPC status truth stays intact: status is not hidden inside success.
- WebSocket close, ping/pong, pressure, stale-session, and slow-peer truth stay
  intact.
- TLS half-duplex lane rules stay intact; do not double-arm TLS read/write.
- `Runtime::step()` remains nonblocking; threaded worker park remains the Phase
  151 readiness-driven shape.
- DST fingerprints change only when a public replay-visible fact really
  changes, and the PR explains why.

## Rock 1: HTTP/2 Equivalent Rows

Add native HTTP/2 perf rows in `examples/systems/perf_native`.

Use real Tina HTTP/2 service/client surfaces, not private shortcuts. The row
should be equivalent to an existing HTTP/1 row:

- small request/response;
- fixed body request/response if the public path supports it cleanly;
- keepalive / reused connection if HTTP/2 client/session reuse exists;
- same operation count and timeout budget as the comparison row.

First-form row names:

- `http2_h2c_close_request`
- `http2_h2c_keepalive_sequential`
- `http2_h2c_steady_state_small`

Use h2c first. TLS/mTLS is not required for this phase.

If the closest external comparison is hyper/tonic and would make the row much
larger, keep the first row Tina-only using a `perf-native` line with
`baseline=none reason=no_equivalent_baseline_yet`. Do not fake semantic
equality. If a baseline is added, the test must assert both sides do the same
number of successful operations with the same payload bytes.

Required output:

- `perf-compare` or `perf-native` line with stable schema;
- p50/p90/p99;
- allocation count and allocated bytes if available;
- stage count / scheduler gap count where the harness can collect it;
- leak-clean and zero timeout proof.

## Rock 2: WebSocket Equivalent Rows

Add native WebSocket rows:

- `websocket_open_close`;
- `websocket_text_round_trip`;
- `websocket_steady_state_small`;
- `websocket_slow_peer_pressure` if it can be CI-sized and deterministic.

Use the public WebSocket session/client surfaces. The row must exercise the
normal app session path, not only frame encoding helpers.

Required truth:

- successful messages counted;
- close/drain is clean;
- pressure is typed if the row intentionally fills outbound capacity;
- no hidden unbounded queue in the test harness.

If `websocket_slow_peer_pressure` is too timing-sensitive, replace it with a
deterministic capacity-fill row that uses the public send/report path and proves
typed pressure without sleeping on a slow client.

## Rock 3: Reduce Protocol-Internal Byte Cost

Do not start from a vague audit. Start from the inventory above and reduce at
least one measured source of protocol-internal allocation/copy in a normal
public path.

Known implementation targets:

- HTTP/2 frame decode/encode: `tina-http/src/http2/frame.rs`
  `to_vec`/payload clone sites;
- HTTP/2 server/client stream settlement: repeated response/trailer/header
  clone sites in `tina-http/src/http2/server.rs` and
  `tina-http/src/http2/client.rs`;
- gRPC-over-HTTP/2 frame/message construction in
  `tina-http/src/grpc_client.rs`;
- WebSocket client/server frame payload copies in
  `tina-http/src/websocket.rs`, `tina-http/src/websocket_client.rs`, and
  server WebSocket handling in `tina-http/src/connection.rs`;
- response/request construction in the perf workload if it allocates in a way
  real users would not need.

For each path:

- prefer reusing existing buffers, moving payloads instead of cloning, and
  building frames directly into pending-write storage where that keeps the
  protocol code simple;
- preserve failure truth: if a driver error cannot return the buffer yet, name
  that as the remaining broader error-envelope gap instead of silently dropping
  ownership facts;
- keep DST trace hashes stable unless the public call kind really changes.

Do not migrate by adding a benchmark-only fast path. Normal app code gets the
win.

Required proof:

- add or tighten allocation ceilings for the changed protocol row, process-wide
  when possible;
- add a unit/integration guard for the exact bug-prone transformation
  (for example, partial frame decode still waits for more bytes, fragmented
  WebSocket payloads still assemble correctly, trailers still settle gRPC
  status);
- include before/after row snippets in the phase notes or PR body.

## Rock 4: Connection Setup Rows

Add rows that separate:

- connect/open;
- accept;
- first read/write;
- steady-state reused connection work.

The point is not to make connection setup vanish. It is real kernel work. The
point is to stop mixing it with steady-state service cost.

Required proof:

- HTTP/1 close vs HTTP/1 keepalive rows still print;
- new HTTP/2/WebSocket rows say whether they include setup or reuse;
- include explicit labels for setup-heavy rows and reused-connection rows;
- stage breakdown names setup-heavy stages in the same vocabulary as existing
  hotpath rows;
- no timing assertion is so tight that shared CI becomes flaky.

## Rock 5: Perf History And Docs

Update:

- `.intent/phases/152-protocol-perf-byte-path/perf_history.jsonl`;
- `examples/systems/perf_native/README.md`;
- `ROADMAP.md` done/remaining text;
- `CHANGELOG.md`.

Docs must say:

- what improved;
- what did not improve;
- what still allocates/copies;
- which rows are macOS-only or Linux/x86;
- no production performance claim yet.

Also update `examples/systems/perf_native/tests/perf.rs` expected label lists
and schema assertions so the new rows cannot silently disappear.

## Proof

Run focused proof:

- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture`
- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture`
- protocol tests touched by the byte-path migration:
  - `cargo test -p tina-http --all-targets`
  - relevant `tina-runtime` TCP/TLS/Unix call tests if runtime byte APIs change

Add direct changed-path proof:

- at least one HTTP/2 public-path e2e row test asserts success count, clean
  shutdown, no timeout, and allocation evidence;
- at least one WebSocket public-path e2e row test asserts open/send/receive/
  close truth, clean shutdown, no timeout, and allocation evidence;
- if a byte-path optimization changes frame/header/body handling, add an
  adversarial protocol test for the edge it could break: partial frames,
  flow-control window updates, trailers/status, fragmented WebSocket messages,
  close frames, or slow-peer pressure.

Run regression proof:

- `cargo fmt --all --check`
- `cargo clippy -p tina-http -p tina-runtime --all-targets -- -D warnings`
- `make proof-fast`

If runtime call kinds or replay-visible events change, run the affected DST
tests and update fingerprints only with evidence in the phase notes.

Linux proof:

- collect at least one Linux/x86 release sample using the existing Fly or
  Ubuntu workflow and save it in the phase dir before merge readiness;
- if Linux cannot be run in the implementation session, the PR is not final:
  it must say "Linux sample missing" and the orchestrator must run it before
  merge.

## Done

- The named HTTP/2 and WebSocket rows exist, are asserted in the perf test
  label list, and print stable schema lines.
- Setup vs steady-state cost is visible.
- At least one real protocol-internal byte/allocation cost is reduced and
  proved, or the phase records evidence that the measured cost is elsewhere.
- Perf docs are honest.
- Existing HTTP/1 perf rows still pass.
- No deterministic simulator or proof-fast regression.
