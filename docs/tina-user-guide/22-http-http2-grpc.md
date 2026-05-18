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
control, HTTP body high-water, WebSocket session lifecycle, and the
native gRPC final status frame.

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

## What does NOT emit facts

- Blocking host helpers. `grpc_unary_call_h2c_blocking` is a
  convenience client that hits an HTTP/2 listener directly. It is not
  a Tina isolate. It does not emit replay facts. Reports still count
  outbound traffic; replay is silent on this path until Tina has a
  native gRPC client isolate.
- Test-only helpers. Facts only ride real protocol code, never test
  shims.

## Replay projections

`TraceProjection::protocol_facts()` keeps only `FactObserved` events.
Three siblings (`http2_streams`, `websocket_sessions`, `grpc_status`)
share the same shape today and exist for call-site clarity. Every
unrelated event kind is explicitly listed in `ignored`, so the
projection still fails closed on unknown event kinds.

```rust
use tina_sim::dst::TraceProjection;

let shape = tina_sim::dst::project_trace_shape(
    &trace,
    &TraceProjection::protocol_facts(),
)?;
```

A live trace that contains a fact the simulator cannot produce is
reported through the typed `ProtocolReplayMismatch::UnsupportedProtocolFact`
arm. That is honesty, not failure: the simulator does not model real
TCP timing, so some live-only physics has no sim counterpart.

## Stable tags

The trace encodes effect kinds, runtime event kinds, and protocol fact
families with stable single-byte tags. Phase 112 adds:

- `EffectKind::Fact` = 13
- `RuntimeEventKind::FactObserved` event tag = 36
- `RuntimeFact::Protocol` family tag = 1
- `ProtocolFact` variant tags 1..8 in the order documented in the
  source file.

Existing tags are not renumbered. A stable-hash regression test pins
this so future variant additions cannot accidentally move them.
