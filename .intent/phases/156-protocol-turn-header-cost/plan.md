# Phase 156: Protocol Turn And Header Cost

Status: planned.

## Goal

Move the real HTTP/2 and gRPC hot path again.

Phase 155 removed the worst public gRPC request/response materialization. The
remaining cost is not one cute clone. It is:

- inbound HPACK/header decode still doing public-header work on compact paths
- gRPC streaming paths still falling back to public `HttpRequest` shapes
- HTTP/2 client responses still cloning `HeaderMap`s
- `GrpcStreamDecoder::push` allocating a fresh output `Vec` per chunk
- dynamic gRPC request/response framing still allocating fresh buffers in the
  ordinary non-preframed path
- warmed gRPC still paying protocol/runtime turns that may not all be policy
  boundaries
- Linux/x86 evidence still not repeated enough to trust the story

This is not a harness phase. Harness changes are allowed only to prove code
changes. The PR must change protocol/runtime hot-path code and show measured
movement.

## Current Code Facts

Checked on current `main` before writing this plan:

- `tina-http/src/http2/headers.rs`
  - `HeaderBlock` still owns `path: Option<String>` and `headers: HeaderMap`.
  - `decode_headers_block_compact_with` skips storing regular headers, but
    `add_header_with_storage` still builds `HeaderName` / `HeaderValue` for
    regular headers before deciding not to store them.
  - `:path` still always allocates a `String`.
- `tina-http/src/grpc.rs`
  - `GrpcHttp2Request` still owns `path: String`.
  - streaming/raw streaming requests still call `into_http_request()`.
  - pending streamed requests store public `HttpRequest`.
- `tina-http/src/http2/client.rs`
  - `ActiveClientStream` stores `response_headers: HeaderMap` and
    `response_trailers: HeaderMap`.
  - `apply_response_headers`, trailer handling, and streamed response heads
    clone header names/values.
- `tina-http/src/grpc_client.rs`
  - `GrpcStreamDecoder::push` returns `Vec<Resp>` every time.
  - `GrpcUnaryTemplate::request` / `preframed` call `encode_grpc_message`, which
    allocates a fresh framed body.
- `tina-http/src/grpc.rs`
  - unary/server-streaming handlers call `encode_grpc_message` for every
    response message.
- `examples/systems/perf_native`
  - warmed gRPC rows exist, but the README still lacks repeated Linux rows for
    warmed gRPC / pooled gRPC / server-streaming.

## Non-Negotiables

- Do not bypass Tina's service isolate call for speed.
- Do not bypass mailbox capacity, request caps, flow-control, timeout, cancel,
  or final gRPC status facts.
- Do not silently drop public HTTP/2 headers for generic HTTP/2 services.
- Do not make a private fast path that returns different wire status/trailers.
- Do not claim success from only adding benchmark rows.
- No broad production-performance claim.

## Build

### 1. Make compact HPACK decode actually compact

Split the header decode path so compact protocol dispatch can parse facts
without constructing public header values.

Required changes:

- In compact mode, do not build `HeaderName` / `HeaderValue` for ordinary
  headers unless the fact needs it.
- Keep fact parsing by string:
  - `content-type`
  - `grpc-encoding`
  - `content-length`
  - `te`
  - forbidden connection-control names
  - `host` / `:authority`
- Keep all validation:
  - pseudo-header order
  - duplicate pseudo-headers
  - uppercase names
  - invalid / duplicate `content-length`
  - invalid `te`
  - forbidden connection-control headers
  - header byte cap
- Keep fallback HPACK decode correctness for indexed, dynamic, and Huffman
  paths.

Good shape:

- `HeaderDecodeMode::Public` / `HeaderDecodeMode::CompactGrpc`, or equivalent.
- A compact `HeaderFacts` / `CompactHeaderBlock` if that makes the code clearer.

Bad shape:

