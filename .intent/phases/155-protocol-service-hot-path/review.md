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
