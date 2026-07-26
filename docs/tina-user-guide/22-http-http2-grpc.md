# HTTP, HTTP/2, And gRPC Protocol Facts

This page explains protocol facts: named, replayable observations a Tina
protocol isolate can emit alongside ordinary effects.

## What is a protocol fact

A *fact* is one named observation that an isolate emits with
`Effect::Fact(I::Fact)`. The runtime translates it through
`IntoRuntimeFact` and records `RuntimeEventKind::FactObserved { fact }`
in the deterministic trace. Replay sees the same fact in the same order.

Today `RuntimeFact` has one family: `RuntimeFact::Protocol(ProtocolFact)`.
The first protocol fact set covers HTTP/2 stream lifecycle and flow
control, HTTP body high-water, WebSocket server/client session lifecycle,
and the native gRPC final status frame.

```rust
use tina_runtime::{
    Http2StreamId, ProtocolConnectionId, ProtocolDirection, ProtocolFact,
};

tina::fact::<Self>(ProtocolFact::Http2StreamOpened {
    connection: ProtocolConnectionId::new(self.connection_id),
    stream: Http2StreamId::new(stream_id),
    direction: ProtocolDirection::Inbound,
})
```

## Reports vs facts

Reports answer "what is happening right now?" for operators. Counters,
gauges, high-water marks, queued bytes. They stay intact.

Facts answer "did this protocol event happen?" for replay and debug.
A fact is the same in live and in simulation.

Operators read reports. Replay reads facts. They do not compete.

## Ordinary isolates pay nothing

Every isolate has a `Fact` associated type. Ordinary code declares
`Fact = std::convert::Infallible` (the macros set it by default) and
never calls `tina::fact::<Self>(...)`. An ordinary isolate trying to
emit a `ProtocolFact` is a compile error: the function signature wants
exactly `I::Fact`, and there is no `ProtocolFact -> Infallible`
conversion.

## Opt in for protocol code

Protocol isolates declare `Fact = ProtocolFact`. The
`#[tina_runtime::isolate]` macro accepts `fact = ProtocolFact`:

```rust
#[tina_runtime::isolate(
    message = MyMsg,
    fact = tina_runtime::ProtocolFact,
)]
impl MyConnection { /* ... */ }
```

For a manual `Isolate` impl, write `type Fact = ProtocolFact;` next to
the other associated types. The runtime registration path additionally
requires `I::Fact: IntoRuntimeFact`; the bundled `ProtocolFact` impl
covers this. Any user-defined fact type must implement `IntoRuntimeFact`
or registration is a compile error.

## Native gRPC Client

Tina is a native gRPC client, not only a server. `GrpcClient` sits over
an `Http2ClientConnection` isolate — no Tokio, no hidden queue. Unary
calls encode one `prost` message, submit it as one HTTP/2 stream, and
decode the reply into a typed `GrpcUnaryOutcome` where the gRPC status
is first-class:

```rust,ignore
use tina_http::{GrpcClient, GrpcLimits, GrpcUnaryOutcome,
    Http2ClientConnection, Http2ClientMsg, Http2Target};

// 1. Register a connection isolate to the target authority and Begin it.
let target = Http2Target::H2c { authority: "svc".into(), addr };
let conn = app.register_root::<Http2ClientConnection<S>, _>(
    Http2ClientConnection::new(target, Default::default())?, 32)?;
app.try_send(conn, Http2ClientMsg::Begin)?;

// 2. A GrpcClient is a thin wrapper over that connection.
let client = GrpcClient::new(conn, GrpcLimits::default());

// 3. Build the submit, call the connection, decode the outcome.
let submit = client.unary_request("/pkg.Service/Method", &request_msg)?;
let CallOutcome::Replied(reply) =
    app.call_blocking(client.connection(), submit, timeout)?
else { /* host call timed out / target gone */ return; };
match client.unary_outcome_from_reply::<ReplyMsg>(reply) {
    GrpcUnaryOutcome::Ok(msg) => { /* OK status + decoded message */ }
    GrpcUnaryOutcome::Status(status) => { /* non-OK gRPC status */ }
    GrpcUnaryOutcome::Transport(t) => { /* HTTP/2 transport failure */ }
    GrpcUnaryOutcome::Malformed(e) => { /* not well-formed gRPC */ }
}
```

