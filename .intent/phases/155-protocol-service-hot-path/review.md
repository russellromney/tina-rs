# Phase 155 Review

## Plan Review 1

Finding 1 [P2]: The plan could still let the implementer only rename
`Http2RequestParts` to a compact type and claim victory.

Resolution: `Done Means` now requires the warmed gRPC path to stop materializing
public `HttpRequest` / `HeaderMap`, and perf proof must show allocation drop on
the pinned rows. The compact type must be internal and headerless.

Finding 2 [P2]: Turn-count reduction can become unsafe if it bypasses the Tina
service call.

Resolution: The plan explicitly forbids direct service handler calls and names
the policy boundaries that must remain visible: service mailbox admission,
`call_cancelable`, caps, timeout/cancel, and write completion.

Finding 3 [P2]: Compact gRPC could accidentally lose error/status truth.

Resolution: The test matrix now names bad content-type, unsupported
`grpc-encoding`, oversized body, service full, timeout, rejected/closed service,
and trace status facts as required e2e proof.

Finding 4 [P3]: Header allocation work could weaken generic HTTP/2 user
semantics.

Resolution: The plan requires a generic HTTP/2 service test that reads a custom
header. Compact dispatch is only for built-in protocol services.

Finding 5 [P2]: Perf proof could be macOS-only again.

Resolution: Linux/x86 before/after rows are required. If they cannot run, the PR
must stay draft.

Finding 6 [P3]: The plan says "reuse scratch" but could invite cross-stream data
leaks.

Resolution: The build section now says reuse only where safe and no stream data
may leak. The implementation review must check scratch lifetime explicitly.

Finding 7 [P2]: The first draft optimized inbound request/header materialization
but left gRPC responses on the public `HttpResponse.headers` path. That would
leave a major HPACK/header allocation source untouched.

Resolution: Added a compact gRPC response wire shape requirement for unary and
finite buffered server-streaming. It must preserve wire headers/trailers and
trace status facts without rebuilding fake public `HeaderMap`s.

## Plan Review 2

Finding 8 [P2]: The plan still allowed per-request method-path allocation.
`GrpcRequest<T>` and streaming request types currently expose `path: String`, so
a compact HTTP/2 request could avoid `HeaderMap` and still allocate the route
path before every handler call.

Resolution: The plan now requires a compact/shared method-path value or a
bounded visible path cache. Route lookup on warmed gRPC calls must not allocate a
new route `String`.

Finding 9 [P2]: The plan could still merge marginal wins: one allocation gone,
turn count explained away, performance still bad.

Resolution: Added acceptance bars. A warmed gRPC row must reduce turn count or
the PR stays draft with the missing runtime primitive named. Whole-process
warmed unary allocation must drop materially, with 20% as the first target
unless evidence proves another allocator dominates.

Finding 10 [P2]: Compact path proof was too implicit. A test could pass because
the public path still worked, not because compact dispatch was used.

Resolution: The plan now requires an observable compact/public dispatch proof
hook: report counters, trace facts, or test-only instrumentation. Tests must
assert gRPC uses compact dispatch and generic HTTP/2 uses public dispatch.

Finding 11 [P3]: Negative tests could accidentally use the native client, which
may refuse to build malformed requests before the server path is exercised.

Resolution: Added raw HTTP/2 wire negative tests for malformed gRPC headers/body.
The bad-input proof must hit the server connection and compact router path.

Finding 12 [P3]: Changing `GrpcRequest.path` from `String` is the likely clean
fix, but the plan did not say whether public API churn is allowed. That could
lead to a compatibility wrapper that keeps the allocation alive.

Resolution: The plan now allows the public gRPC request structs to move to the
new method-path type and forbids hot-path compatibility wrappers that preserve
the old `String` allocation.
