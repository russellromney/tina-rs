# Phase 155: Protocol Service Hot Path

Status: landed.

Outcome: compact native gRPC dispatch no longer materializes the public
`HttpRequest`/`HeaderMap` shape where the built-in gRPC path does not need it;
unary requests use reusable method-path/template shapes; finite server-streaming
can use a bounded buffered response; compact gRPC responses avoid generic header
map churn; and perf rows plus e2e negative tests prove caps/status/failure truth
still hold. Remaining performance work is deeper protocol/runtime turn-count
and HPACK/header allocation, not this first compact-dispatch pass.

The implementation plan below is historical. Do not treat the "Current Hot
Path" section as current state after this phase landed.

## Goal

Make warmed HTTP/2 gRPC service work cheaper in the real protocol path.

This is not a harness phase. It changes `tina-http` internals. The win must come
from fewer service turns and less header/request allocation, not from weakening
Tina's caps, status truth, or service-call boundary.

## Current Hot Path

Known code shape on `main`:

- `Http2Connection::handle_headers` decodes into `HeaderBlock`.
- `GrpcRouterMsg` opts into `Http2ServiceMessage::compact_http2_headers()`.
- Compact mode skips regular public header storage, but still flows through
  `Http2RequestParts`.
- `GrpcRouter` converts those parts into `GrpcHttp2Request`, then routes through
  `response_for_http2`.
- Streaming gRPC still falls back to `GrpcHttp2Request::into_http_request()`.
- `GrpcRequest<T>` / streaming request types expose `path: String`, so a compact
  transport request can still reallocate the method path before user code sees
  it.
- Unary and buffered server-streaming gRPC responses still build
  `HeaderMap`-shaped `HttpResponse`s and then convert those headers back into
  HPACK header/trailer blocks.

That is better than before Phase 154, but still too much public HTTP shape for
native gRPC paths that do not need public request headers.

## Build

### 1. Add a compact HTTP/2 service request

Add an internal request shape for built-in protocol services:

```text
CompactHttp2Request {
    method,
    method_path,
    body,
    content_type_ok,
    grpc_encoding_unsupported,
    content_length,
}
```

Rules:

- It is internal to `tina-http`; do not expose it as the normal service API.
- It does not carry `HeaderMap`.
- It carries only facts the gRPC router needs.
- The method path must not allocate a new `String` on every warmed gRPC call.
  Use a compact/shared method-path value such as `GrpcMethodPath` or an
  equivalent bounded per-connection cache. If a cache is used, it must have an
  explicit capacity and visible overflow behavior. No hidden unbounded path map.
- It is OK to change public gRPC request structs from `path: String` to the new
  method-path type. There is no stable API yet. Do not keep compatibility
  wrappers that preserve the old allocation on the hot path.
- Generic HTTP/2 services still get `HttpRequest` with real public headers.
- Streaming gRPC gets a compact stream body too; do not rebuild a public
  `HttpRequest` just to preserve the stream source.

Expected code homes:

- `tina-http/src/http2/server.rs`
- `tina-http/src/grpc.rs`
- small helpers in `tina-http/src/http2/headers.rs` if needed

### 2. Split compact and public dispatch

Replace the current `Http2RequestParts` fast path with two explicit paths:

- public HTTP/2 path: decode and store public headers, build `HttpRequest`
- compact protocol path: decode facts, build compact request, call built-in
  service message

The public path must not regress. A user HTTP/2 service that reads a custom
header must still see it.

Add an observable proof hook:

- `Http2ConnectionReport` counters, trace facts, or test-only instrumentation
  must show compact request dispatch and public request dispatch separately.
- Tests must prove the gRPC route used the compact path and the ordinary HTTP/2
  route used the public path.

### 3. Keep gRPC status and caps on the compact path

Compact gRPC must preserve every terminal truth:

- bad/missing `content-type` -> gRPC `InvalidArgument`
- unsupported `grpc-encoding` -> gRPC `Unimplemented`
- request body over cap -> gRPC `ResourceExhausted` or the existing HTTP/2 reset
  path, depending where the cap is enforced today
