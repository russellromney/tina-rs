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

This phase also owns the first no-behavior file split. Several core files are
too large for humans and agents to edit safely. Split them along real module
boundaries while preserving public API and behavior.

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
- split the worst long files into boring modules, with public re-exports kept
  stable:
  - `tina-runtime/src/lib.rs`
  - `tina-runtime/src/call.rs`
  - `tina-sim/src/lib.rs`
  - `tina-sim/src/dst.rs`
  - `tina/src/lib.rs`
- add module maps at the top of split files so future agents know where to edit
- move large test files only when there is an obvious test module boundary:
  request context, deferred replies, local system, I/O simulation
- update docs or comments that point at old file homes

## Does Not Include

- no new protocol feature
- no new bridge feature
- no flow macro
- no public rename
- no new runtime semantics
- no broad crate move
- no behavior change in the file split portion
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

## File Split Rules

- Split by concepts already visible in the code, not by arbitrary line count.
- Keep public paths stable with `pub use` where needed.
- Keep private helpers private.
- Preserve git history enough for review: move blocks, then do tiny fixups.
- Do not combine refactor and semantic fixes.
- If a semantic bug is found, fix it in a separate commit after the pure split.

Preferred first module targets:

- `tina`: address, context/request authority, effect/isolate traits, macros,
  time/deadline, capacity, pool/call helpers
- `tina-runtime`: dispatch core, threaded runtime, call builders by rail
  family, observation, lifecycle/service reports, bridge vocabulary, driver
  rails
- `tina-sim`: simulator runner, resource histories, DST helpers, replay case
  helpers, fault config/checkers
- `tina-http`: do not split broadly here unless Wave A needs it; HTTP is a
  battery and can wait for the battery phase

## Proof Shape

- docs explain core vs official batteries in one screen
- every official battery rule has at least one current crate/example that
  satisfies it or a named gap
- no new dependency cycle
- public docs no longer imply HTTP/DB/AWS are required to learn Tina core
- Wave A phase outlines reference the new layering
- large core files are smaller and have clear module homes
- `cargo fmt --all --check`
- targeted tests for moved modules still pass
- `make verify` or the repo's normal verify command passes before merge
