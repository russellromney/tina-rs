# Hostile Review: 095 HTTP/2 Streaming Substrate

## Verdict

Good phase, but only if it stays a substrate phase.

The dangerous failure mode is overclaiming: response streaming alone is enough
to unblock gRPC server-streaming, but it is not enough for client-streaming,
bidirectional streaming, or production pooled gRPC clients.

## Findings

### Fixed In Plan: Request Streaming Was Overclaimed

The first draft said the phase claim included incremental request bodies while
also allowing request streaming to be deferred. That was a contradiction.

The plan now says response streaming is the minimum done claim, and request
streaming only ships if Rock 2 lands with matching proof.

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

### Still Risky: Request Streaming May Be Too Much For One PR

The sane implementation order is response streaming plus trailers first.
Request streaming is valuable, but if it threatens the PR, defer it loudly.
That still unlocks gRPC server-streaming next.

## Recommendation

Implement 095 in this order:

1. Response DATA streaming from a source.
2. Trailers after streamed DATA.
3. Reset cancellation and late-reply proof.
4. Window/queue pressure proof.
5. Only then consider request streaming.

Do not start gRPC server-streaming until 1-4 are green.
