# Hostile Review: 096 Finish Native gRPC Streaming

## Verdict

Right direction, but the phase has two traps:

- claiming a production client without an HTTP/2 client state machine;
- claiming interop from a hand-rolled test client.

## Findings

### Production Client Is Conditional

The plan correctly says a production gRPC client can only land if a real HTTP/2
client state machine exists. Keep that line hard. The Phase 057 h2c helper must
not grow into a hidden production client by accident.

### Interop Must Use Real Tools

Interop means tonic/grpcurl commands or tests, not "our own frames decode." If
the phase only tests Tina client/helper against Tina server, it may claim native
streaming, not interop.

### Bidi Needs Lifecycle Rules Before Code

Bidirectional streaming needs policy for:

- response before request EOF;
- request EOF while response continues;
- service error while request DATA is still arriving;
- peer reset while source calls are in flight;
- final status exactly once.

These should be pinned in Rock 0 before implementation.

### Reflection Should Not Hijack Streaming

grpcurl often wants reflection for convenience, but explicit proto/descriptor
interop is enough to prove transport/service behavior. Reflection is a separate
feature unless deliberately scoped.

### Compression Must Stay Loud

If compression remains unsupported, every streaming mode must reject compressed
messages consistently. Do not let one path silently accept `grpc-encoding`.

## Recommendation

Implement after 095 in this order:

1. Server-streaming.
2. Client-streaming.
3. Bidirectional streaming.
4. h2c tonic/grpcurl interop for shipped server modes.
5. Production client only if the native HTTP/2 client state machine is already
   real; otherwise create the client phase next.
