# Phase 122: Ecosystem Hooks And Async Boundary

## Status

- Future implementation plan for Wave B.
- Can run in parallel with Phase 121 if ownership stays in public hook
  traits, extension smoke crates, capability reports, and docs.
- Runs after Phase 115 so hooks respect the core/battery boundary. Can absorb
  lessons from Phase 117 codecs and Phase 118 service policies.

## Spike Facts

- Tina already has native TCP/TLS/HTTP/1/HTTP2/gRPC/WebSocket/timers/files/
  process/DNS/pools/codecs work in flight or shipped. Async interop is not the
  first answer for these.
- Existing bridges (`reqwest`, `sqlx`, `sqlite`, `aws`, `tower`, `tokio`) prove
  the bridge pattern, but each still hand-rolls setup/lifecycle/metrics shape.
- Phase 113 is the bridge author kit seed. This phase should expose public
  hooks that make third-party bridge/codecs/policy crates possible without
  private runtime access.
- Rust ecosystem convention is traits + feature-gated crates + examples, not a
  dynamic plugin ABI.

## Purpose

Let Tina grow an ecosystem without every feature landing in core.

The user story:

```text
I can plug in a codec, bridge, capacity surface, event sink, or policy without
private runtime access and without weakening bounded/DST truth
```

## Includes

- public capacity surface hook
- bounded event sink hook
- sync codec adapter hook
- service policy hook
- bridge author smoke crate using the Phase 113 vocabulary
- fake external bridge smoke crate using only public APIs
- custom codec smoke crate using only public APIs
- custom admission/policy smoke crate using only public APIs
- runtime capability report for rails/cancel/drain/sim support
- clear async boundary docs:
  - native Tina path
  - bridge path
  - unsupported path

## Does Not Include

- no dynamic plugin ABI
- no broad `Future`/`Stream` bridge unless a smoke crate proves the bounded
  shape
- no hidden Tokio under native Tina services
- no hook that bypasses trace/capacity/cancel truth
- no semver promise for internal runtime modules

## Implementation Shape

Expose small stable seams:

```text
CapacitySurface
BoundedEventSink
SyncCodec
ServicePolicy
BridgeAuthorParts
RuntimeCapabilityReport
```

Rules:

- Hooks use owned reports and typed outcomes, not callbacks into private runtime
  internals.
- Extension crates may observe and report capacity; they may not mutate runtime
  queues directly.
- Codecs are synchronous parser/encoder state. Tina owns I/O and backpressure.
- Event sinks are bounded and choose `drop_oldest`, `drop_newest`, or `reject`.
- Service policies return decisions; they do not send messages or retry work.
- Bridge author parts name install result, address, closer, metrics, pressure,
  shutdown, worker-terminal outcome, and caller-observed outcome.
- Capability reports say supported, unsupported, live-only, sim-backed,
  cancel-backed, tombstoned, and drain-backed explicitly.
- Async boundary docs classify examples:
  - native Tina: use Tina-owned rails
  - bridge: external async ecosystem is valuable, bounded at bridge edge
  - unsupported: cannot preserve bounded/DST truth yet

## Extension Smoke Crates

Add small workspace-excluded crates under `examples/extensions/` that use public
APIs only and run by manifest path:

- `tina-extension-fake-bridge`: one bounded worker around a blocking function,
  with install/closer/metrics/pressure/shutdown docs.
- `tina-extension-custom-codec`: line or length codec adapter used by a tiny TCP
  service.
- `tina-extension-capacity-surface`: custom pressure surface joining
  `CapacitySummary`.
- `tina-extension-service-policy`: custom per-key admission decision that uses
  `ctx.now()` and reports replayable state.

No smoke crate may import `tina_runtime::runtime_internal` or private modules.

## Proof Shape

- extension smoke crates compile and run using only public APIs
- custom surface joins normal capacity summary
- event sink is bounded and reports drops/full/closed
- codec hook keeps parser state replayable
- capability report says supported/unsupported/cancel/drain/sim truth
- compile-fail tests prevent hooks from constructing invalid private runtime
  state
- docs list at least five common Tokio ecosystem cases and put each in native,
  bridge, or unsupported
- one fake bridge test proves caller timeout/cancel does not pretend external
  work stopped unless the bridge owns real cancellation

## Hostile Review Notes

- Do not build a plugin system. Rust crates are the plugin system.
- Do not make a generic async bridge unless it proves bounded queueing,
  cancellation, pressure, and replay/unsupported truth.
- Do not expose internals just because a smoke crate wants an easy path.
- Do not let custom codecs own sockets. Tina owns sockets.
- Do not let custom policies hide retries or waiters.
