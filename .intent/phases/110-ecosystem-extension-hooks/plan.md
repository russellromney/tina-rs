# Phase 110: Ecosystem Extension Hooks

## Status

- IDD implementation phase.
- Runs after the first 103-109 slices expose which hooks are real.

## Grug Truth

Tina cannot ship every rail, protocol, bridge, metric sink, and policy.

The ecosystem needs stable seams. The seams must be Tina-shaped: bounded,
typed, traceable, cancel/close/drain aware, and simulator-honest.

## Goal

Make it easy to extend Tina without forking core:

- custom bridge crates
- custom protocol codecs
- custom runtime rails/backends where supported
- custom observability sinks
- custom capacity surfaces
- custom service layers/policies

## Non-Goals

- No dynamic plugin ABI.
- No "just implement Future" async escape hatch.
- No unbounded extension queues.
- No extension hook that bypasses trace/cancel/capacity truth.
- No stable public hook for internals that are still moving.

## Rocks

### Rock 1: Extension Boundary Map

Turn the existing implicit seams into explicit modules/docs:

- bridge install/close/drain/metrics/config validation
- sync codec adapter pattern
- capacity surface/report provider
- bounded event sink
- service layer/policy hook
- runtime rail capability report

Each boundary names what is stable, what is sealed, and what remains internal.

### Rock 2: Bridge Author Kit

Add a tiny `tina::bridge` helper module that removes repeated bridge code
without hiding worker-terminal truth:

- install result shape
- closer shape
- metrics shape
- config validation helper
- late-result vocabulary
- tracing field helpers

No base class. No framework.

### Rock 3: Codec Adapter Kit

Document and provide a small copied pattern for sync codecs:

- feed bytes
- emit messages
- request more bytes
- enforce caps
- map malformed input
- close/drain cleanly

Use HTTP/WebSocket/gRPC or a line/length-delimited specimen as proof.

### Rock 4: Observability Sink Hook

Let users plug bounded sinks for runtime/service events:

- cap
- overflow policy
- dropped count
- drain snapshot
- trace correlation where available

The default remains no hidden sink.

### Rock 5: Capacity Surface Hook

Make custom services report pressure like built-ins:

- surface name validation
- current/high-water/cap/full/released
- mode: enforced/tuning/unbounded-expiring
- discovery line
- assertion helper

Custom surfaces must be usable by 107 summaries.

### Rock 6: Extension Smoke Crates

Add small external-looking crates under examples or tests:

- custom fake bridge
- custom codec/service
- custom capacity surface
- custom event sink

They must use only public extension APIs.

## User Proof

Add `examples/systems/extension_smoke`:

- installs a custom fake bridge
- uses a custom length-delimited codec
- reports custom capacity
- drains a bounded event sink
- has a README with copied "build your own extension" steps

## Required Proof

- Extension smoke compiles without private imports.
- Full/Closed/Timeout/cancel/close/drain facts stay visible.
- Custom capacity appears in runtime/service summary.
- Custom event sink drops visibly when full.
- Simulator either supports the extension path or reports unsupported honestly.
- Docs show "extension author path" and "normal app user path" separately.

## Done Means

A third-party crate can add one boring Tina-shaped capability without touching
core and without weakening boundedness, trace, cancellation, or simulator truth.
