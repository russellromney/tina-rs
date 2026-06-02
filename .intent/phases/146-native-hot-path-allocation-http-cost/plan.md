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

## Rock 0: Make Perf Rows Harder To Lie With

Tighten the existing perf harness before claiming wins.

Required:

- `make perf` / `make perf-compare` print:
  - p50 / p90 / max;
  - process allocations;
  - process allocated bytes;
  - worker-thread allocations where the probe can honestly see them;
  - RSS delta;
  - semantic match label;
  - git sha, platform, and profile.
- `make perf-record` writes rows with all fields needed for later comparison.
- `make perf-check` compares against the checked-in baseline.
- Perf history stores platform-specific rows separately. Do not merge macOS and
  Linux into one number.
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
```

Rules:

- `max_len` is still a service-owned cap.
- The returned `len` names the valid prefix of `buffer`.
- EOF remains `len == 0`.
- Errors return enough ownership truth to avoid leaking capacity or stranded
  bytes. If returning the buffer on every error makes the public shape noisy,
  document and test where the bytes are dropped.
- Existing `tcp_read` / `tcp_write` stay as compatibility helpers unless
  migration is tiny and source-compatible.
- Add stable trace/call tags append-only if new call variants are needed.
- Simulator gets byte-identical semantics for the new calls.

TLS:

- Add the same owned-buffer shape for TLS if it can reuse the TCP-rail TLS
  machinery without a second semantic design.
- If TLS cannot fit safely in this PR, document exact deferral and keep HTTPS
  perf rows out of any win claim.

Proof:

- TCP live read/write success, EOF, partial write, close, cancel, and shutdown
  tests.
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

- `HttpConnection` reuses a read buffer instead of receiving a fresh read
  `Vec<u8>` and copying it into `self.read_buf`.
- `write_pending()` stops cloning the full pending response before every write.
- Partial writes still drain exactly the accepted prefix.
- Body-pressure accounting stays exact.
- Known-length, chunked, WebSocket, request-body streaming, and keepalive paths
  keep their current wire behavior.
- Request body pull avoids `drain(..take).collect()` where a split/reuse path
  can remove a copy without making the API worse. If this part is not safe,
  leave it for a named follow-up and prove the rest.

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

## Rock 4: Smaller HTTP Header And Response Allocation

After Rocks 1-3, use the perf evidence to remove the next obvious HTTP
allocation.

Allowed fixes:

- improve response-head capacity calculation so common responses do not
  reallocate;
- avoid `to_string()` allocation for common content lengths/status fields where
  a stack buffer is easy;
- reduce small-header allocation if measured rows show `HeaderMap` construction
  dominates common requests;
- reuse response-body/head buffers inside a connection where ownership stays
  clear.

Rules:

- Keep `HttpRequest` / `HttpResponse` public source compatibility unless a
  compile-time safety improvement is worth a deliberate migration.
- Do not weaken duplicate-header, content-length, transfer-encoding, Host, or
  parser strictness.
- Do not replace correctness with a special-case benchmark path.

Proof:

- allocation deltas before/after;
- parser strictness tests still pass;
- one request with many headers still uses the heap fallback correctly;
- common small request path uses the measured smaller shape.

## Rock 5: Measure Handler-Turn Cost, Then Only Fix If Evidence Demands

HTTP may still be slower because it takes many Tina turns.

Measure first.

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

If a narrow same-turn continuation primitive removes a repeated Tina-owned turn
without hiding a suspension point, implement it.

If the fix would become fake async, hidden callbacks, or state mutation outside
the handler, do not build it in this phase.

Proof:

- report says whether remaining cost is allocation, turn count, socket floor, or
  semantic cost;
- no new helper hides failure/cancel/pressure facts;
- if no turn-count fix lands, the README names why.

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

## Done

- A user can run `make perf` and see credible Tina-native rows.
- HTTP/1 read/write paths avoid the obvious fresh-`Vec` and clone waste.
- Allocation rows improve, or the remaining allocation source is named with
  evidence.
- Tiny host send/call rows remain below millisecond scale.
- Faster paths still prove `Full` / `Closed` / `Timeout` / cancel truth.
- The next bottleneck is explicit enough to seed the next phase.
