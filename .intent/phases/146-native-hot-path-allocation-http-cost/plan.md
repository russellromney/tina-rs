# Phase 146: Native Hot-Path Allocation And HTTP Cost Reduction

## Status

- Future implementation plan.
- Runs after Phase 145.
- One PR unless the reusable-I/O-buffer API forces a narrow runtime/simulator
  PR first.
- Owns native performance rows, runtime TCP/TLS I/O buffer reuse, HTTP/1
  allocation/copy reduction, and proof that speed did not weaken Tina truth.

## Grug Truth

Phase 145 found the dumb millisecond sleep tax and killed it.

Now Tina is fast enough to see the next waste:

- socket reads allocate fresh `Vec<u8>`;
- HTTP copies those bytes into its own buffers;
- HTTP writes clone pending bytes on every partial write;
- header/body shapes still allocate more than the bounded Tokio comparison;
- host/call paths are better, but still have real turn cost.

Fix the waste Tina owns.

Do not cheat:

- no removing trace facts;
- no hiding pressure;
- no unbounded buffers;
- no fake benchmark wins;
- no comparing truthful Tina to an untruthful Tokio shape.

## Goal

Make the simplest native Tina service path credible.

User story:

```text
I can run a basic native Tina service and see local performance evidence that is
not absurd, while still getting Tina's bounded/cancel/replay truth.
```

The phase may claim:

- tiny host send/call stays sub-millisecond;
- HTTP/1 no longer does obvious extra read/write allocation and clone work;
- perf rows separate semantic cost from implementation waste;
- overload perf rows still show `Full` / `Closed` / `Timeout` truth;
- the next bottleneck is named from evidence.

## What Will Not Change

This phase changes cost, not Tina's semantics.

Do not change:

- public `CallOutcome` meaning;
- service `Full` / `Closed` / `Timeout` / `Rejected` mapping;
- request-scope cancellation truth;
- trace/replay stable tags except append-only additions for new call variants;
- HTTP/1 parser strictness;
- HTTP body-pressure accounting;
- HTTP keepalive close/retire semantics;
- TLS certificate/SNI/ALPN verification truth;
- simulator replay determinism;
- bounded admission and explicit buffer ownership.

If a proposed optimization needs one of those to change, stop and split a
separate semantic phase.

## Starting Facts

- `examples/systems/perf_native` exists and records native Tina vs bounded
  Tokio rows.
- `make perf`, `make perf-compare`, `make perf-record`, and `make perf-check`
  exist.
- Phase 145 fixed the worker-loop progress sleep tax and replaced per-call
  host-call driver registration with a dispatcher pool plus host reply channel
  pool.
- Remaining Phase 145 evidence:
  - `host_request_reply` is around hundreds of microseconds, still much slower
    than the bounded Tokio comparison;
  - HTTP rows are much improved but still allocate roughly 1.45-1.8x the Axum
    comparison;
  - current TCP read path returns a fresh `Vec<u8>`;
  - `HttpConnection::handle_bytes_read` immediately copies that `Vec<u8>` into
    its reusable `read_buf`;
  - `HttpConnection::write_pending` clones `pending_response` before every
    write;
  - client/keepalive paths have the same broad read/write shape.
- Effects are owned values. A borrowed `&mut [u8]` must not cross the runtime
  effect boundary.

## Planning Decisions Already Made

No implementation session should spend this phase deciding these again:

- Reusable I/O uses **owned buffers that move through effects and come back**.
  No borrowed `&mut [u8]` crosses a runtime call boundary.
- HTTP server reads use a per-connection `read_scratch` buffer. The scratch
  buffer moves into `tcp_read_buf` / `tls_read_buf` and comes back with `len`.
  Existing partial request bytes stay in `read_buf`.
- HTTP writes move the pending wire buffer into the runtime call and receive it
  back with the accepted byte count. No clone-before-write.
- TLS gets the same public owned plaintext read/write API in this phase.
  The claim is "no caller-side fresh plaintext buffer/clone"; rustls may still
  keep internal ciphertext/plaintext buffers.
- Header-map replacement is out of scope. This phase does encoder/header-byte
  allocation cleanup only. If `HeaderMap` remains the dominant cost, seed a
  later phase from evidence.
- Same-turn continuation / "stay in handler" is out of scope. This phase records
  handler-turn counts; it does not add a new runtime control-flow primitive.
- Perf history for this phase lives under
  `.intent/phases/146-native-hot-path-allocation-http-cost/perf_history.jsonl`.
  Scripts may accept an override, but the default should move to the current
  phase evidence file.

