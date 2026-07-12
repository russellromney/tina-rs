# Extension Hooks

This page is for someone growing the Tina ecosystem from *outside* the
workspace: a third-party crate that adds a codec, a service policy, a custom
capacity surface, an event sink, or a bridge — without forking Tina and without
private runtime access.

The rule:

> Rust crates are Tina's plugin system. Hooks are public traits and owned data.
> An extension may observe and report; it may not reach into private runtime
> state or weaken bounded/DST truth.

There is no dynamic plugin ABI and no generic `Future`/`Stream` bridge. If you
want to know where an async dependency belongs, see
[26-async-boundary.md](26-async-boundary.md). For the layering this page builds
on, see [23-core-and-batteries.md](23-core-and-batteries.md); for the per-battery
checklist, [24-battery-authoring.md](24-battery-authoring.md); for the deep
bridge copy path, [30-bridge-author-kit.md](30-bridge-author-kit.md).

## Where your code belongs

Five homes, depending on what you are adding:

1. **`tina` (core).** Only Anthropic-of-Tina-core changes land here — the trait
   vocabulary. You do not add to core to ship an extension.
2. **`tina-runtime` / `tina-sim`.** The runtime rails and the simulator. You do
   not add private rails here; you plug into the public hooks they already
   expose.
3. **Official batteries** (`tina-http`, `tina-codec`, …). Blessed first-party
   crates. New official protocols/codecs may land here, under the
   [battery-authoring](24-battery-authoring.md) rules.
4. **Bridge crates** (`tina-*-bridge`). Adapters to the existing async
   ecosystem, bounded at the bridge edge.
5. **Your own crate** (third-party). Depends on published `tina` + `tina-runtime`
   (+ `tina-codec`, `tina-sim` as needed). This is where most extensions live.
   The workspace-excluded crates under `examples/extensions/` are the worked
   examples.

The boundary between 4/5 and 1/2/3 is the whole point: an extension uses public
hooks, never private internals.

## The hooks

Each hook is small and stable. Prefer the existing name; Tina did not invent a
new vocabulary for an old concept.

### Capacity surface — `CapacitySurfaceReport` + `CapacitySummary::push`

The capacity hook is **data, not a trait**. Your extension owns some bounded
structure; render it as a `tina::capacity::CapacitySurfaceReport` with the
public `count(..)` / `weighted(..)` constructor — the same one every runtime
surface uses — and `push` it into a `tina_runtime::CapacitySummary`. It then
appears in discovery, `surface(name)` lookups, and `any_full()` exactly like a
runtime surface.

There is deliberately **no `CapacitySurface` trait**. Owned reports are enough;
`examples/extensions/tina-extension-capacity-surface` is the proof.

### Bounded event sink — `BoundedEventSink`

`tina_runtime::BoundedEventSink<T>` is the bounded sink for logs/metrics/events.
It never blocks and chooses one overflow policy: `DropPolicy::DropOldest` or
`DropPolicy::DropNewest` (the reject-new shape). It reports `len`, `high_water`,
`dropped`, `accepted`, and renders a `CapacitySurfaceReport` via
`surface_report(mode)`. Use it instead of growing the first unbounded queue in an
otherwise bounded service. There is no third drop synonym.

### Sync codec — `tina_codec::SyncCodec`

`SyncCodec` is the open codec seam:

```rust
fn feed(&mut self, bytes: &[u8]) -> usize;
fn next_frame(&mut self) -> FrameDecision<Self::Frame, Self::Malformed>;
```

The built-in `Framer` trait is **sealed** (only `LineFramer` /
`LengthDelimitedFramer`). `SyncCodec` is how a third-party crate adds its own
codec; both built-ins also implement it, so generic code can drive either.

A custom codec must stay a good citizen:

- **No I/O.** Tina owns sockets, files, capacity, cancellation, and replay. The
  codec is plain state on the isolate. There is no async codec variant.
- **Bounded.** Return `FrameDecision::Full` before growing past the cap.
- **Replayable.** `feed` + `next_frame` are pure over the bytes seen, so a sim
  socket replays like a live one.

Proof: `examples/extensions/tina-extension-custom-codec`.

### Service policy — `tina_runtime::ServicePolicy`

`ServicePolicy` is the open admission/rate-policy seam:

```rust
fn decide(&mut self, key: &Self::Key, now: Instant) -> AdmissionDecision<Self::Permit>;
fn report(&self) -> AdmissionReport;
```

