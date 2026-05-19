# Phase 120: Post-Wave-A Ergonomics

## Status

- Future implementation plan.
- Runs after phases 116-119 land.
- Runs before Wave B if Wave A changes the copied service shape enough that
  fairness/load tests would otherwise copy stale patterns.
- One PR when executed.

## Starting Facts

- `examples/FINDINGS.md` is useful but noisy. After big waves it must separate
  current pain from solved pain.
- Systems already surface real copied-path rough spots: pending replies,
  request context, admission vocabulary, recurring ticks, shutdown, capacity
  summaries.
- Phase 110 covers workflow pending helpers. This phase should not rebuild
  those. It digests whatever actually landed in 116-119.
- The user story is not "prettier docs." It is "a cheap model can copy one
  production-shaped service and wire the right helpers."

## Purpose

Digest protocol-client, local-I/O, codec, IPC, pool, and durable-state work into
the copied service path.

## Includes

- refresh service skeleton with:
  - outbound HTTP/2/gRPC client
  - file/codec/local IPC examples
  - mature pools
  - durable restore path
  - admission/rate policy copied path
- one "whole service" specimen that uses the new Wave A primitives together
- update prelude/import tiers
- simplify repeated setup only after repetition is proven
- replace copied snippets that still teach old/raw paths
- update systems README and findings
- cheap-model proof using the new service skeleton

## Does Not Include

- no new core capability
- no broad flow macro
- no release rename
- no semantic changes to protocol/pool/persistence primitives

## Blast Radius

Small-to-medium blast radius.

- Allowed: docs, examples, specimens, prelude/import guidance, copied snippets,
  findings cleanup, tiny wrappers only when two copied examples prove the need.
- Not allowed: core runtime semantics, protocol behavior, resource policy
  behavior, durability semantics, or new major nouns.
- If a helper needs behavior changes in a core crate, stop and make a separate
  implementation phase.

## Implementation Shape

Touch only copied paths and examples/docs around newly landed Wave A features:

- Refresh `examples/systems/mini_saas_api` into the production-shaped service
  skeleton. It must use gRPC outbound, local file/codec/IPC, admission policy,
  resource policy, and durable restore in one small service.
- If `mini_saas_api` becomes too dense, split one copied service into:
  - `system_production_edge_service`
  - `system_local_data_service`
  Both must run; do not leave one as prose.
- Add a short "which noun do I use?" guide for the new primitives. Keep it
  grouped by task, not by type list:
  - "limit work"
  - "retry after Full"
  - "call a protocol client"
  - "stream local bytes"
  - "own durable state"
  - "shut down"
- Update prelude/import docs only where the copied path proves repetition.
- Move solved findings from `examples/FINDINGS.md` to history or mark closed
  with phase numbers.
- Update `examples/systems/README.md` so each completed system has a smoke
  command and names which Wave A primitive it exercises.
- Add cheap-model instructions: build one tiny feature using only the skeleton
  README, then record any new rough edge in findings.
- Keep names task-shaped:
  - "call another service"
  - "limit work"
  - "read local bytes"
  - "recover state"
  - "shut down"
  Avoid type-index docs as the first learning path.

## Proof Shape

- systems still pass
- every edited specimen/system README command runs
- docs show one production-shaped client/server/stateful service
- solved pain moved out of current findings
- at least one common wrong setup becomes compile-fail or impossible through
  the copied path
- the refreshed skeleton has a smoke test, a load-ish test, a shutdown test, and
  one bad-config/bad-input test
- the skeleton includes one overload path and one recovery/shutdown path,
  because those are where copied examples usually lie
- the skeleton proves at least one compile-time guardrail from recent phases by
  linking to or adding a trybuild case for the copied mistake
- every changed snippet compiles or is marked `ignore` with a reason
- findings diff proves no stale "Eiffel" or pre-helper wording returned

## Hostile Review Notes

- Do not make this a docs-only victory lap. The service skeleton must run.
- Do not hide Tina truth behind a giant facade. Helpers can reduce ceremony, not
  remove named pressure/cancel/reply outcomes.
- Do not add new major nouns in this phase. This phase teaches and composes the
  nouns created by Wave A.
- Do not leave stale current findings that describe already-solved pain.
