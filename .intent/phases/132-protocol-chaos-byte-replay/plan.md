# Phase 132: Protocol Chaos And Byte Replay

## Status

- Future implementation phase.
- Can run beside Phase 131 only if ownership stays mostly in tests,
  `tina-proof-harness`, and parser/replay fixes.
- One PR.

## Grug Truth

Happy-path protocol tests lie.

Real peers half-close, reset, send bad bytes, stall, reconnect, and violate
framing rules. Tina should turn those into typed outcomes and replayable facts.

## Goal

Make native protocol correctness credible.

Ship:

- reusable bad-peer scenarios
- WebSocket compliance-style classification
- WebSocket byte-level replay first form
- HTTP/2/gRPC bad-peer probes
- CI-sized and long-soak commands
- fixes for bugs the harness finds

## Starting Facts

- `tina-proof-harness::bad_peer` already has small TCP/HTTP scenarios.
- WebSocket server/client/session facts exist.
- HTTP/2/gRPC protocol facts exist.
- Live trace replay capture exists, but not byte-perfect protocol replay.
- Roadmap still names Autobahn-style classification and WebSocket byte replay.

## Does Not Include

- no full public benchmark suite
- no giant fuzz platform
- no network daemon recorder
- no claim of byte-perfect replay for every TCP stream
- no production compliance badge unless the test actually runs the corpus
- no hidden sleeps in tests; flaky timing means bug

## Decisions

- Home for reusable drivers: `tina-proof-harness`.
- User-facing names:
  - `BadPeerScenario`
  - `ProtocolChaosReport`
  - `WebSocketComplianceCase`
  - `ProtocolByteReplayCase`
- Byte replay is explicit:
  - supported protocol frames can replay
  - unsupported raw stream behavior records `UnsupportedFact`
  - no pretending a live OS socket race is deterministic
- WebSocket compliance first form is local and bounded. Do not add Autobahn in
  this phase. Use the same category names where they help, but keep the corpus
  hermetic and CI-stable.

## Implementation

### Rock 1: Bad-Peer Harness Upgrade

Extend typed bad-peer scenarios:

- half-close during request
- reset during body
- slowloris headers
- stalled writer
- stalled reader
- malformed HTTP/2 frame
- malformed WebSocket frame
- bad TLS handshake
- reconnect storm

Each scenario returns `ProtocolChaosReport`, not logs.

### Rock 2: WebSocket Compliance Cases

Add a bounded local corpus:

- valid text/binary/fragmented frames
- invalid UTF-8 across fragments
- reserved bits without negotiated extension
- oversized lengths
- masked server frame / unmasked client frame where applicable
- ping/pong/close handshake edge cases

Classify as pass/fail with typed reason.

### Rock 3: Protocol Byte Replay

Add first replay helper for materialized WebSocket frame bytes:

- history is bytes + direction + expected protocol facts
- cap total bytes/events
- replay parser/session state in sim or pure protocol harness
- unsupported live facts fail closed

### Rock 4: HTTP/2 And gRPC Bad-Peer Proofs

Add probes:

- invalid frame size
- duplicate pseudo-header
- stream reset while response body active
- GOAWAY while streams active
- gRPC trailers missing status
- oversized gRPC message

Every probe asserts typed outcome and protocol fact.

## Required Proof

- CI-sized proof target runs fast and deterministic.
- Long-soak target is opt-in and documented.
- Every scenario has typed outcome and no log scraping.
- Malformed bytes never reach app-level user message as valid data.
- Slow/stalled peer hits a cap or timeout with visible report.
- Reconnect storm does not leak live sessions or queued bytes.
- WebSocket byte replay reproduces a saved bad frame case.
- Unsupported byte replay facts fail closed.
- Any parser/protocol fix gets a before-failing, after-passing regression.
