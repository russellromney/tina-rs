# Hostile Review: 095 HTTP/2 Streaming Substrate

## Verdict

Good phase, but only if it stays a substrate phase and does the whole
server-side substrate.

The dangerous failure mode is underbuilding: response streaming alone is useful
but would force the next gRPC phase to reopen HTTP/2 for client-streaming and
bidi. Build request and response streaming here.

## Findings

### Superseded: Request Streaming Is Now Required

The earlier review fixed an overclaim by allowing request streaming to be
deferred. That was too timid. The updated plan makes request streaming required
for 095 to be done.

Emergency partial work should be renamed, not called done.

### Fixed In Plan: Production Client Was Too Loosely Blocked

The first draft implied HTTP/2 streaming substrate blocks production gRPC
client behavior by itself. That is only partly true.

Production pooled gRPC clients also need a separate HTTP/2 client connection
state machine: stream id allocation, settings, concurrent stream table,
flow-control accounting, reset/cancel, reconnect/retire, pooling, and pressure
reports.

### Still Risky: Trailer API Could Become gRPC-Shaped

Rock 3 must keep trailers ordinary HTTP/2 trailers. If it bakes in
`grpc-status` as a special transport concept, future ordinary HTTP/2 services
will inherit gRPC assumptions.

### Still Risky: Source Cancellation Can Be Faked

It is not enough to remove the HTTP/2 stream from the connection table. Tests
must prove the response source or request source receives cancel/release truth,
and that late source replies are visible.

### Still Risky: Flow-Control Tests Can Be Too Gentle

Tests must force both connection-window and stream-window blocking. A single
happy multi-DATA response does not prove pressure. The plan now requires both,
but implementation must avoid “read timeout means blocked” tests that pass for
the wrong reason.

### Still Risky: Full Duplex Ownership Can Become Muddy

The connection isolate must remain the only TCP reader/writer, but services
need request-body pull and response-body source handles. The implementation
must name who owns every buffer, wait, cancel handle, and trailer decision.

## Recommendation

Implement 095 in this order:

1. Response DATA streaming from a source.
2. Trailers after streamed DATA.
3. Request DATA streaming to a bounded source/handle.
4. Reset cancellation and late-reply proof in both directions.
5. Window/queue pressure proof in both directions.

Do not start gRPC streaming until 1-5 are green.
