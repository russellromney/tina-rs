# Phase 154 Hostile Review

## Findings Checked

1. Compact submit could bypass HTTP/2 admission truth.

   Fixed by copying the same checks as generic `Submit`: closed, stream-id
   exhausted, local stream cap, peer stream cap, outbound queue cap, and header
   size cap. Pre-connect compact requests fall back to a normal queued request,
   so they do not create a second queue.

2. Preframed unary could hide dynamic-message semantics.

   Kept explicit. `unary_request` and `GrpcUnaryTemplate::request` still encode
   each message. Only `GrpcPreframedUnary` reuses bytes, and its name says the
   payload is already framed.

3. Buffered server-streaming could replace real streaming with a lie.

   Kept as a separate method: `server_streaming_buffered`. It is for small
   finite streams. Source-backed `server_streaming` remains the honest path for
   flow-controlled, late-produced, or unbounded-length work.

4. Shared body could break body caps.

   HTTP/2 validates `Shared` with the same `max_response_body_bytes` check as
   `Buffered`. It is still a known-length buffered body.

5. Shared body could break HTTP/1.

   HTTP/1 now accepts it. Small bodies coalesce by slice; large shared bodies
   become owned at the existing pending-body staging point.

6. Perf might only prove wrapper changes.

   The PR changes protocol code and rows:

   - compact gRPC HTTP/2 client admission;
   - shared outbound DATA bodies;
   - shared buffered HTTP response bodies;
   - buffered finite server-streaming route.

   It still does not solve the whole gRPC process-allocation story.

## Remaining Sharp Edges

- `HttpResponseBody::Shared` is a public enum variant. That is useful, but it
  widened exhaustive matches. The tests caught stale matches; user code will
  also need to handle it. This is acceptable before a stable API.
- Process allocations remain too high. This phase improves load-worker
  allocations and some p50/p90 rows, but does not justify a production
  performance claim.
- Linux numbers are still not in this artifact.

## Second Hostile Pass

- The shared pre-connect cap proof did not exercise `SubmitGrpcUnary`. Fixed by
  changing the raw-peer e2e cap test to park a compact gRPC unary request under
  the same bounded pre-connect queue and then prove the next request is `Full`.
- The buffered finite streaming helper had positive e2e coverage, but no direct
  negative proof for oversized protobuf messages. Fixed with a unit proof that
  `from_messages` returns `GrpcError::EncodeTooLarge` before a route can map it
  to an app-chosen status.
- The unchecked `GrpcBufferedServerStreamingResponse::from_framed_body` escape
  hatch was public. That made the new helper too easy to misuse by bypassing
  `GrpcLimits`. Fixed by making it private; user code gets the checked
  `from_messages` path.
- `Http2ClientGrpcUnaryRequest::owned/shared` were public, which let external
  callers construct compact gRPC submits without the `GrpcClient` path
  validation and message-size checks. Fixed by making those constructors
  crate-private; the e2e pre-connect cap test now uses `GrpcClient::unary_request`
  like user code.
- The compact gRPC request `path` field stayed public after the constructors
  were made crate-private. A caller could build a valid request through
  `GrpcClient`, mutate the path, then submit an invalid method path. Fixed by
  making the field private and exposing only `path()` inspection.
- Buffered finite server-streaming bounded each message but not the message
  count or total framed response body before allocating the full body. That was
  a request-sized batch footgun. Fixed with `GrpcBufferedStreamLimits`
  (`max_messages`, `max_body_bytes`) and live proof that overflow returns
  `ResourceExhausted` without partial messages.
- Pre-connect compact gRPC submits were admitted through the generic
  `Http2ClientRequest` queue, losing the shared body and fixed-header path
  before `Begin`. Fixed with a bounded `queued_grpc_unary` queue that counts
  against the same pre-connect cap and flushes through `admit_grpc_unary_stream`.

## Verdict

Real improvement, not enough. Mergeable if focused tests, clippy, and perf pass.
Next performance phase should attack protocol turn count and server/client
internal allocation tables, not add more request wrappers.
