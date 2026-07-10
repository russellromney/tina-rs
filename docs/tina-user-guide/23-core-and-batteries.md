# Core And Batteries

This page draws a single line: which crates teach the Tina model, and which
crates are batteries you reach for after you already know the model. New users
should be able to learn Tina core without first learning HTTP, gRPC, SQL, or
AWS.

The rule:

> Core Tina is small. Batteries are blessed but optional. Bridges connect to
> the existing ecosystem without lying about pressure.

A new user should be able to read just the core docs and the first three core
crates to understand what Tina is. HTTP, databases, AWS, and bridge crates are
batteries on top of the same nouns — not prerequisites.

## The shipped layers

Tina's shipped crates fall into four dependency layers. Batteries depend on
core, never the other way. Test and R&D support is listed separately.

### 1. Core model crates

These crates define the Tina vocabulary. App authors and isolate authors live
here.

- `tina` — the trait crate. `Isolate`, `Effect`, `Address`, `Outbound`,
  `Context`, `CallContext`, `RequestContext`, `IsolateTypes`, supervision
  policy types, the `#[tina::isolate(...)]` macro, and `tina::prelude`.
- `tina-mailbox-spsc` — the bounded SPSC mailbox implementation. Proven
  FIFO, `Full`, `Closed`, no hidden overflow.
- `tina-macros` and `tina-rpc-macros` — proc-macros that expand into core
  shapes only.
- `tina-supervisor` — supervision tree mechanism.

Core crates do not depend on HTTP, TCP, files, gRPC, or any battery. A user
reading just the core docs and the core crates should be able to write a
purely in-memory isolate, simulate it, and reason about overload.

### 2. Runtime and simulator crates

These crates ship the live and deterministic substrates. Runtime owners and
host-test authors live here.

- `tina-runtime` — the live single-/multi-shard runtime, runtime-owned rails
  (time, TCP, TLS, DNS, files, persistence, process, signals), call
  authority types, bridge install/closer/metrics traits.
- `tina-sim` — the deterministic simulator/replay oracle, DST sweep, seeded
  fault streams, replay cases, projection helpers.

Runtime crates own the Tina rails that batteries plug into. A battery is not
allowed to invent its own private TCP/TLS rail; it goes through
`tina-runtime`'s public hooks.

### 3. Official batteries

Blessed Tina crates that ship the most-used protocols and adapters. App
authors choose batteries; they do not have to read battery internals to
understand Tina.

- `tina-http` — native HTTP/1.1 + HTTP/2 server/client, native gRPC h2c
  server/client modes, native WebSocket server/client sessions, HTTPS,
  keepalive client pool, body streaming.
- `tina-tracing` — opt-in tracing surface for Tina events.
- `tina-proof-harness` — assertion-backed proof helpers used by specimens.
- `tina-rpc` and `tina-rpc-tokio` — typed Tina-internal RPC types and the
  Tokio-edge transport, when a Tina service must terminate a Tokio-shaped
  client.

Official batteries are bound by the **Official battery rules** in the next
section.

### 4. Bridge batteries

Bridges live next to a Tokio-shaped ecosystem package. They keep Tina state
and pressure visible while letting Tokio speak the wire format.

- `tina-tokio-bridge` — generic Tokio adapter shape.
- `tina-tower-bridge` — `tower::Service` consumers from Tina.
- `tina-reqwest-bridge` — outbound HTTP through `reqwest`.
- `tina-sqlite-bridge`, `tina-sqlx-bridge` — database access through
  existing Rust SDKs.
- `tina-aws-bridge` — AWS S3/SQS/SNS/DynamoDB/Secrets through the AWS SDK.

Bridges plug into runtime hooks (install, closer, metrics, pressure,
classifier) just like official batteries.

### Test, proof, and specimen support

Not a runtime layer but worth naming so newcomers do not mistake it for the
core:

- `tina-proof-harness` — typed assertion helpers used by replay/sim proofs.
- `examples/specimen_*` — runnable specimens that exercise the model. These
  are not part of the API; they are evidence.

## Official battery rules

Every official battery (today: `tina-http`, every bridge crate, and any
future blessed crate) must keep these six truths visible. If a battery
cannot meet one, the gap is named — not hidden.

1. **Bounded admission.** Every inbound queue, every worker pool, every
   keepalive pool has an explicit cap. No hidden unbounded buffers.
2. **Typed outcomes.** Pressure surfaces as `Full`, `Closed`, `Timeout`, or
   a battery-specific typed error variant. Never a stringly-typed message.
3. **Lifecycle: close and drain report.** Each battery exposes a closer or
   handle that names what `close()` means (admission only) and what
   `drain(deadline)` reports (in-flight count, leased count, timeouts).
4. **Pressure and capacity report.** Each battery exposes current/
   high-water/cap counters (or names them explicitly unsupported). The
   numbers join the runtime's normal capacity summary.
5. **Replay support, or honest unsupported truth.** A battery either replays
   under `tina-sim` (named events, projection presets, saved cases) or
   names itself unsupported / projection-only so a saved replay case fails
   closed instead of silently skipping it.
6. **No hidden Tokio/runtime queues.** A battery may speak to a Tokio-shaped
   SDK through a bridge worker, but the path from caller → bounded queue →
   worker → late-result counter must be visible. No "we'll buffer that for
   you" shortcut.

If a future battery violates one of these rules, name it in the **Known
hook gaps** table on [Battery Authoring](24-battery-authoring.md) and do
not call the battery production-ready.

## Prelude tiers

Tina ships three import tiers. They are intentionally not the same prelude.

- `tina::prelude` — for app and isolate authors. Imports `Effect`,
  `Address`, `Outbound`, `Context`, `CallContext`, `RequestContext`,
  `Isolate`, `IsolateTypes`, supervision types, and the effect helpers
  (`reply`, `send`, `call`, `batch`, etc.). This is what `#[tina::isolate]`
  expects to be in scope.
- `tina_runtime::prelude` — for runtime owners, host test code, and system
  setup. Adds runtime/launcher types, rail handles, host-blocking helpers,
  bridge install/closer traits, capacity/topology report types, runtime
  events, and replay-fact registration.
- Battery preludes (e.g. `tina_http::prelude` where it exists) — battery-
  specific helpers only. A battery prelude must not re-export the entire
  Tina core; users who learn the battery should still see Tina nouns from
  `tina::prelude`.

If you are an app author and find yourself reaching for `tina_runtime::*`
inside an isolate handler, that is a smell. Handlers consume `Context` /
`CallContext` and return `Effect`. Runtime types belong in the host setup
path.

## How to learn Tina (no batteries required)

1. Read [Mental Model](01-mental-model.md), [First Isolate](02-first-isolate.md),
   [Effects And Runtime Calls](03-effects-and-runtime-calls.md).
2. Run `cargo run --locked -p tina-runtime --example hello_world`.
3. Open `tina::prelude` in your editor and read the names. That is the
   working vocabulary.
4. When you actually need HTTP, gRPC, SQL, or AWS, pick the battery, read
   its short page, and follow its closer/pressure/replay rules.

You should not have to read `tina-http` or an R&D specimen to learn what a
`Call` effect is.