- "compact" mode that still builds public `HeaderMap` pieces.
- skipping validation because the header is not stored.

### 2. Stop rebuilding public requests for native gRPC streaming

Remove the remaining compact-path fallback through `GrpcHttp2Request::into_http_request()`.

Required changes:

- Give streaming and raw streaming handlers an HTTP/2 compact entry point.
- Store pending streamed gRPC requests as compact gRPC request state, not public
  `HttpRequest`.
- Keep body-pull behavior bounded and cancelable.
- Keep request-context reply obligation intact.
- Keep public `HttpRequest` dispatch for generic HTTP/2 services.

Delete compatibility wrappers if they only preserve old allocation shapes. There
are no stable users to protect.

### 3. Shrink gRPC method-path churn

`GrpcHttp2Request` and public gRPC request structs should not allocate a fresh
method-path `String` on every warmed call.

Required changes:

- Replace hot-path `String` ownership with a shared/compact method-path type
  such as `GrpcMethodPath`.
- Route lookup may use `&str`, but the request delivered to gRPC handlers must
  not force a new owned `String` per call.
- If a cache/intern table is used, it must be explicitly bounded and report
  overflow. No hidden unbounded path map.

Acceptance proof:

- A warmed unary route and a warmed streaming route both use the compact path.
- Add a focused allocation probe around a warmed route dispatch. It must fail if
  the handler request rebuilds the method path into a fresh owned `String` per
  request. Do not prove this only by code inspection.

### 4. Reduce client response header allocation for gRPC

The generic HTTP/2 client needs public response headers. gRPC mostly needs:

- HTTP status
- `grpc-status`
- optional `grpc-message`
- content-type / unsupported encoding facts when the path needs them

Required changes:

- Add a compact gRPC response-head/trailer fact path for `GrpcClient` use.
- Avoid cloning a full `HeaderMap` for unary gRPC when the caller only asks for
  `GrpcUnaryOutcome`.
- The compact receive path should hang off gRPC-shaped client calls such as
  `SubmitGrpcUnary`, not by weakening generic `Http2ClientOutcome`.
- Keep generic `Http2ClientOutcome::Replied(Http2ClientResponse { headers, ... })`
  working unchanged for ordinary HTTP/2 users.
- Keep streamed HTTP/2 response heads public when the user opens a generic
  stream.

Bad shape:

- deleting public headers from generic client outcomes.
- parsing `grpc-status` only after constructing a full map if the compact path
  had enough facts already.

### 5. Reuse gRPC stream decoder output storage

`GrpcStreamDecoder::push` currently returns a fresh `Vec<Resp>` per chunk.

Required changes:

- Add a reusable-output API such as `push_into(&mut Vec<Resp>, bytes)` or a
  callback-style fold API.
- Keep the existing simple API only if it is a thin convenience wrapper over
  the reusable path.
- Update perf/specimen code to use the reusable path where repeated chunks are
  expected.

Proof:

- partial frame across chunks
- several messages in one chunk
- compressed message rejected
- over-cap message rejected before allocation
- stream finish with truncated frame rejected

### 6. Reuse dynamic gRPC encode/decode buffers

The preframed path is good for fixed messages. Real services also send dynamic
protobuf messages.

Required changes:

- Add a reusable framing API for dynamic unary/client-streaming requests, such
  as `GrpcUnaryTemplate::request_into(&mut Vec<u8>, message)` or equivalent.
- Add reusable server-side response framing where safe:
  - no shared mutable buffer may outlive the handler turn
  - no response may borrow a scratch buffer after it is enqueued
  - if a response buffer leaves the isolate, the scratch slot must be replaced
    with an empty/reused buffer explicitly
  - any reusable/pool storage must have an explicit service-owned cap and a
    visible `Full` / `ResourceExhausted` path; no hidden growing response pool
- Keep simple public helpers as wrappers over the reusable path, not separate
  hot paths.
