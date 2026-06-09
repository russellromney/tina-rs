# Phase 156 Plan Review

Reviewer: Codex

## Findings

### [P2] Original scope could still pass as a harness phase

The user explicitly asked for actual performance work, not another benchmark
slice. The plan now says the PR must change named protocol/runtime hot-path
files and cannot pass with harness-only changes.

### [P2] Turn-count reduction can accidentally bypass Tina policy

The dangerous "fast" fix is direct-calling the gRPC handler from the HTTP/2
connection. That would be fast and wrong: mailbox capacity, timeout, request
context, and trace truth would be hidden. The plan now names policy boundaries
that must stay visible and forbids direct service-handler calls.

### [P2] "Prove the blocker" was too easy to use as an escape hatch

The first draft let the PR finish without any turn-count reduction if it proved
unary was already all policy boundaries. That is too soft for this phase. The
plan now requires at least one warmed protocol/app turn-count row to improve;
unary is preferred, streaming or HTTP/2 steady-state is acceptable if unary is
truly blocked by Tina policy boundaries.

### [P2] Compact HPACK can weaken validation

Skipping public header storage is good. Skipping validation is not. The plan now
lists every validation rule that compact and public paths must share, including
duplicate content length and forbidden connection-control names.

### [P2] Linux evidence must be required, not optional

Several previous perf phases found different behavior on Linux. The plan now
requires repeated Linux/x86 before/after rows and says the PR remains draft if
Linux cannot run.

### [P3] Path sharing could become an unbounded intern table

Avoiding per-request `String` allocation is good, but a hidden method-path cache
would violate Tina's boundedness story. The plan now allows a cache only if it is
explicitly bounded and reports overflow.

### [P2] Dynamic protobuf cost was named but underplanned

The first draft only covered stream decoder output reuse. Current code also
allocates fresh framed buffers in `GrpcUnaryTemplate` and server-side
`encode_grpc_message` paths. The plan now requires reusable dynamic framing
without cheating by using only preframed fixed-payload rows.

## Result

Plan updated. It is implementation-ready and grug enough: exact files, exact hot
spots, hard proof, and no planning/audit work left inside the phase.