The built-in policies (`ConcurrencyLimit`, `KeyedLimit`, `RateLimit`) keep their
direct `try_admit` methods and also implement `ServicePolicy`, so generic code
can drive built-in or custom policies through one shape. `ConcurrencyLimit` uses
key `()`. `KeyedLimit` and `RateLimit` use their normal key type. Non-time-based
policies ignore `now`.

A custom policy must:

- **Return a decision; never act.** No sending, spawning, sleeping, retrying, or
  hidden queue. `Wait { delay }` is advice; the caller owns the wait.
- **Be replayable.** Pure over `(config, now, key history)`; never read the wall
  clock — take `now` from `ctx.now()` (live) or the simulator (replay).
- **Report the truth.** `report()` reflects real accumulated state.

Proof: `examples/extensions/tina-extension-service-policy`.

Tiny hook example:

```rust
use std::time::Instant;
use tina_runtime::{AdmissionDecision, AdmissionReport, ServicePolicy};

fn admit<P: ServicePolicy>(
    policy: &mut P,
    key: &P::Key,
    now: Instant,
) -> AdmissionDecision<P::Permit> {
    policy.decide(key, now)
}

fn pressure<P: ServicePolicy>(policy: &P) -> AdmissionReport {
    policy.report()
}
```

### Bridge author parts — `tina_runtime::bridge`

A bridge glues Tina to a messy outside system. Tina bounds admission and
observes worker-terminal truth; it cannot always stop the outside work. The
shared vocabulary names every part a bridge must expose:

- **install result** — your install handle, implementing `BridgeInstall`
  (it owns the bridge address, a `Closer`, and a `Metrics` handle);
- **address** — the typed address the bridge hands callers;
- **closer** — `BridgeCloser`: idempotent `close()` + `is_closed()`;
- **metrics / pressure** — `BridgePressure` (private fields; built via the
  validated `measured(..)`), rendering installed capacity, in-flight,
  high-water, and the rejection/late counters; joins the capacity summary;
- **shutdown** — `BridgeCloseMode`, `BridgeCloseAdmission`, `BridgeDrainReport`;
- **worker-terminal outcome** — `BridgeTerminal::{Reached(class), Aborted}`,
  classified with `BridgeOutcomeClass`;
- **caller-observed outcome** — `BridgeCallerWarning`. When the caller's deadline
  fires first, the bridge replies `ExternalWorkMayContinue`. It does **not**
  pretend the external work stopped unless it owns real cancellation.

Proof: `examples/extensions/tina-extension-fake-bridge`.

### Runtime capability report — `RuntimeCapabilityReport`

`RuntimeCapabilities::report()` gives a `RuntimeCapabilityReport`: one row per
runtime-owned rail, each saying — explicitly — whether it is `supported`,
`unsupported`, `simulated_only`, cancel-backed, tombstoned, or drain-backed, with
a grep-friendly `discovery_report()`. An extension uses it to discover what the
runtime it was handed can actually do, without reaching into private state. It
renames nothing: `simulated_only` is `ResourceSupport::SimulatedOnly`,
`tombstoned` is the tombstone shape, and so on.

This report is read-shaped on purpose. It helps an extension choose a clear
path — native rail, bounded bridge, or unsupported — but it does not install
fallbacks, spawn helpers, or silently switch a native rail to an external async
crate.

## What a hook may not do

- import a private runtime module (there is no `runtime_internal`);
- mint a runtime-owned token/handle/lease (permits are minted only by their
  gate; their fields are private);
- construct a private report/capability with a raw struct literal
  (`BridgePressure`, `ResourceCapability` have private fields);
- own a socket, file, or pipe (Tina owns I/O);
- hide a retry, waiter, queue, or fake cancellation;
- bypass trace, capacity, or cancel truth.

These boundaries are pinned by `examples/extensions/tina-extension-compile-fail`,
whose `compile_fail` doctests prove each forbidden shape does not compile.

Not every boundedness rule can be made a Rust type error. A custom codec or
policy can still write bad code internally, just like any Rust crate can put a
`Vec` behind a method and grow it. Tina's hard line is: the runtime-owned tokens
and private reports cannot be forged, and every extension smoke crate must prove
its own bounded path with tests and capacity reports.

## Checklist for an extension crate

1. Depends only on published `tina` / `tina-runtime` (+ `tina-codec` / `tina-sim`)
   public APIs.
2. Every queue/worker/sink has an explicit cap and a typed `Full`/`Closed`
   outcome.
3. Codecs are sync and bounded; policies return decisions only.
4. Pressure joins the normal `CapacitySummary`.
5. Caller-timeout never claims external work stopped.
6. Ships a README with a run command and a smoke test.