A non-OK status is `GrpcUnaryOutcome::Status`, never hidden inside a
successful response. The received status is emitted as a
`ProtocolFact::GrpcFinalStatusReceived` fact — the receive-side mirror
of the server's `GrpcFinalStatusSent`.

Streaming client calls use the same service:

- `GrpcClient::server_streaming_request(path, &req)` opens a response stream.
  Pull chunks with `Http2ClientMsg::ResponseNext` and fold them through
  `GrpcStreamDecoder`.
- `GrpcClient::client_streaming_request(path, source)` sends a streamed request
  body and decodes one final response.
- `GrpcClient::bidi_request(path, source)` sends a streamed request body while
  the caller pulls streamed response chunks.

The HTTP/2 client can target `Http2Target::H2c` or `Http2Target::Tls`.
If TLS ALPN does not select `h2`, the caller sees
`Http2ClientOutcome::TlsAlpnMismatch`.

## What does NOT emit facts

- Blocking host helpers. `grpc_unary_call_h2c_blocking` is a test-only
  convenience that hits an HTTP/2 listener directly with synchronous
  socket code. It is not a Tina isolate and emits no replay facts.
  **Prefer the native `GrpcClient` path above** — it runs on the
  runtime's TCP rail and emits `GrpcFinalStatusReceived`. The blocking
  helper remains only for quick host scripts and tests.
- Test-only helpers. Facts only ride real protocol code, never test
  shims.

## Replay projections

The projection helpers do what their names say. Each one is a thin
constructor over the shared `TraceProjection::Projected` shape, so
unknown runtime event kinds still fail closed.

- `TraceProjection::protocol_facts()` keeps every `FactObserved`
  event, regardless of protocol family. Use this when you want to
  pin "the trace had exactly N protocol facts, in this stable order"
  without narrowing to one family.
- `TraceProjection::protocol_family(ProtocolFamily::Http2 |
  ::WebSocket | ::Grpc | ::HttpBody)` keeps `FactObserved` events
  whose `RuntimeFact::Protocol(fact).family()` matches. Non-matching
  facts are dropped silently the way `ignored` event kinds are.
- `TraceProjection::http2_streams()` /
  `TraceProjection::websocket_sessions()` /
  `TraceProjection::grpc_status()` are named shortcuts for the
  matching `protocol_family(...)` call. They filter by family, they
  are **not** aliases for `protocol_facts()`.

```rust
use tina_runtime::ProtocolFamily;
use tina_sim::dst::TraceProjection;

// Every protocol fact, regardless of family.
let all_facts = tina_sim::dst::project_trace_shape(
    &trace,
    &TraceProjection::protocol_facts(),
)?;

// Only HTTP/2 stream-level facts.
let http2_only = tina_sim::dst::project_trace_shape(
    &trace,
    &TraceProjection::http2_streams(),
)?;

// Equivalent and slightly more explicit.
let http2_only_explicit = tina_sim::dst::project_trace_shape(
    &trace,
    &TraceProjection::protocol_family(ProtocolFamily::Http2),
)?;
```

The family check reads `RuntimeFact::Protocol(fact).family()`; no
debug-string parsing happens. A trace that mixes HTTP/2, WebSocket,
and gRPC facts in the same run produces three distinct trace hashes
under the three named helpers — they each project onto a different
subset of the same events.

A live trace that contains a fact the simulator cannot produce is
reported through the typed `ProtocolReplayMismatch::UnsupportedProtocolFact`
arm. That is honesty, not failure: the simulator does not model real
TCP timing, so some live-only physics has no sim counterpart.

## Stable tags

The trace encodes effect kinds, runtime event kinds, and protocol fact
families with stable single-byte tags:

- `EffectKind::Fact` = 13
- `RuntimeEventKind::FactObserved` event tag = 36
- `RuntimeFact::Protocol` family tag = 1
- `ProtocolFact` variant tags 1..8 in the order documented in the
  source file.

Existing tags are not renumbered. A stable-hash regression test pins
this so future variant additions cannot accidentally move them.
