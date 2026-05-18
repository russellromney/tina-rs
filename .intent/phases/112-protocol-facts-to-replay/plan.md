# Phase 112: Protocol Facts To Replay

## Status

- IDD implementation phase.
- One PR.
- Can run beside Phase 110 and Phase 111.
- Owns protocol fact trace/replay plumbing.
- Does not own HTTP/WebSocket/gRPC feature expansion.
- Does not own service report assembly.

## Grug Truth

Protocol bugs are real bugs.

Today some protocol truth lives in reports.

DST sees runtime events.

If a fact matters in replay, it needs a replayable fact shape.

No fake exact replay.

Unsupported facts are good honesty.

## Goal

Move selected protocol lifecycle/pressure facts into runtime/sim replay truth.

First set:

- HTTP/2 stream opened
- HTTP/2 stream closed
- HTTP/2 stream reset
- HTTP/2 flow-control full
- HTTP body high-water
- WebSocket slow-peer close
- WebSocket session close reason
- gRPC final status sent
- gRPC final status received

The user outcome:

```text
live protocol weirdness -> captured fact -> sim replay / unsupported fact
```

## Names

Use user-facing protocol fact names.

Preferred shape:

```rust
RuntimeEventKind::ProtocolFact(ProtocolFact)
```

with:

```rust
ProtocolFact::Http2(...)
ProtocolFact::HttpBody(...)
ProtocolFact::WebSocket(...)
ProtocolFact::Grpc(...)
```

Names must describe the protocol event, not the internal counter.

Avoid:

- `MetricUpdated`
- `ReportObserved`
- `DebugFact`
- storage-detail names

## Non-Goals

- no new protocol features
- no HTTP/2 RFC expansion
- no gRPC interceptor stack
- no WebSocket room rewrite
- no live socket replay claim
- no giant trace dump
- no changing existing stable hash tags for unrelated events
- no making reports disappear; reports still exist

## Build

### Rock 1: Protocol Fact Vocabulary

Add a small protocol fact vocabulary.

Required fields:

- protocol family
- connection/session/stream id where available
- direction where relevant
- reason/outcome where relevant
- pressure value where relevant
- stable debug/render token

Required facts:

- `Http2StreamOpened`
- `Http2StreamClosed`
- `Http2StreamReset`
- `Http2FlowControlFull`
- `HttpBodyHighWater`
- `WebSocketSlowPeerClosed`
- `WebSocketSessionClosed`
- `GrpcFinalStatusSent`
- `GrpcFinalStatusReceived`

If a crate does not have an id for a fact, add a monotonically allocated
connection/session/stream-local id at the point the protocol state is created.
Do not invent random ids.

Tests:

- each fact has stable debug/render output
- each fact maps to a stable hash tag
- adding the new variant does not renumber existing stable hash tags
- unknown future fact is not silently ignored by projection code

### Rock 2: Runtime Trace Emission

Emit protocol facts from existing protocol code paths.

Required sources:

- `tina-http` HTTP/2 stream lifecycle
- `tina-http` body high-water / chunk pressure path
- `tina-http` WebSocket slow peer/session close path
- `tina-http` native gRPC final status send/receive path

Rules:

- emit at the point the fact becomes true
- do not emit from tests only
- do not double-count report-only counters
- existing report counters stay intact
- trace pressure does not become an unbounded queue

Tests:

- HTTP/2 live test sees stream open/close fact
- HTTP/2 flow-control pressure test sees full fact
- body pressure test sees high-water fact
- WebSocket slow-peer test sees close fact
- gRPC status test sees final status fact

### Rock 3: Simulator Support

Add simulator trace support for the same facts.

Rules:

- if sim models the protocol path, emit the same fact
- if the live fact comes from a live-only path, replay reports
  `UnsupportedProtocolFact`
- no fake pass
- no dropping facts to make hashes match

Tests:

- sim HTTP/2 path emits matching stream fact
- unsupported fact produces a typed unsupported replay result
- live-vs-sim projection fails closed when a fact is neither included nor
  ignored

### Rock 4: Replay Projection Presets

Add small projection helpers for protocol facts.

Preferred names:

- `TraceProjection::protocol_facts()`
- `TraceProjection::http2_streams()`
- `TraceProjection::websocket_sessions()`
- `TraceProjection::grpc_status()`

Rules:

- presets are just named include/ignore sets
- every ignored event kind is listed
- unknown event kinds fail closed
- projected hash/count are visible

Tests:

- preset includes expected protocol facts
- preset rejects unknown event kind
- projected mismatch names the missing/extra fact

### Rock 5: One Saved Replay Proof

Add one service-shaped saved replay case.

Must include:

- one protocol fact
- one Tina pressure/lifecycle fact
- visible config/topology
- expected count/hash or projected count/hash

Use this proof:

- HTTP/2 stream reset under flow pressure.

Also add one explicit unsupported-fact test using a synthetic live-only
protocol fact.

Tests:

- saved case passes when facts match
- changing config changes or invalidates replay
- removing the protocol fact fails the check
- unsupported fact path is typed and documented

### Rock 6: Docs

Update:

- `docs/tina-user-guide/08-simulation-and-dst.md`
- protocol docs for HTTP/WebSocket/gRPC as applicable
- `examples/systems/system_live_replay_bugbox/README.md`
- `examples/FINDINGS.md`
- `CHANGELOG.md`

Docs must say:

- reports are for operators
- protocol facts in trace are for replay/debug
- live physics is not replay
- unsupported facts are not failure; they are honesty

## Required Proof

Run at least:

```text
cargo fmt --all --check
cargo test -p tina-runtime protocol_fact -- --nocapture
cargo test -p tina-sim protocol_fact -- --nocapture
cargo test -p tina-http http2 --tests
cargo test -p tina-http websocket --tests
cargo test -p tina-http grpc --tests
cargo clippy -p tina-runtime -p tina-sim -p tina-http --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

## Hostile Review Checklist

Before merge, prove:

- existing stable hashes do not churn for unrelated events
- protocol facts are emitted by real code paths
- report counters and trace facts do not disagree
- sim either emits the fact or reports unsupported
- projection is fail-closed
- one saved case proves user-visible replay/debug value
- no protocol feature work snuck in

## Done Means

Protocol pressure/lifecycle facts are no longer trapped inside ad-hoc reports.
They can be captured, projected, replayed, or rejected as unsupported with a
typed reason.