- Keep message-size caps enforced before committing unbounded allocation.
- Keep protobuf decode from slices where possible; do not copy framed body bytes
  just to decode.

Proof:

- dynamic unary request path uses reusable framing storage
- dynamic unary server response path uses reusable framing storage or a clearly
  bounded owned-buffer pool
- over-cap request and response messages fail with existing gRPC status truth
- no test may pass by switching only to the preframed fixed-payload helper

### 7. Reduce real protocol turns

Add a warmed gRPC unary timeline/turn probe first. Then remove non-policy turns.

Policy boundaries that must stay visible:

- host/client call into HTTP/2 connection
- stream admission and peer cap checks
- service isolate call
- body cap / flow-control decisions
- timeout / cancel
- response write completion

Good targets:

- compact request translation that currently lands as an extra protocol turn
- public request rebuild continuations
- same-turn protocol-local response framing when flow-control allows it
- streamed gRPC body handling that adds a turn only to convert shapes

Bad targets:

- direct-calling user handlers from the connection isolate
- bypassing service mailbox capacity
- converting timeout/full/closed/rejected into one generic success/error

Turn-count proof rules:

- Count turns from stable runtime trace events or an existing hotpath probe that
  records actual handler/protocol dispatches.
- Save the before and after timeline text in this phase folder.
- Do not change the definition of "turn" between before and after.
- Do not count only the host thread. The worker/protocol/service side is the
  cost being attacked.

Done means:

- at least one warmed protocol row has fewer protocol/app turns than before,
  with old and new event timelines saved.
- warmed gRPC unary is the first target.
- if warmed gRPC unary cannot drop because every remaining turn is a policy
  boundary, the PR must reduce turns in warmed gRPC streaming or HTTP/2 small
  steady-state instead, and must name the exact future runtime primitive needed
  for unary.
- WebSocket turn wins do not count for this phase. The turn-count win must come
  from HTTP/2 or gRPC.

### 8. Hard performance proof

Perf proof must be before/after from the same machine class.

Required rows:

- `grpc_h2c_unary_warmed`
- `grpc_h2c_unary_pooled_concurrent`
- `grpc_h2c_server_streaming_steady_state`
- `http2_h2c_steady_state_small`
- `http2_h2c_client_steady_state_post`
- `perf-h2-alloc`
- a warmed gRPC turn-count row
- a compact-vs-public dispatch row or counter

Required evidence:

- macOS/aarch64: 3 before runs and 3 after runs
- Linux/x86_64: 3 before runs and 3 after runs
- p50, p90, p99
- process allocations and allocated bytes
- RSS delta when available

Targets:

- warmed gRPC unary process allocations: at least 20% lower than the saved
  same-platform before median
- warmed gRPC server-streaming process allocations: at least 15% lower than the
  saved same-platform before median
- HTTP/2 steady-state process allocations: lower than before; no regression
- p90 and p99: no worse than 10% unless the PR names the measured new bottleneck
  and shows allocation/turn wins are real
- no row may pass by deleting work, weakening caps, or using a different
  workload
- at least one protocol/app turn-count row must improve; otherwise the PR is not
  done
- save raw output and parsed summaries under
  `.intent/phases/156-protocol-turn-header-cost/`

If Linux cannot run, leave the PR draft. Do not call Phase 156 complete.

## Tests

### Unit

- Compact HPACK decode parses gRPC facts without storing public headers.
- Compact HPACK decode does not construct public `HeaderName` / `HeaderValue`
  for skipped regular headers; use an allocation/count probe or a test seam.
- Public HPACK decode still stores public headers.
- Compact and public decode reject the same malformed pseudo-header,
  uppercase-name, invalid-`te`, forbidden-header, duplicate-content-length, and
  over-cap inputs.
- Dynamic/indexed/Huffman-supported fallback HPACK blocks still decode.
- `GrpcMethodPath` / equivalent validates absolute method paths and can be
  shared through unary and streaming handler requests.
