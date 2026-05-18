# Phase 118: Post-Wave-A Ergonomics

## Status

- Future IDD outline.
- Runs after phases 115-117 land.
- One PR when executed.

## Purpose

Digest protocol-client, local-I/O, codec, IPC, pool, and durable-state work into
the copied service path.

## Includes

- refresh service skeleton with:
  - outbound HTTP/2/gRPC client
  - file/codec/local IPC examples
  - mature pools
  - durable restore path
- update prelude/import tiers
- simplify repeated setup only after repetition is proven
- update systems README and findings
- cheap-model proof using the new service skeleton

## Does Not Include

- no new core capability
- no broad flow macro
- no release rename

## Proof Shape

- systems still pass
- docs show one production-shaped client/server/stateful service
- solved pain moved out of current findings
- at least one common wrong setup becomes compile-fail or impossible by copied
  path

