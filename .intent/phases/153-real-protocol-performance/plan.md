# Phase 153: Real Protocol Performance

## Status

- Follows Phase 152.
- Phase 152 added honest HTTP/2/WebSocket rows and found the remaining hot
  spots. It did not make protocol performance good enough.
- This phase is the fix pass. It must change real protocol code and prove the
  changed public paths got cheaper.
- Start after Phase 152 is merged, or stack directly on Phase 152 and rebase
  after it merges. The Phase 152 rows are the baseline.

## Grug Truth

Do not add more pretty rows and call it performance.

The service path must move fewer bytes, allocate fewer objects, and take fewer
turns. Measure before. Change code. Measure after.

## Goal

Make native HTTP/2, gRPC-over-HTTP/2, and WebSocket public paths cheaper:

1. move HTTP/2 DATA payloads instead of cloning them;
2. send HTTP/2 buffered responses without cloning/slicing body bytes into new
   `Vec`s per frame;
3. reduce streaming/gRPC DATA frame copies on the request and response paths;
4. reduce one real WebSocket public-path copy/allocation cost;
5. pin before/after allocation and stage evidence on macOS and Linux/x86.

Done means at least three changed protocol paths show lower allocation/copy
cost in normal public API use. A harness-only improvement does not count.

## Non-Goals

- no new scheduler;
- no new benchmark-only fast path;
- no production performance claim;
- no weakening HTTP/2 flow-control, reset, GOAWAY, trailer, or gRPC status
  truth;
- no weakening WebSocket close, ping/pong, pressure, stale-session, or slow-peer
  truth;
- no public API churn unless it removes a real copy/allocation from normal user
  code.

## Starting Inventory

The known code paths are already pinned. Do not spend this phase rediscovering
them.

- `tina-http/src/http2/frame.rs`
  - `data_payload(&Frame)` clones every unpadded DATA payload even though
    server/client handlers own the `Frame`.
  - `Frame::encode(&self)` copies payload bytes into a new `Vec`.
- `tina-http/src/http2/server.rs`
  - `enqueue_response(&HttpResponse, ...)` clones buffered response bodies.
  - `send_pending_response` slices buffered bodies and calls `chunk.to_vec()`
    per DATA frame.
  - `flush_response_stream` drains response bytes into a new `Vec` per DATA
    frame.
- `tina-http/src/http2/client.rs`
  - `ActiveClientStream::outbound_body: VecDeque<u8>` turns buffered request
    bodies into per-byte queue work.
  - `flush_outbound_data` rebuilds each DATA frame chunk byte-by-byte.
  - `handle_data` uses the cloning `data_payload(&Frame)` helper.
- `tina-http/src/websocket.rs`,
  `tina-http/src/websocket_client.rs`, and `tina-http/src/connection.rs`
  - frame parse always copies payload out of the read buffer;
  - server app delivery sends both session-rich and legacy message variants,
    cloning text/binary payloads;
  - ping handling clones payload for both app notification and pong.
- `examples/systems/perf_native`
  - Phase 152 rows are the baseline. Use them. Do not replace them with a new
    easier workload.

## Rock 1: Move HTTP/2 DATA Payloads

Change HTTP/2 DATA extraction to consume the owned frame:

- add `into_data_payload(frame: Frame) -> Result<(Vec<u8>, usize), ...>` or an
  equivalent owned helper;
- unpadded DATA returns `frame.payload` directly;
- padded DATA still validates pad length and returns only the unpadded payload;
- also return or preserve the flow-control byte count, because padded DATA
  consumes wire payload length, not unpadded length.

Use the owned helper in both:

- `Http2Connection::handle_data`;
- `Http2ClientConnection::handle_data`.

Proof:

- unit tests for unpadded DATA, padded DATA, bad padding, empty DATA;
- server/client integration tests still pass for request body, response body,
  flow-control credit, and DATA-before-HEADERS errors;
- Phase 152 HTTP/2 rows show lower process allocation count or allocated bytes.

## Rock 2: Send Buffered HTTP/2 Responses Without Body Clones

Stop cloning buffered response bodies on the ordinary service reply path.

Implement the response path so the service-owned `HttpResponse` can be consumed
when it arrives:

- change `enqueue_response` to take `HttpResponse` by value for
  `CallOutcome::Replied(response)`;
- keep small generated fallback responses (`Full`, `Closed`, `Timeout`) owned
  too;
- validate `max_response_body_bytes` before storing/sending;
- put `HttpResponseBody::Buffered(bytes)` directly into `PendingResponse`;
- preserve Stream/ChunkedStream/WebSocket handling.

Then remove per-frame body copies:

- do not call `chunk.to_vec()` for every DATA frame;
- either encode DATA frames directly from body slices into the outbound write
  buffer, or move chunks out of the body without copying;
- preserve frame splitting at `peer_max_frame_size`;
- preserve trailers and END_STREAM rules.