- Compact gRPC response/trailer fact parsing preserves OK, non-OK, and
  message-bearing status.
- Reusable `GrpcStreamDecoder` output handles partial, multi-message,
  compressed, too-large, and truncated inputs.

### Integration / E2E

- Native gRPC unary OK over warmed h2c uses compact request and compact response
  paths.
- Native gRPC server-streaming uses compact request state and reusable decoder
  output.
- Native gRPC bidirectional or raw streaming no longer rebuilds public
  `HttpRequest`.
- Generic HTTP/2 service still receives custom public headers.
- Bad gRPC content-type returns `InvalidArgument`.
- Unsupported `grpc-encoding` returns `Unimplemented`.
- Oversized gRPC request returns the existing bounded failure truth.
- Service mailbox full maps to gRPC `ResourceExhausted`.
- Service timeout maps to gRPC `DeadlineExceeded`.
- Service rejected/closed maps to gRPC `Internal`.
- Final gRPC status facts still appear in trace/replay facts.
- HTTP/2 client generic response still returns public headers.
- gRPC client compact receive path still reports status/trailer errors.
- Raw wire tests cover malformed request headers; do not rely only on native
  clients, because native clients refuse to build some bad input.
- Public docs/examples that mention `GrpcRequest.path` compile and show the new
  copied shape if the type changes.

### Commands

Run focused checks:

```sh
cargo test -p tina-http grpc -- --nocapture
cargo test -p tina-http http2 -- --nocapture
cargo test -p tina-http --test grpc_client_live -- --nocapture
cargo test -p tina-http --test grpc_live -- --nocapture
cargo test -p tina-http --test http2_live -- --nocapture
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf native_protocol_rows_are_printable_and_bounded -- --nocapture
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture
cargo fmt --all --check
cargo clippy -p tina-http --all-targets -- -D warnings
cargo clippy --manifest-path examples/systems/perf_native/Cargo.toml --all-targets -- -D warnings
```

Run full proof before merge:

```sh
make proof-fast
```

Run Linux/x86 perf evidence through `examples/systems/perf_native/fly/` or the
manual Linux perf workflow. Save the raw command output and parsed rows in this
phase folder.

## Files Likely To Change

- `tina-http/src/http2/headers.rs`
- `tina-http/src/http2/server.rs`
- `tina-http/src/http2/client.rs`
- `tina-http/src/grpc.rs`
- `tina-http/src/grpc_client.rs`
- `tina-http/src/types.rs` only if a compact response shape needs it
- `tina-http/tests/*grpc*`
- `tina-http/tests/*http2*`
- `examples/systems/perf_native/*`
- docs/examples that mention `GrpcRequest.path`

## Non-Goals

- No public web framework.
- No gRPC reflection, interceptor stack, or load balancing.
- No HTTP/2 mTLS.
- No async ecosystem bridge change.
- No direct handler calls that break Tina isolation.
- No production-performance claim.

## Done Means

- Protocol code changed in the hot paths named above.
- Compact HPACK decode is cheaper without weaker validation.
- Native gRPC streaming no longer falls back through public `HttpRequest`.
- gRPC method paths do not allocate a fresh `String` per warmed request.
- gRPC client receive can avoid full public `HeaderMap` churn for unary status.
- `GrpcStreamDecoder` has a reusable-output path used by repeated streaming
  code.
- Dynamic gRPC request/response framing has a reusable path used by at least
  one non-preframed perf row.
- At least one warmed protocol/app turn-count row is lower; warmed gRPC unary is
  preferred, but streaming or HTTP/2 steady-state is acceptable if unary is all
  policy boundaries and the PR proves that timeline.
- macOS and Linux repeated before/after rows are saved.
- p50/p90/p99 and process allocations are pinned.
- Negative e2e tests prove caps/status/failure truth survived.
- The PR cannot pass with benchmark/harness-only changes.
