# Hostile Review: 095 HTTP/2 Streaming Substrate

## Verdict

Good phase, but only if it stays a substrate phase and does the whole
server-side substrate.

The dangerous failure mode is underbuilding: response streaming alone is useful
but would force the next gRPC phase to reopen HTTP/2 for client-streaming and
bidi. Build request and response streaming here.

## Findings

### Fixed In Code Review: Report Calls Were Broken By Request Pulls

The first implementation changed `Http2Connection`'s reply type to
`RequestChunkReply`, which silently broke the existing `Report` call path.
That was a bad substrate regression. The connection now replies with an
explicit `Http2ConnectionReply` enum so request chunks and reports can coexist.

### Fixed In Code Review: Request Trailers Were Fake EOF

Trailing request HEADERS were being treated as clean end-of-stream for the
streaming request path. That is a lie until real request-trailer semantics are
implemented. The code now rejects trailing request HEADERS with stream reset.

### Fixed In Code Review: Body Cap Counted Resident Bytes, Not Total Bytes

The streaming request cap initially counted queued resident chunks. A fast
consumer could drain chunks and let total request bytes exceed `max_body_bytes`.
The cap now applies to total bytes received for the HTTP/2 stream.

### Fixed In Execution Slice: Tiny Chunk Credit Flood

A thousand-message client-streaming test exposed another pressure bug: the
connection emitted two `WINDOW_UPDATE` frames for every tiny consumed DATA
chunk, filled the bounded outbound queue, and reset the stream. Request-body
window credit is now coalesced and flushed at a threshold or EOF.

### Fixed In Hostile Review: Buffered Responses Were All-Or-Nothing

Large buffered responses used to wait until the entire body fit both the
connection and stream send windows. That could deadlock a healthy client on a
large unary response. Buffered responses now send headers and DATA
incrementally, keep unsent bytes pending, and resume on `WINDOW_UPDATE`.

### Fixed In Hostile Review: Peer Initial Stream Window Was Ignored

The server ACKed SETTINGS but ignored `SETTINGS_INITIAL_WINDOW_SIZE`, so real
clients could advertise larger stream windows and still get default-window
behavior. The connection now applies the SETTINGS delta to active streams and
uses the peer's current initial stream window for new streams.

### Fixed In Hostile Review: Duplicate Pseudo-Headers Were Accepted

The HPACK/header path used to let duplicate `:method`, `:path`, `:scheme`,
`:authority`, or `:status` overwrite earlier values. Duplicate pseudo-headers
now fail as malformed HTTP/2 input.

### Fixed In Hostile Review: Streaming EOF Could Lose END_STREAM

Streaming response EOF used to ignore outbound queue failure while enqueueing
final trailers or the final empty DATA frame, then remove the stream. The EOF
marker now stays pending until the final frame is actually queued.

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

### Fixed In Plan: END_STREAM Was Under-Specified

The plan said "trailers after DATA" but did not force a precise HTTP/2
`END_STREAM` state machine. That leaves room for bugs where a stream accepts
DATA after EOF, treats request trailers as body, or ends response DATA before
trailers. The plan now requires explicit END_STREAM and request-trailer policy
before coding.

### Fixed In Plan: Full-Duplex Progress Needed Proof

The plan required request and response streaming, but did not require tests that
one blocked direction still allows the other direction to make progress. That
is the classic bidi deadlock. The plan now requires full-duplex substrate tests
for inbound progress while outbound is window-blocked and reset handling while
outbound is blocked.

## Recommendation

Implement 095 in this order:

1. Response DATA streaming from a source.
2. Trailers after streamed DATA.
3. Request DATA streaming to a bounded source/handle.
4. Reset cancellation and late-reply proof in both directions.
5. Window/queue pressure proof in both directions.
6. Full-duplex blocked-one-way progress proof.

Do not start gRPC streaming until 1-6 are green.
