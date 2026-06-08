# Phase 153: Real Protocol Performance

## Status

- Follows Phase 152.
- Phase 152 gives the perf rows and baseline. It mostly measured the problem.
- Phase 153 fixes the problem. It must change protocol code, not just harness
  code.
- Start after Phase 152 merges, or stack on it and rebase after merge.

## Grug Truth

Rows are not performance. Faster code is performance.

Move fewer bytes. Allocate fewer things. Take fewer turns. Measure before and
after on the public paths users actually call.

## Must Improve

All of these must get cheaper in normal public API use:

- HTTP/2 steady-state row;
- one gRPC-over-HTTP/2 row;
- one WebSocket public session row.

Also required:

- at least one HTTP/2 or WebSocket stage row has fewer turns;
- before/after rows come from the same machine class, release profile, and
  sample policy;
- macOS and Linux/x86 evidence are saved.

Helper tests can prove a clone is gone. They do not finish the phase unless
the public rows move.

## Do Not Change

- no new scheduler;
- no benchmark-only fast path;
- no production performance claim;
- no weakening HTTP/2 flow-control, reset, GOAWAY, trailer, or gRPC status
  truth;
- no weakening WebSocket close, ping/pong, pressure, stale-session, or slow-peer
  truth;
- no unrelated public API churn;
- no compatibility wrapper that keeps the old duplicate/allocation-heavy path
  as the documented default.

## Known Hot Spots

Do not start with a broad audit. Start here.

- `tina-http/src/http2/frame.rs`
  - `data_payload(&Frame)` clones unpadded DATA even though handlers own the
    `Frame`.
  - `Frame::encode(&self)` copies payload bytes into a new `Vec`.
- `tina-http/src/http2/server.rs`
  - `enqueue_response(&HttpResponse, ...)` clones buffered bodies.
  - `send_pending_response` calls `chunk.to_vec()` per DATA frame.
  - `flush_response_stream` drains response bytes into a new `Vec` per DATA
    frame.
- `tina-http/src/http2/client.rs`
  - `outbound_body: VecDeque<u8>` causes per-byte request-body work.
  - `flush_outbound_data` rebuilds each DATA frame byte-by-byte.
  - `handle_data` uses the cloning DATA helper.
- WebSocket server/client
  - parsing copies payload out of the read buffer;
  - server delivery emits both session-rich and legacy app messages;
  - ping handling clones payload for app notification plus pong.

## Rock 1: Owned HTTP/2 DATA

Add an owned DATA helper, for example:

`into_data_payload(frame: Frame) -> Result<(Vec<u8>, usize), Http2ProtocolError>`

Rules:

- unpadded DATA returns `frame.payload` directly;
- padded DATA validates padding and returns only unpadded bytes;
- the returned `usize` is the flow-control wire length;
- server and client DATA handlers use the owned helper;
- the old cloning helper is removed or renamed so handlers do not pick it by
  accident.

Proof:

- unpadded DATA extraction does not allocate beyond the already-owned payload;
- padded, bad-padded, empty DATA tests pass;
- request/response body, flow-control credit, and DATA-before-HEADERS tests
  still pass.

## Rock 2: HTTP/2 Response Writer

Stop cloning buffered responses.

Rules:

- consume `HttpResponse` by value on `CallOutcome::Replied(response)`;
- generated error responses are owned too;
- validate `max_response_body_bytes` before storing/sending;
- store `HttpResponseBody::Buffered(bytes)` directly in `PendingResponse`;
- add a direct DATA writer (`encode_frame_into` / `push_data_frame`) that writes
  frame header plus payload into the pending write buffer/queue;
- no per-frame DATA payload `Vec` for multi-frame buffered responses;
- Stream, ChunkedStream, WebSocket, trailers, END_STREAM, and frame splitting
  still work.

Proof:

- multi-frame buffered response arrives byte-identical;
- trailers still arrive after DATA;
- oversized response still resets with `EnhanceYourCalm`;
- HTTP/2 steady-state allocation count or allocated bytes improves.

