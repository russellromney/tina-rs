# Hostile Review: 096 Finish Native gRPC Streaming

## Verdict

Right direction, but the phase has two traps:

- claiming a production client without an HTTP/2 client state machine;
- claiming interop from a hand-rolled test client.

## Findings

### Fixed In Code Review: Server Streaming Needed Repeat-Call Proof

The first server-streaming test only called one route once. That can hide
single-use source bugs. The live suite now calls the same streaming route twice
on one connection and proves both calls receive full messages plus final
status.

### Fixed In Code Review: Streaming Modes Needed Same-Connection Proof

Testing server-streaming and client-streaming separately does not prove HTTP/2
multiplexing. The live suite now runs server-streaming and client-streaming on
separate streams of the same connection and asserts no cross-talk.

### Fixed In Code Review: Client-Streaming Needed Hostile Framing Proof

Client-streaming now has tests for request trailers, content-length overrun,
content-length underrun, and total body cap across consumed chunks. These force
protocol bugs to reset visibly instead of returning friendly but false gRPC
statuses.

### Still Risky: Server-Streaming API Is Too Raw

`GrpcServerStreamingResponse` currently asks handlers to provide a
`ResponseChunkMsg` source of already gRPC-framed bytes. That is real transport
plumbing, but it is not yet the final pleasant Tina-shaped typed message source.
The next pass should wrap typed `prost::Message` streams so user handlers do
not hand-build gRPC frames.

### Bullshitproof E2E Tests Still Needed

Before claiming 096 complete, add tests that:

1. Run grpcurl against the specimen with an explicit proto/descriptor.
2. Run a tonic client against unary, server-streaming, client-streaming, and
   bidi routes.
3. Force server-streaming pressure with a non-reading client, then send
   `RST_STREAM` and prove source cancel.
4. Force client-streaming with thousands of small messages and one oversized
   declared message that must fail before protobuf allocation.
5. Interleave bidi request/response messages while one direction is
   flow-control blocked.
6. Start/stop the specimen repeatedly on random ports and run copy-paste
   documented commands, not private helper clients.

### Production Client Is Conditional

The plan correctly says a production gRPC client can only land if a real HTTP/2
client state machine exists. Keep that line hard. The Phase 057 h2c helper must
not grow into a hidden production client by accident.

### Fixed In Plan: Server Claim Was Easy To Misread

The goal now says server-side gRPC streaming. That matters. Without an HTTP/2
client state machine, this phase can finish the server layer and interop
against real clients, but it cannot honestly claim a production Tina gRPC
client.

### Interop Must Use Real Tools

Interop means tonic/grpcurl commands or tests, not "our own frames decode." If
the phase only tests Tina client/helper against Tina server, it may claim native
streaming, not interop.

### Fixed In Plan: Interop Commands Must Be Owned

The first review said "use real tools" but did not force the repo to own the
commands. The plan now requires checked-in tests, scripts, or specimen commands,
and requires documented commands to own descriptors, ports, fixture setup, and
environment. A command pasted from chat history is not a gate.

### Bidi Needs Lifecycle Rules Before Code

Bidirectional streaming needs policy for:

- response before request EOF;
- request EOF while response continues;
- service error while request DATA is still arriving;
- peer reset while source calls are in flight;
- final status exactly once.

These should be pinned in Rock 0 before implementation.

### Fixed In Plan: Bidi Lifecycle Is A Gate

Rock 0 now requires the exact bidirectional lifecycle policy before coding:
request EOF, response EOF, early service error, peer reset, local cancel, and
final status ownership. Tests now include service error while request DATA is
still arriving.

### Reflection Should Not Hijack Streaming

grpcurl often wants reflection for convenience, but explicit proto/descriptor
interop is enough to prove transport/service behavior. Reflection is a separate
feature unless deliberately scoped.

### Compression Must Stay Loud

If compression remains unsupported, every streaming mode must reject compressed
messages consistently. Do not let one path silently accept `grpc-encoding`.

### Still Risky: User Perspective Can Be Under-Proven

A user does not care that an internal helper passed. They care that a specimen
server starts, a documented command talks to it, caps fail with the named
status, cancel/deadline behavior is observable, and the command exits cleanly.
The plan now requires command proofs, but implementation must keep them in CI
or a scripted smoke target.

### Still Risky: Pressure Proof Can Drift Back Into HTTP/2

096 should not retest every HTTP/2 window edge, but it must prove gRPC exposes
the pressure truth produced by 095. If gRPC wraps every flow-control outcome as
`Internal`, users lose the reason this native stack exists.

## Recommendation

Implement after 095 in this order:

1. Server-streaming.
2. Client-streaming.
3. Bidirectional streaming.
4. User-facing specimen command proof.
5. h2c tonic/grpcurl interop for shipped server modes.
6. Production client only if the native HTTP/2 client state machine is already
   real; otherwise create the client phase next.