## Likely Files

Runtime I/O:

- `tina-runtime/src/call/tcp.rs`
- `tina-runtime/src/call/tls.rs`
- `tina-runtime/src/call/io.rs`
- `tina-runtime/src/driver/tcp.rs`
- `tina-runtime/src/driver/tls.rs`
- `tina-runtime/src/tcp_loops.rs`
- `tina-sim/src/...` call/replay support for new call variants
- runtime TCP/TLS tests

HTTP:

- `tina-http/src/connection.rs`
- `tina-http/src/client.rs`
- `tina-http/src/keepalive.rs`
- `tina-http/src/parse.rs`
- HTTP server/client/keepalive/body/WebSocket tests

Perf/proof:

- `examples/systems/perf_native`
- `tina-proof-harness/src/perf.rs`
- `scripts/perf_record.sh`
- `scripts/perf_check.sh`
- `.intent/phases/146-native-hot-path-allocation-http-cost/perf_history.jsonl`

## Rock 0: Make Perf Rows Harder To Lie With

Tighten the existing perf harness before claiming wins.

Required:

- `make perf` / `make perf-compare` print:
  - p50 / p90 / p99 / max;
  - process allocations;
  - process allocated bytes;
  - worker-thread allocations where the probe can honestly see them;
  - RSS delta;
  - semantic match label;
  - git sha, platform, and profile.
- `make perf-record` writes rows with all fields needed for later comparison.
- `make perf-check` compares against the checked-in baseline.
- extend `LoadReport` / perf report output with `latency_p90_us` and
  `latency_p90_ns`.
- Perf history stores platform-specific rows separately. Do not merge macOS and
  Linux into one number.
- perf history default path moves to this phase's evidence file; keep an env
  override (for example `TINA_PERF_HISTORY_FILE`) so future phases do not edit
  scripts just to change the evidence path.
- The README explains which rows are framework hot paths, which are HTTP rows,
  and which are only partial semantic matches.

Proof:

- perf rows are median-of-samples after warmup;
- no row labels worker-only allocations as the whole allocation cost;
- one test proves JSON / grep output still includes boundedness fields.

## Rock 1: Owned Reusable TCP/TLS I/O Buffers

Add reusable owned buffers to the runtime call surface.

Do not pass borrowed buffers through effects. Use owned buffers that move into
the call and come back.

Target shape:

```rust
tcp_read_buf(stream, buffer, max_len) -> TypedCall<TcpReadBufReply>
tcp_write_owned(stream, bytes) -> TypedCall<TcpWriteOwnedReply>
tls_read_buf(stream, buffer, max_len, timeout) -> TypedCall<TlsReadBufReply>
tls_write_owned(stream, bytes, timeout) -> TypedCall<TlsWriteOwnedReply>
```

Where the reply owns the buffer again:

```rust
TcpReadBufReply {
    buffer: Vec<u8>,
    len: usize,
}

TcpWriteOwnedReply {
    bytes: Vec<u8>,
    written: usize,
}

TlsReadBufReply {
    buffer: Vec<u8>,
    len: usize,
}

TlsWriteOwnedReply {
    bytes: Vec<u8>,
    written: usize,
}
```

Rules:

- `max_len` is still a service-owned cap.
- The returned `len` names the valid prefix of `buffer`.
- EOF remains `len == 0`.
- Errors return enough ownership truth to avoid leaking capacity or stranded
  bytes. If returning the buffer on every error makes the public shape noisy,
  document and test where the bytes are dropped.
- Existing `tcp_read` / `tcp_write` / `tls_read` / `tls_write` stay as
  compatibility helpers and may be implemented by allocating a temporary owned
  buffer internally.
- Add stable trace/call tags append-only if new call variants are needed.
- Simulator gets byte-identical semantics for the new calls.
- On error, return the buffer in the typed reply if the call reached the driver
  and the runtime still owns the buffer. For submission failures that return
  `CallError` before driver ownership is established, dropping the buffer is
  acceptable only through the compatibility helper; the owned-buffer helper must
  preserve ownership where possible. Test this rule.

Proof:

- TCP and TLS live read/write success, EOF, partial write, close, cancel, and
  shutdown tests.
- Simulator parity for read/write buffers.
- Stable trace hashes update only where the new call variant is used.
- No unbounded buffer growth; callers own the buffer capacity.

## Rock 2: Move HTTP/1 Server To Reusable I/O

Use Rock 1 in the native HTTP/1 server.

Target files:

- `tina-http/src/connection.rs`
- `tina-http/src/parse.rs` only if encoder sizing changes are needed
- HTTP/1 server tests
- `examples/systems/perf_native`