- service mailbox full -> gRPC `ResourceExhausted`
- service timeout -> gRPC `DeadlineExceeded`
- rejected/closed service -> gRPC `Internal`
- final gRPC status facts still emit

No faster path may bypass `call_cancelable` to the service isolate.

### 4. Reuse/compact header decode storage

Reduce allocation in the common Tina-native HPACK shape:

- avoid creating a `HeaderMap` in compact gRPC dispatch
- keep pseudo-header/path/status parsing allocation-minimal
- reuse per-connection scratch where it is safe and does not leak data across
  streams
- keep fallback HPACK decoder correctness for indexed/Huffman/dynamic-table
  blocks

Do not weaken HTTP/2 validation:

- pseudo-header ordering
- duplicate pseudo-headers
- uppercase names
- connection-control header rejection
- duplicate/invalid `content-length`
- authority/Host rule
- header-list byte cap

### 5. Add a compact gRPC response wire shape

Unary and finite buffered server-streaming gRPC should not allocate a public
`HeaderMap` just to say:

```text
content-type = application/grpc+proto
grpc-status = <code>
grpc-message = optional percent-encoded message
body = owned/shared framed bytes
```

Add an internal gRPC response path that carries exactly those facts and lets the
HTTP/2 encoder write response HEADERS and trailers directly.

Preferred shape for this repo:

- keep `Http2Connection`'s public service reply as `HttpResponse`
- add an internal response-body or response-extension variant that represents
  gRPC wire facts without inserting fake entries into `HttpResponse.headers`
- make HTTP/2 enqueue that variant directly into HPACK HEADERS/trailers
- if HTTP/1 can receive it, serialize it correctly; otherwise make the type
  private enough that only HTTP/2 gRPC can produce it

Rules:

- The service isolate still replies through the ordinary Tina call boundary.
- Generic `HttpResponse` behavior does not change.
- Existing public `HttpResponse` constructors keep working.
- HTTP/1 must either serialize the new shape correctly or the new shape must be
  impossible to reach from HTTP/1.
- gRPC status facts still emit from the same successful and failed paths.

Good implementation shapes:

- a gRPC-specific body/response variant used only by `GrpcRouter`
- or an internal `Http2ServiceReply` conversion that maps gRPC replies to
  compact wire data before enqueueing

Bad implementation shapes:

- stuffing fake headers back into `HeaderMap`
- direct service-handler calls
- losing `grpc-message`
- changing generic HTTP/2 response encoding to optimize only gRPC

### 6. Reduce gRPC unary turns where no policy boundary is crossed

Add a turn-count probe for warmed gRPC unary before changing behavior.

Then remove only non-policy turns. Policy boundaries that must stay visible:

- client call into HTTP/2 connection
- HTTP/2 stream admission / peer cap checks
- service isolate call
- body cap / flow-control decisions
- timeout/cancel path
- response write completion

Good targets:

- same-turn local response framing after service reply when flow control allows
- no extra continuation just to translate compact request parts
- no public request rebuild in streaming fallback
- fewer app/router turns for warmed unary when no body streaming is involved

Bad targets:

- hidden direct handler calls into the service isolate
- bypassing caller reply obligation
- bypassing service mailbox capacity
- turning gRPC failures into generic HTTP success

The implementation must not merge with only "turns are measured." Either reduce
the warmed unary turn count, or show an event timeline proving every remaining
turn is one of the policy boundaries above. If the latter happens, the phase
must still ship the request/header/response allocation wins.

Acceptance bar:

- at least one warmed gRPC row must show a real turn-count reduction, or the PR
  must stay draft and name the runtime primitive needed to reduce the remaining
  policy-boundary turns
- whole-process allocations for warmed gRPC unary must drop materially, not by
  one token allocation; use 20% as the first target unless the before evidence
  proves a different dominant allocator
- if p50/p90 do not improve, the PR must include the measured next bottleneck
  and the code path that owns it

### 7. Perf proof

Update `examples/systems/perf_native` so the rows prove this exact work:

