# Phase 120: Post-Wave-A Ergonomics

## Status

- Future implementation plan.
- Runs after phases 116-119 land.
- One PR when executed.

## Spike Facts

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

## Implementation Shape

Touch only copied paths and examples/docs around newly landed Wave A features:

- Refresh `examples/systems/mini_saas_api` into the production-shaped service
  skeleton. It must use gRPC outbound, local file/codec/IPC, admission policy,
  resource policy, and durable restore in one small service.
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

## Proof Shape

- systems still pass
- docs show one production-shaped client/server/stateful service
- solved pain moved out of current findings
- at least one common wrong setup becomes compile-fail or impossible through
  the copied path
- the refreshed skeleton has a smoke test, a load-ish test, a shutdown test, and
  one bad-config/bad-input test
- every changed snippet compiles or is marked `ignore` with a reason
- findings diff proves no stale "Eiffel" or pre-helper wording returned

## Hostile Review Notes

- Do not make this a docs-only victory lap. The service skeleton must run.
- Do not hide Tina truth behind a giant facade. Helpers can reduce ceremony, not
  remove named pressure/cancel/reply outcomes.
- Do not add another noun unless two examples prove the need.
- Do not leave stale current findings that describe already-solved pain.
