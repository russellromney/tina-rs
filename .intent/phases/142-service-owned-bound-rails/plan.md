# Phase 142: Service-Owned Bound Rails

## Goal

Make request-sized work harder to write accidentally.

```text
request may be big
service bound is explicit
helpers take the bounded wrapper
capacity assertions show the bound held
```

This phase is not a linter. It adds default Tina paths that require a service
owned cap before producing many effects.

## Build

1. Add bounded collection wrappers where users currently reach for raw `Vec`.
   - `BoundedItems<T>`: built from iterator + `max_items`.
   - `BoundedEffects<I>`: built from iterator + `max_effects`, then converted
     to `batch(...)`.
   - Both return typed `TooManyItems` / `TooManyEffects` with observed count.
   - Zero cap is rejected.

2. Wire helpers into docs and examples.
   - `BroadcastTargets` from Phase 141 uses the same naming style.
   - Update at least two specimens/systems that currently do
     `iter.map(...).collect::<Vec<_>>()` for request-sized work.
   - Keep simple fixed small batches alone where the bound is obvious in code.

3. Add capacity assertion helpers for copied specimens.
   - A tiny `assert_service_owned_bound(name, configured, observed)` helper in
     the proof harness or runtime test utilities.
   - It reports `Exceeded`, `MissingObservation`, and `UnboundedDeclared`.
   - Use it in the migrated specimens.

4. Docs.
   - Boundedness guide gets a "request-sized loop" box.
   - Show bad shape and good shape:
     `for item in request.items { spawn/call/send }` -> `BoundedItems`.

## Must Not

- Do not ban normal Rust loops.
- Do not pretend compile-time can know runtime request length.
- Do not wrap every `Vec` in the codebase. Only copied service/fanout paths.
- Do not make bounds magic defaults. The caller passes the cap.

## Proof

- Unit tests for cap, exact-at-cap, over-cap, zero-cap, order preservation.
- Compile-fail/doc test proving helpers consume `BoundedEffects`, not raw `Vec`,
  where applicable.
- Specimen tests prove observed high-water/full counts match the configured cap.
- Docs compile where snippets are real Rust.

