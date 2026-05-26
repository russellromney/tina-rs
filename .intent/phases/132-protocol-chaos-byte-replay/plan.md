# Phase 132: Protocol Chaos And Byte Replay

## Status

- Future implementation phase.
- One PR.
- Can run beside 131 if it owns `tina-proof-harness`, parser tests, protocol
  regression tests, and docs.
- Can also run beside 133 if it does not change request-scope APIs.

## Grug Truth

Happy-path protocol tests lie.

Real peers reset, half-close, drip bytes, send bad frames, stall, and reconnect
too fast. Tina should answer with typed protocol truth, not logs.

## Current Code Facts

- `tina-proof-harness::bad_peer` already has TCP/HTTP-ish scenarios:
  half-close, reset, slowloris, stalled reader/writer, malformed frame, TLS
  handshake failure, reconnect storm.
- `ProtocolFact` already has stable families for HTTP/2, HTTP body, WebSocket,
  and gRPC.
- `TraceProjection::http2_streams`, `websocket_sessions`, and `grpc_status`
  already project protocol facts.
- Protocol facts already enter traces as `RuntimeFact::Protocol(...)`.
- Live replay capture exists, but `LiveReplayFact` currently carries capacity
  facts only. Protocol byte replay needs a new, explicit case format and a
  typed `LiveReplayFact::Protocol(...)` bridge.
- Phase 143 shipped overload bugbox helpers. Protocol chaos should reuse the
  same fail-closed discipline: unsupported facts are explicit rows, never
  "close enough."

## Goal

Ship credible protocol bad-peer proof:

- typed `ProtocolChaosReport`;
- bounded WebSocket compliance-style corpus;
- WebSocket byte replay;
- protocol facts in live replay cases;
- HTTP/2 and gRPC bad-peer probes;
- proof targets that run fast in CI and longer locally;
- regression fixes for bugs the harness finds.

## Does Not Include

- no full Autobahn badge;
- no giant fuzz platform;
- no downloaded corpus;
- no byte-perfect replay claim for arbitrary OS socket races;
- no log scraping;
- no hidden sleeps. Repeated flakes are bugs.

## Names And Homes

- Reuse `tina-proof-harness`.
- Keep `BadPeerScenario`.
- Add:
  - `ProtocolChaosReport`
  - `ProtocolChaosCase`
  - `WebSocketComplianceCase`
  - `ProtocolByteReplayCase`
  - `ProtocolByteReplayReport`
- Byte replay lives in `tina-proof-harness`, not `tina-sim` core.
  It replays materialized protocol bytes through pure parser/session state and
  compares typed protocol facts.
- Add `LiveReplayFact::Protocol(ProtocolFact)` in `tina-sim` so live capture
  can save protocol facts beside capacity facts. Raw socket physics that the
  simulator cannot model still records `UnsupportedFact`.

## Implementation

### Rock 1: Protocol Chaos Report

Extend the bad-peer harness so every scenario returns one typed report:

- case name;
- protocol family;
- bytes written/read;
- peer action;
- server/client terminal action;
- app delivery count;
- close/reset/status, when any;
- protocol facts observed;
- elapsed budget;
- unsupported facts, if any.

Do not remove the existing simple `BadPeerOutcome`; either wrap it or add a
conversion path so current users keep compiling.

### Rock 2: WebSocket Compliance Corpus

Add a small hermetic corpus:

- valid text;
- valid binary;
- valid fragmented text;
- invalid UTF-8 across fragments;
- reserved bits without extension;
- oversized control frame;
- oversized message;
- masked server frame;
- unmasked client frame;
- ping/pong edge;
- close handshake edge.

Each `WebSocketComplianceCase` must name:

- input bytes/actions;
- expected app messages;
- expected close code/error;
- expected `ProtocolFact`;
- expected report counters.

Malformed bytes must not reach app code as valid data.

### Rock 3: WebSocket Byte Replay

Add `ProtocolByteReplayCase` for WebSocket:

- ordered byte chunks;
- direction: client-to-server or server-to-client;
- max bytes and max chunks;
- expected app deliveries;
- expected close/reset;
- expected protocol facts;
- saved-case read/write;
- shrink helper that removes chunks/events and refreshes expected facts.
- stable hash/expected fact count over typed protocol facts, not debug text.

Unsupported live facts fail closed with `UnsupportedFact`. They do not pass as
exact replay.

### Rock 4: Protocol Facts In Live Replay

Extend live replay facts:

- `LiveReplayFact::Protocol(ProtocolFact)`;
- saved-case read/write for protocol facts;
- display lines that include protocol family;
- projection helpers that keep HTTP/2, WebSocket, or gRPC facts;
- mismatch rows that distinguish absent replayable fact from unsupported
  live-only fact.
- overload bugbox integration: a protocol chaos case with bounded pressure can
  save both protocol facts and capacity facts in one capture.

Do not hash debug strings. Use the typed `ProtocolFact` values and existing
stable trace/fact tags.

### Rock 5: HTTP/2 And gRPC Probes

Add hermetic bad-peer probes:

- invalid HTTP/2 frame size;
- duplicate pseudo-header;
- DATA after stream close;
- RST_STREAM while response body is active;
- GOAWAY while streams are active;
- flow-control window exhaustion;
- gRPC trailers missing `grpc-status`;
- oversized gRPC message.

Each probe asserts typed outcome and protocol fact. Do not merely assert
"connection closed".

### Rock 6: Make Targets And Docs

Update:

- `make proof-fast` to include the CI-sized bounded corpus;
- `make proof-bad-peer` to print typed reports with `--nocapture`;
- `make proof-soak` to repeat the same semantics at higher count;
- `examples/systems/README.md` with when to use proof harness vs local test.

## Required Proof

- CI proof is deterministic and fast.
- Long soak is opt-in and documented.
- Every scenario has stable name and typed report row.
- Valid fragmented WebSocket text reaches app code once after reassembly.
- Invalid WebSocket bytes do not reach app code as valid messages.
- WebSocket close code/fact/report counters match expected case data.
- HTTP/2 malformed frames map to typed reset/GOAWAY/protocol-error facts.
- gRPC missing-status and oversized-message cases return typed gRPC outcomes.
- Slow/stalled peer hits cap or timeout with visible report.
- Reconnect storm does not leak sessions, connect attempts, or queued bytes.
- Byte replay reproduces one saved bad-frame case.
- Shrink produces a smaller case and freshly observed expected facts.
- Saved live replay case can include WebSocket/HTTP2/gRPC protocol facts.
- Protocol fact mismatch says replayable-diverged vs unsupported-live-only.
- Unsupported byte replay facts fail closed.
- A saved case that mixes protocol facts and capacity/overload facts fails if
  either family diverges.
- Parser/protocol bug fixes include a failing-before, passing-after test.
