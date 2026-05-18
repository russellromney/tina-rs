# Phase 115: Core / Ecosystem Reorg

## Status

- Future IDD outline.
- Runs after Phase 114, before Wave A if possible.
- One PR when executed.

## Purpose

Draw a clean line between Tina core and Tina batteries before more protocol,
I/O, pool, and ecosystem work lands.

This is architecture cleanup, not cosmetic shuffling. The goal is that a new
user can learn Tina core without also learning every official HTTP/DB/AWS/gRPC
battery.

## Core Rule

Core Tina is sacred and small:

- isolate model
- effects
- bounded mailboxes
- typed send/call/reply
- request context / reply obligation
- cancellation handles
- child lifecycle
- runtime-owned rail interface
- trace/event vocabulary
- capacity vocabulary
- deterministic simulator/replay contracts
- resource lifecycle vocabulary
- compile-time safety rails

Batteries are official ecosystem crates:

- HTTP / HTTP2 / gRPC / WebSocket
- bridge crates
- codec helpers
- service skeletons
- proof harnesses
- system specimens

Batteries may be official and blessed, but they should plug into core through
public hooks wherever possible.

## Includes

- define crate/document layers:
  - core model crates
  - runtime/simulator crates
  - official batteries
  - bridge batteries
  - test/proof/specimen support
- add an "official battery rules" doc:
  - bounded admission
  - typed outcomes
  - lifecycle/close/drain report
  - pressure/capacity report
  - replay support or honest unsupported truth
  - no hidden Tokio/runtime queues
- audit battery crates for private-runtime dependency pressure and record which
  hooks need to become public before third-party batteries can exist
- update docs navigation so "learn core" and "choose batteries" are distinct
- decide prelude tiers:
  - app/core prelude
  - runtime prelude
  - battery-specific imports
- prepare Wave A plans to use the new layering language

## Does Not Include

- no new protocol feature
- no new bridge feature
- no flow macro
- no public rename
- no new runtime semantics
- no mass crate move unless it is trivial and mechanically safe
- no private API breakage just for neat folders
- no dynamic plugin ABI
- no generic async interop bridge

## Reorg Rules

- If a battery needs private runtime magic, either:
  - promote a small public hook into core, or
  - record the battery as not yet ecosystem-shaped.
- Do not move implementation files only to make the tree pretty.
- Do not create umbrella crates without a real dependency boundary.
- Do not make a "framework blob" crate that every battery depends on.
- New public hooks need compile-time rails and at least one smoke user.

## Proof Shape

- docs explain core vs official batteries in one screen
- every official battery rule has at least one current crate/example that
  satisfies it or a named gap
- no new dependency cycle
- public docs no longer imply HTTP/DB/AWS are required to learn Tina core
- Wave A phase outlines reference the new layering