Proof:

- direct test where a buffered response larger than one DATA frame still
  arrives byte-identical;
- direct test where trailers still arrive after DATA;
- oversized response still resets with `EnhanceYourCalm`;
- HTTP/2 steady-state response row has lower allocation count than Phase 152.

## Rock 3: Fix Streaming And gRPC DATA Copies

Make the streaming/gRPC DATA paths use the same cheaper frame writer.

Targets:

- server `flush_response_stream`;
- client `flush_outbound_data`;
- gRPC client/server paths that send DATA through HTTP/2.

Required shape:

- stop using `VecDeque<u8>` for buffered request bodies if it causes per-byte
  pop/copy on every DATA frame;
- store outbound body as a `Vec<u8>` plus cursor, or another bounded owned
  buffer shape that moves/slices without per-byte work;
- build DATA frames into pending write storage without an extra payload `Vec`
  when possible;
- keep streaming request sources bounded and cancelable.

Proof:

- gRPC unary and streaming tests still pass;
- HTTP/2 client buffered POST with a multi-frame body still sends all bytes;
- flow-control blocked streams resume after `WINDOW_UPDATE`;
- one gRPC or HTTP/2 client request row shows lower allocation/copy cost.

## Rock 4: Reduce One WebSocket Public-Path Copy

Reduce a real WebSocket user path, not only codec helper tests.

Implement this semantic cleanup:

- the connection owner emits one app event per wire event, not two;
- emit the session-rich variants (`SessionText`, `SessionBinary`,
  `SessionClose`, `SessionClosed`, etc.) from the connection;
- stop also enqueueing the legacy simple variants for the same wire event;
- keep the simple variants only if current app code uses them as app-local
  messages, but do not emit duplicate compatibility messages from the protocol
  owner;
- update examples/specimens/tests to use the session-rich copied path.

Then reduce one additional WebSocket copy if it is still visible in the row:

- eliminate the ping/pong payload clone by routing a single owned payload
  through app notification and pong encode with a small enum/state object; or
- make frame parsing drain/move payload bytes out of the read buffer without an
  extra `to_vec` for complete single-frame messages.

This rock is not optional. It must reduce at least one normal WebSocket
open/send/receive/close path cost.

Proof:

- WebSocket e2e text round trip still works;
- fragmented text/binary still reassembles;
- ping produces pong and app visibility;
- close handshake and stale-session truth still work;
- WebSocket perf row shows lower allocation count or allocated bytes.

## Rock 5: Reduce One Turn Count Or Stage Gap

Use the Phase 152 stage rows to remove at least one avoidable protocol turn or
one repeated stage gap. This rock is required.

Allowed examples:

- coalesce immediate protocol writes after a response into one write effect
  instead of a later handler turn;
- avoid a needless read/write ping-pong when a queued write is already ready;
- avoid an extra app-control message in the WebSocket perf service if the
  state transition can be represented by an existing typed event.

Not allowed:

- hiding suspension in callbacks;
- bypassing service call/reply semantics;
- skipping typed `Full` / `Closed` / `Timeout` outcomes.

Proof:

- stage counts for one HTTP/2 or WebSocket row decrease;
- the PR explains which turn disappeared and why it was not a policy boundary;
- no trace or DST regression.

## Rock 6: Perf Evidence And Gates

Record before/after rows with the Phase 152 harness:

- macOS/aarch64 release rows;
- Linux/x86_64 release rows using the existing Fly/Ubuntu path;
- clean git SHA rows, not dirty rows;
- allocation count and allocated-byte comparisons;
- p50/p90/p99 shown but not overclaimed.

Add or tighten deterministic ceilings for changed paths:

- allocation ceilings for changed protocol rows where stable;
- stage ceilings for changed stage rows;
- no latency ceiling so tight that shared CI gets flaky.

## Docs

Update:

- `.intent/phases/153-real-protocol-performance/perf_history.jsonl`;
- `examples/systems/perf_native/README.md`;
- `ROADMAP.md`;
- `CHANGELOG.md`.

Docs must say:

- what got faster or cheaper;
- what still costs too much;
- macOS vs Linux evidence;
- no production performance claim yet.

## Proof Commands

Focused:

- `cargo test -p tina-http --all-targets`
- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture`
- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture`

Regression:

- `cargo fmt --all --check`
- `cargo clippy -p tina-http --all-targets -- -D warnings`
- `make proof-fast`

Linux:

- run the existing Linux/Fly perf bundle and save the output in the phase dir;
- if the builder cannot run Linux, the PR is not final until the orchestrator
  runs it.

## Done

- At least three real protocol code paths allocate/copy less.
- At least one protocol stage row has fewer turns.
- HTTP/2, gRPC, and WebSocket semantics are still proved end-to-end.
- macOS and Linux release perf evidence are saved.
- Docs are honest about remaining cost.
- Phase 153 does not become Phase 152 again.
