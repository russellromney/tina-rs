# Phase 115: Core / Ecosystem Reorg

## Status

- Future IDD outline.
- Runs after Phase 114 and before Wave A.
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

Official batteries are blessed Tina crates. They still plug into core through
public hooks. If a battery still needs private runtime magic, name that exact
gap and add the smallest public hook needed by a real crate.

## Includes

- add `docs/tina-user-guide/22-core-and-batteries.md` and link it from
  `docs/tina-user-guide/README.md` and `docs/README.md`
- define the crate/document layers in that page:
  - core model crates
  - runtime/simulator crates
  - official batteries
  - bridge batteries
  - test/proof/specimen support
- add an "official battery rules" section with these rules:
  - bounded admission
  - typed outcomes
  - lifecycle/close/drain report
  - pressure/capacity report
  - replay support or honest unsupported truth
  - no hidden Tokio/runtime queues
- add `docs/tina-user-guide/23-battery-authoring.md` with a short checklist
  for first-party and third-party batteries
- add a "known hook gaps" table to that authoring page with these rows:
  - HTTP/TLS rails: runtime-owned rail access is still mostly first-party
  - bridge lifecycle: install/close/drain/metrics shape exists but has no
    shared author kit for third-party bridges yet
  - body streaming/source lifecycle: works for Tina HTTP, not yet a public
    battery protocol
  - AWS/sqlx/reqwest/Tokio-owned workers: bridge pattern is copied by hand
  - replay support: batteries must say supported, unsupported, or projection
    only
- update docs navigation so "learn core" and "choose batteries" are distinct
- document these prelude tiers:
  - `tina::prelude`: app/core authoring only
  - `tina_runtime::prelude`: runtime owner, host test, system setup
  - battery preludes: domain-specific helpers only, no giant re-export of Tina
    core
- update Phase 116, 117, and 118 outlines to use the new layering language
- split the worst long files into boring modules, with public re-exports kept
  stable:
  - `tina-runtime/src/lib.rs`
  - `tina-runtime/src/call.rs` into `tina-runtime/src/call/`
  - `tina-sim/src/lib.rs`
  - `tina-sim/src/dst.rs` into `tina-sim/src/dst/`
  - `tina/src/lib.rs`
- add module maps at the top of split files so future agents know where to edit
- split these large test homes only along existing test names:
  - `tina-runtime/src/tests.rs`
  - `tina-runtime/tests/local_system.rs`
  - `tina-sim/tests/io_simulation.rs`
- update docs or comments that point at old file homes

## Does Not Include

- no new protocol feature
- no new bridge feature
- no flow macro
- no public rename
- no new runtime semantics
- no crate move
- no behavior change in the file split portion
- no private API breakage just for neat folders
- no dynamic plugin ABI
- no generic async interop bridge

## Reorg Rules

- If a battery needs private runtime magic, either:
  - promote a small public hook into core that exposes existing behavior, or
  - record the battery as not yet ecosystem-shaped.
- Public hooks added here wrap existing behavior only. No new runtime semantic
  is introduced in this phase.
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

- `tina/src/lib.rs`:
  - move address/id/generation types to `tina/src/address.rs`
  - move `Context`, `CallContext`, `RequestCall`, and `RequestContext` to
    `tina/src/context.rs`
  - move `Effect`, effect constructors, and effect helpers to
    `tina/src/effect.rs`
  - move `Isolate`, `IsolateTypes`, outbound traits, and spawn traits to
    `tina/src/isolate.rs`
  - keep macros, `prelude`, `runtime_internal`, and stable `pub use` paths
    visible from `tina/src/lib.rs`
- `tina-runtime/src/lib.rs`:
  - move registration/address-book code to `tina-runtime/src/registration.rs`
  - move effect execution/dispatch helpers to `tina-runtime/src/dispatch.rs`
  - move cross-shard/remote reply helpers to `tina-runtime/src/remote.rs`
  - move host call/blocking helpers to `tina-runtime/src/host_call.rs`
  - keep public `pub use` paths stable from `lib.rs`
- `tina-runtime/src/call.rs` into `tina-runtime/src/call/`:
  - move the file to `tina-runtime/src/call/mod.rs`
  - split common call types into `call/types.rs`
  - split runtime-owned rail builders into:
    - `call/time.rs`
    - `call/tcp.rs`
    - `call/tls.rs`
    - `call/dns.rs`
    - `call/files.rs`
    - `call/process.rs`
    - `call/signals.rs`
    - `call/persistence.rs`
  - split call authority helpers into `call/cancel.rs`,
    `call/pending.rs`, and `call/groups.rs`
- `tina-sim/src/lib.rs`:
  - move simulator state/runner to `tina-sim/src/simulator.rs`
  - move live-vs-sim resource history code to `tina-sim/src/resources.rs`
  - move simulated runtime call execution to `tina-sim/src/calls.rs`
  - move trace/event projection helpers to `tina-sim/src/projection.rs`
  - keep `dst`, config, deferred, internals, and multi-shard module paths
    stable
- `tina-sim/src/dst.rs` into `tina-sim/src/dst/`:
  - move the file to `tina-sim/src/dst/mod.rs`
  - split saved replay case helpers into `dst/replay_case.rs`
  - split seed sweep helpers into `dst/sweep.rs`
  - split shrink helpers into `dst/shrink.rs`
  - split capacity/protocol projection helpers into `dst/projection.rs`
  - split discovery/formatting helpers into `dst/discovery.rs`
- `tina-http` is not split in this phase. It is an official battery and waits
  for the battery cleanup after Wave A.

## Proof Shape

- docs explain core vs official batteries in one screen
- every official battery rule has at least one current crate/example that
  satisfies it or a named gap
- no new dependency cycle
- public docs no longer imply HTTP/DB/AWS are required to learn Tina core
- Wave A phase outlines reference the new layering
- split target files are smaller and have clear module homes:
  - `tina/src/lib.rs` under 1,200 lines
  - `tina-runtime/src/lib.rs` under 1,500 lines
  - `tina-runtime/src/call/mod.rs` under 1,200 lines
  - `tina-sim/src/lib.rs` under 1,500 lines
  - `tina-sim/src/dst/mod.rs` under 1,200 lines
- the moved test homes still have the same test names or clearer file names
- `cargo fmt --all --check`
- `cargo test -p tina --doc`
- `cargo test -p tina-runtime --tests`
- `cargo test -p tina-sim --tests`
- `cargo clippy -p tina -p tina-runtime -p tina-sim --tests -- -D warnings`
- `make verify` or the repo's normal verify command passes before merge

## Hostile Review Notes

- Do not let this become a directory beauty PR. Every moved block must make a
  future feature easier to find.
- Do not hide new ecosystem hooks behind private modules. If batteries need a
  hook, it must be public, typed, bounded, and tested by one real battery.
- Do not make the prelude bigger to make examples shorter. App authors should
  learn fewer core nouns, not import the whole kitchen.
- Do not claim third-party battery readiness if current first-party batteries
  still use private runtime cracks. Name the cracks plainly.
- Do not change behavior while splitting files. If a bug falls out, commit it
  separately with its own test.