- `grpc_h2c_unary_warmed`
- `grpc_h2c_unary_pooled_concurrent`
- `grpc_h2c_server_streaming_steady_state`
- `http2_h2c_steady_state_small`
- `perf-h2-alloc`
- new warmed gRPC turn/allocation probe, printed as a stable line
- compact/public dispatch counters or equivalent proof line

Evidence required before merge:

- macOS/aarch64 before and after rows
- Linux/x86_64 before and after rows
- process allocation counts, not only host-thread counts
- p50/p90/p99 rows
- turn counts for the warmed gRPC unary path

The PR must say plainly if latency did not move. Allocation-only wins are not
enough unless the plan explains why the next dominant cost is now visible.

## Tests

### Unit

- compact HPACK decode keeps gRPC facts without storing public headers
- fallback HPACK decode still handles indexed/dynamic/Huffman-supported shapes
  exactly as before
- compact/shared gRPC method path does not allocate per warmed call for the
  ordinary static `/package.Service/Method` shape
- compact request construction rejects malformed pseudo-headers and invalid
  content-length before service dispatch
- response/trailer compact helpers preserve `grpc-status` and optional
  `grpc-message`
- compact gRPC response helpers encode the same HEADERS/trailers as the public
  `HttpResponse` path for OK, error status, and message-bearing status

### Integration

- gRPC unary OK over native h2c uses the compact service path and returns the
  decoded message
- warmed gRPC unary uses compact/shared method path; a route lookup must not
  require allocating a new route `String`
- gRPC server-streaming compact path works without rebuilding public
  `HttpRequest`
- generic HTTP/2 service still sees custom request headers
- bad gRPC `content-type` returns `InvalidArgument`
- unsupported `grpc-encoding` returns `Unimplemented`
- oversized gRPC request still returns the existing bounded failure truth
- service mailbox full still maps to gRPC `ResourceExhausted`
- service timeout still maps to gRPC `DeadlineExceeded`
- final gRPC status sent/received facts still appear in the trace
- unary and buffered server-streaming responses use the compact response path
  without changing wire-visible status/trailers
- compact/public dispatch proof is observable through report/fact/test
  instrumentation, not only inferred from allocation counts

### E2E / Perf

- `cargo test -p tina-http grpc -- --nocapture`
- `cargo test -p tina-http http2 -- --nocapture`
- raw HTTP/2 wire tests for malformed gRPC headers/body, so the negative proof
  does not depend on the native client refusing to build bad input
- update gRPC docs/tests/specimens that construct or inspect `GrpcRequest.path`
  so the new method-path type is the copied public shape
- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf native_protocol_rows_are_printable_and_bounded -- --nocapture`
- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture`
- Linux/x86 perf bundle with the same rows
- `cargo fmt --all --check`
- `cargo clippy -p tina-http --all-targets -- -D warnings`
- `cargo clippy --manifest-path examples/systems/perf_native/Cargo.toml --all-targets -- -D warnings`

If Linux cannot run in the session, do not call the phase complete. Leave the PR
draft with macOS evidence and an explicit Linux blocker.

## Non-Goals

- No public web framework.
- No HTTP/2 mTLS.
- No gRPC reflection, interceptors, or load balancing.
- No broad production performance claim.
- No generic async/await interop.
- No bypass of Tina service isolation for speed.

## Done Means

- Warmed gRPC no longer materializes public `HttpRequest` / `HeaderMap` on the
  compact native path.
- Warmed gRPC no longer allocates a new method-path `String` just to route a
  static gRPC method.
- Unary and buffered server-streaming gRPC no longer materialize public
  response `HeaderMap`s just to produce fixed gRPC headers/trailers.
- Generic HTTP/2 still materializes public headers for user services.
- Warmed gRPC turn count is measured and lower, or the PR stays draft with a
  named runtime primitive needed to reduce the remaining policy-boundary turns.
- HPACK/header allocation drops in the pinned rows.
- Whole-process warmed gRPC unary allocation drops materially; one or two fewer
  allocations per request is not enough for this phase.
- macOS and Linux/x86 perf evidence are both in the PR.
- Negative e2e tests prove caps/status/failure truth survived.