## Rock 3: Streaming And gRPC DATA

Use the same cheaper DATA writer for streaming and gRPC paths.

Rules:

- fix server `flush_response_stream`;
- fix client `flush_outbound_data`;
- fix gRPC client/server DATA paths that ride HTTP/2;
- replace per-byte `VecDeque<u8>` request-body draining with an owned buffer
  plus cursor/range, or an equivalent bounded shape;
- drop/compact consumed large buffers when a stream finishes;
- streaming sources stay bounded and cancelable.

If Phase 152 did not land a gRPC perf row, add the smallest public unary gRPC
row first, save it as the before row, then improve it in this phase.

Proof:

- gRPC unary and streaming tests pass;
- HTTP/2 buffered POST with a multi-frame body sends all bytes;
- flow-control blocked streams resume after `WINDOW_UPDATE`;
- one gRPC row and one HTTP/2 client request row improve, unless the PR proves
  they share the same changed code path and only one row can move separately.

## Rock 4: WebSocket Single Event Path

Stop duplicate protocol-owner app delivery.

Rules:

- one wire event becomes one app event;
- connection owner emits session-rich events (`SessionText`, `SessionBinary`,
  `SessionClose`, `SessionClosed`, etc.);
- it no longer also emits legacy `Text` / `Binary` / `Close` for the same wire
  event;
- simple variants may remain for app-local messages, but not as duplicate
  protocol-owner output;
- examples/specimens/tests use the session-rich path.

This may be a breaking cleanup. Tina has no stable API yet. Prefer one clear
new-user path over a compatibility tax.

Then remove one more visible WebSocket copy if the row still shows it:

- ping/pong payload clone; or
- frame parse payload copy for complete single-frame messages.

Proof:

- text round trip works;
- fragmented text/binary works;
- ping produces pong and app visibility;
- close handshake and stale-session truth work;
- a compile/doctest-style app compiles using session-rich events only;
- WebSocket row allocation count or allocated bytes improves.

## Rock 5: Fewer Turns

Remove at least one avoidable protocol turn or repeated stage gap.

Allowed examples:

- write immediately after a response instead of waiting for another handler
  turn;
- skip a read/write ping-pong when queued write bytes are already ready;
- remove an app-control message from the canonical public specimen path.

Rules:

- the removed turn must be in runtime/protocol code or a canonical public
  specimen path;
- perf-harness-only shortcuts do not count;
- do not hide suspension in callbacks;
- do not bypass service call/reply truth;
- keep typed `Full` / `Closed` / `Timeout` outcomes.

Proof:

- one HTTP/2 or WebSocket stage row has fewer turns;
- PR explains which turn disappeared and why it was not a policy boundary;
- trace/DST proof still passes.

## Evidence

Save in the phase dir:

- Phase 152 before rows used for comparison;
- Phase 153 after rows;
- macOS/aarch64 release rows;
- Linux/x86_64 release rows;
- clean git SHA rows, not dirty rows.

Include a short before/after table in the PR body or phase notes:

- process allocations;
- allocated bytes;
- p50/p90/p99;
- stage count.

If latency gets worse while allocations improve, do not bury it. Fix it or mark
the phase incomplete.

## Docs

Update:

- `.intent/phases/153-real-protocol-performance/perf_history.jsonl`;
- `examples/systems/perf_native/README.md`;
- `ROADMAP.md`;
- `CHANGELOG.md`.

Say what got cheaper, what still costs too much, which rows are macOS/Linux,
and that this is still not a production performance claim.

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

- run the existing Linux/Fly perf bundle and save output in the phase dir;
- if the builder cannot run Linux, the PR is not final.

## Done

- HTTP/2 steady-state, one gRPC row, and one WebSocket row are cheaper in
  public API use.
- named clone/copy sites are removed, or the PR gives a code-level reason one
  cannot be removed safely yet.
- one protocol stage row has fewer turns.
- HTTP/2, gRPC, and WebSocket semantics are still proved end-to-end.
- macOS and Linux release evidence are saved.
- docs are honest.
