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

### Fixed In Execution Slice: Typed Finite Server-Streaming Helper

`GrpcServerStreamingResponse::from_messages` now lets a handler return finite
typed `prost::Message` streams without manually building gRPC frames. This is
not the final infinite/backpressured typed stream API, but it removes the worst
specimen ergonomics lie from the first server-streaming shape.

### Fixed In Execution Slice: Many Small Client Messages

The live suite now sends 1000 tiny client-streamed messages split across
awkward DATA chunk boundaries. This caught the HTTP/2 window-credit flood and
now proves the route survives user-shaped small-message streams.

### Fixed In Execution Slice: Non-Reading Streaming Cancel

The live suite now starts a server-streaming response whose first source chunk
is larger than the HTTP/2 send window, never drains the response DATA, sends
`RST_STREAM`, and proves the source receives cancel. This catches the specific
lie where cancellation only works after the happy reader has already drained a
chunk.

### Fixed In Execution Slice: Declared Message Cap Before Service

Client-streaming now sends a malicious gRPC frame header declaring a message
larger than `max_message_bytes` without providing the protobuf payload. The
route returns `ResourceExhausted` and the user handler is not invoked, proving
the cap fires before protobuf decode or service code.

### Fixed In Execution Slice: Tonic h2c Interop

The specimen now has a tonic client test that connects over h2c and exercises
unary, server-streaming, and client-streaming routes. This exposed a real
interop gap: the server's request HPACK decoder only understood the private
literal headers used by Tina tests. Incoming HTTP/2 headers now use a stateful
HPACK decoder so dynamic/indexed/huffman-encoded client headers are not a
hidden compatibility cliff.

### Fixed In Hostile Review: Tonic Large Unary And Request-Sensitive Streaming

The tonic interop test now asks for a 70KB protobuf response and verifies
server-streaming output depends on the decoded request message. This caught the
HTTP/2 buffered-response all-or-nothing flow-control bug and removes the
previous specimen lie where `Watch` returned fixed values regardless of input.

### Fixed In Hostile Review: Tight-Queue Final Trailer Proof

The gRPC live suite now runs server-streaming with a tiny outbound frame queue
and still requires final `grpc-status` trailers. This protects the exact edge
where HTTP/2 EOF handling could drop trailers and close the stream state.

### Fixed In Hostile Review: grpcurl Command Ownership

The specimen now owns `proto/specimen_counter.proto` plus documented grpcurl
commands for unary, server-streaming, and client-streaming. The current local
verification environment does not have `grpcurl`, so CI automation is still
listed as deferred rather than falsely claimed.

### Still Risky: Client-Streaming Handler API Is Buffered

The HTTP/2 request pull path is real, caps fire before service code, and the
many-small-message path is proven. But `GrpcRouter::client_streaming` still
collects decoded messages into a `Vec<T>` before invoking the user handler.
That is not the final service-level streaming API for unbounded streams, early
application reject, or request/response overlap.

### Fixed In Plan: Client-Streaming Is Now A Bidi Prerequisite

The plan now makes `GrpcRequestStream<T>` or an equivalent Tina-shaped pull
handle a required Rock 3 deliverable before bidirectional streaming. This is
important because bidi must reuse the same inbound message stream instead of
inventing a parallel decoder or hiding another buffered `Vec<T>` path.

### Hostile Review: Client-Streaming Plan Still Needs Ownership Precision

The updated plan is better, but implementation must freeze exact ownership
before coding:

- who owns the partial five-byte gRPC frame header;
- who owns partially accumulated protobuf bytes for the current message;
- when HTTP/2 window credit is returned for bytes that have been framed but not
  decoded;
- whether early success drains or resets unread request DATA;
- how pending `next` calls wake on peer reset, service return, and local
  deadline.

If any of these are left implicit, the first "streaming" API will either leak
memory, over-credit flow control, or leave a caller parked forever.

### Fixed In Plan: Buffered Helper Must Be Named As Buffered

The plan now says the existing `Vec<T>` behavior may survive only as an
explicit buffered helper such as `client_streaming_buffered`. Keeping it under
the default `client_streaming` name would teach users the wrong mental model
and make later real streaming a breaking semantic surprise.

### Hostile Review: Memory Proof Must Measure The Right Thing

The plan asks for "many messages without resident memory growing with message
count." That must not be a hand-wavy assertion. The test should use either a
route-side high-water counter, runtime/body metrics, or a deliberately tiny
resident cap that would fail if the router buffered all messages. A test that
sends 10,000 tiny messages and merely completes is not proof.

### Hostile Review: Early Success Is As Dangerous As Early Error

Most people remember to test early error. Early success is worse: it can return
OK while the peer continues sending request DATA. The plan now requires a
pinned success-before-EOF policy. Implementation must prove the connection does
not accept unbounded unread DATA, silently reuse the stream id, or lose final
status.

### Still Risky: Server-Streaming API Is Too Raw

`GrpcServerStreamingResponse` currently asks handlers to provide a
`ResponseChunkMsg` source of already gRPC-framed bytes. That is real transport
plumbing, but it is not yet the final pleasant Tina-shaped typed message source.
The next pass should wrap typed `prost::Message` streams so user handlers do
not hand-build gRPC frames.

### Bullshitproof E2E Tests Still Needed

Before claiming 096 complete, add tests that:

1. Run grpcurl against the specimen with an explicit proto/descriptor in CI.
2. Run a tonic client against bidi routes.
3. Add the service-level client-streaming API and prove early application
   reject and early success before request EOF.
4. Prove client-streaming resident memory is bounded by current message/chunk,
   not total message count.
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
2. True service-level client-streaming via `GrpcRequestStream<T>`.
3. Rename or clearly preserve the buffered client-streaming helper as buffered.
4. Bidirectional streaming reusing `GrpcRequestStream<T>`.
5. User-facing specimen command proof.
6. h2c tonic/grpcurl interop for shipped server modes.
7. Production client only if the native HTTP/2 client state machine is already
   real; otherwise create the client phase next.