Required:

- Add `read_scratch: Vec<u8>` to `HttpConnection`.
- `HttpConnection` uses `tcp_read_buf` / `tls_read_buf` with
  `std::mem::take(&mut self.read_scratch)`.
- The returned scratch buffer is retained for the next socket read.
- If `self.read_buf` is empty, move or swap the scratch bytes into `read_buf`
  where possible. If partial head/body bytes already exist, append only the
  valid prefix from scratch.
- `write_pending()` stops cloning the full pending response before every write.
- Add an in-flight write state if needed so `pending_response` can move into the
  runtime and return through the write reply.
- Partial writes still drain exactly the accepted prefix.
- Body-pressure accounting stays exact.
- Known-length, chunked, WebSocket, request-body streaming, and keepalive paths
  keep their current wire behavior.
- Request body pull stops using `drain(..take).collect()` for the common
  front-chunk case. Use a bounded buffer/split shape that avoids shifting the
  whole remaining body on every chunk. The public `RequestChunkReply::Chunk` may
  still own a `Vec<u8>`.
- WebSocket read/write paths use the same scratch/write-owned path where they
  sit inside `HttpConnection`.

Proof:

- HTTP/1 close, keepalive, fixed-body, chunked, body lifecycle, bad-input, and
  WebSocket server tests still pass.
- Perf rows show before/after for:
  - `http1_close_request`;
  - `http1_keepalive_sequential`;
  - `http1_fixed_body_close`.
- Allocation counts decrease or the plan explains the measured reason they did
  not.
- Overload behavior remains typed: service `Full`, body cap full, closed target,
  and timeout still map to the same outcomes.

## Rock 3: Move HTTP/1 Client And Keepalive To Reusable I/O

Use the same owned-buffer shape in native HTTP/1 clients.

Target files:

- `tina-http/src/client.rs`
- `tina-http/src/keepalive.rs`
- HTTP client / keepalive tests

Required:

- Client read loops reuse buffers.
- Keepalive connection read loops reuse buffers across requests.
- Write paths avoid cloning pending request bytes where possible.
- Host/authority/SNI rules stay unchanged.
- Connection retire/reuse truth stays unchanged.

Proof:

- existing HTTP client, keepalive, TLS, bad-input, and pool tests still pass;
- perf rows for keepalive and fixed body improve or name the next bottleneck;
- dead connection retirement still works after buffer reuse.

## Rock 4: Concrete HTTP Encoder Allocation Cleanup

Remove concrete encoder allocations. Do not redesign request headers here.

Required:

- improve response-head capacity calculation so common responses do not
  reallocate;
- replace `length.to_string()` and similar numeric formatting with direct
  `write!(&mut Vec<u8>, ...)` or a tiny stack decimal/hex helper so no
  temporary `String` is allocated;
- replace `format!("{:x}\r\n", n).into_bytes()` in chunked response framing with
  direct write into the output `Vec<u8>`;
- pre-size chunked response frames as `hex_len + 2 + body_len + 2`;
- reuse response-body/head buffers inside a connection where ownership stays
  clear.

Rules:

- Keep `HttpRequest` / `HttpResponse` public source compatibility unless a
  compile-time safety improvement is worth a deliberate migration.
- Do not weaken duplicate-header, content-length, transfer-encoding, Host, or
  parser strictness.
- Do not replace correctness with a special-case benchmark path.
- Do not replace `HeaderMap` in this phase.

Proof:

- allocation deltas before/after;
- parser strictness tests still pass;
- one request with many headers still uses the heap fallback correctly;
- common small request path uses the measured smaller shape.

## Rock 5: Record Handler-Turn Cost

HTTP may still be slower because it takes many Tina turns.

Add or extend hot-path reports to count worker turns and named stages for:

- HTTP/1 close request;
- HTTP/1 keepalive request;
- fixed-body response;
- `call_blocking`;
- service request/reply chain.

Stage names should be concrete:

- read;
- parse;
- service call;
- service returned;
- write;
- close or keepalive loop;
- host unblocked.

This phase does not add a same-turn continuation primitive. The implementation
must record the turn count and write the observed next bottleneck into the
phase evidence. If turn count is dominant, name the next semantic phase there.

Proof:

- report says whether remaining cost is allocation, turn count, socket floor, or
  semantic cost;
- no new helper hides failure/cancel/pressure facts.

## Rock 6: Linux Verification

Phase 145 was mostly macOS aarch64 evidence.

Required:

- run and record the same perf rows on Linux x86_64 when available;
- store platform in perf history;
- do not compare Linux and macOS as one baseline;
- check that the no-spin / idle behavior stays true on Linux.

Proof:

- perf history includes Linux rows or a clear "not run" note;
- Linux does not regress worker-loop no-spin behavior;
- any Linux-specific difference is documented as evidence, not smoothed away.

## Rock 7: Keep The Bounded Story Attached

Speed only counts if Tina remains Tina.

Add at least one overload-shaped perf/proof row that still carries capacity and
terminal truth:

- mailbox/service `Full`;
- body-cap `Full`;
- keepalive or client-pool `Full`;
- request-scope cancellation or shutdown drain.

Reports must include:

- submitted;
- completed;
- full;
- closed;
- timeout;
- cancelled;
- late;
- high-water;
- final-current.

Proof:

- final counts return to zero after shutdown unless the scenario intentionally
  reports a leak;
- faster paths still emit the same typed outcomes;
- no hidden retry or buffering is added to improve numbers.

## Traps

Review these before coding:

- Borrowed buffers crossing an effect boundary are not Tina-shaped. Use owned
  buffers that move out and back.
- `Vec::with_capacity` can still allocate per op if the buffer is not retained
  by the owner. The point is reuse, not a prettier constructor.
- Returning a buffer on error can make APIs noisy, but dropping it can hide
  capacity ownership. Pick one rule and prove it.
- A faster HTTP row that loses `Full` / body-cap / timeout truth is a bad row.
- A faster write path that assumes all writes complete is wrong; partial writes
  are normal.
- TLS may need a separate owned-buffer path because rustls owns encrypted and
  plaintext buffers internally. Do not weaken cert/SNI checks to make it fit.
- Perf rows are local evidence. They are not a production performance claim.

## Does Not Include

- no public production performance claim;
- no broad scheduler rewrite;
- no global allocator trick;
- no removing trace/capacity/replay facts;
- no hidden queues;
- no HTTP/2/gRPC/WebSocket tuning unless the reusable I/O work naturally applies
  through shared runtime calls;
- no native AWS performance work;
- no benchmark that uses SQLite or another bridge as the headline Rust-vs-Tina
  comparison.

## Required Verification

Run focused checks first, then the normal proof gate:

```sh
cargo fmt --all --check
git diff --check
cargo test -p tina-runtime tcp -- --nocapture
cargo test -p tina-http --tests -- --nocapture
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture
make perf-record
make perf-check
make proof-fast
```

If `cargo test -p tina-http --tests` is too broad while iterating, run the
touched HTTP client/server/keepalive/body/WebSocket tests first, but the final
PR must run the broad HTTP test set.

## Proof Matrix

Direct proof:

- owned TCP/TLS read/write buffer tests hit the new public call helpers;
- HTTP server/client/keepalive tests use the new reusable path, not the old
  fresh-`Vec` path;
- perf rows show before/after latency and allocation deltas.

Integration proof:

- native HTTP server + client path still works over TCP;
- HTTPS path works if TLS buffer reuse lands;
- keepalive pool still reuses and retires connections correctly;
- simulator replay covers any new call variants used by the runtime path.

E2E proof:

- `examples/systems/perf_native` exercises host send/call, HTTP close,
  keepalive, fixed body, and overload/capacity rows through public paths;
- at least one overload row proves faster code still exposes pressure and final
  capacity state.

Blast-radius proof:

- existing HTTP bad-input/parser strictness tests pass;
- existing chunked/body lifecycle/WebSocket tests pass;
- existing TLS/HTTPS tests pass if TLS is touched;
- existing DST/proof-fast gate passes;
- stable trace tags are append-only.

If a proof is missing because a sub-rock is deliberately deferred, say so in
the PR summary and in the phase evidence. Do not call the deferred behavior
done.

## Done

- A user can run `make perf` and see credible Tina-native rows.
- HTTP/1 read/write paths avoid the obvious fresh-`Vec` and clone waste.
- Allocation rows improve, or the remaining allocation source is named with
  evidence.
- Tiny host send/call rows remain below millisecond scale.
- Faster paths still prove `Full` / `Closed` / `Timeout` / cancel truth.
- The next bottleneck is explicit enough to seed the next phase.

## After Proof

Only after the implementation and proof pass:

- update `CHANGELOG.md` with what got faster and what stayed bad;
- update `examples/systems/perf_native/README.md` with before/after rows;
- update `.intent/SYSTEM.md` only if the work changes the proved baseline
  truth, not merely because a benchmark improved;
- write `commits.txt` with commit hashes and the proof commands/evidence.
