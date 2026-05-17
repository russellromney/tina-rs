# Phase 109: Typed Config And Protocol-State Safety

## Status

- IDD implementation phase.
- Not started.
- Follows the first compile-time safety rails pass.

## Grug Truth

LLMs write the code now.

The best error is the one they cannot write. The second best error is a compiler
message that says the actual mistake.

## Goal

Move broad silent runtime mistakes into compile-time structure where it pays:

- public request vs internal event split
- callable vs send-only handles by default
- typed config/budget manifests
- private protocol typestate for bug-prone state machines
- better diagnostics on public traits/macros

## Non-Goals

- No type puzzle for ordinary apps.
- No typestate for every struct.
- No removing runtime `Full`/`Closed`/`Timeout`.
- No hiding dynamic env/file config validation.

## Rocks

### Rock 1: Public Request / Internal Event Split

Make the common authoring model separate:

- external callable requests
- fire-and-forget public events
- internal continuation events

Wrong lane should fail at compile time or require an explicit escape hatch.
`handle_call`/`handle` confusion should stop being easy to write.

### Rock 2: Capability Handles By Default

Prefer handles that encode capability:

- send-only
- callable
- spawn-observed-capable
- internal-only for continuation/event messages in the new split model

Raw `Address<M, R>` remains available only where needed.

### Rock 3: Typed Config And Budget Manifests

Build typed builders for copied service knobs:

- mailbox caps
- pending caps
- pool caps
- body caps
- bridge in-flight caps
- deadlines
- retry budgets
- shared capacity scopes
- startup config

Env/file config still validates at runtime, but normal Rust service config
should make missing required caps hard to compile.

### Rock 4: Protocol Typestate

Use private state tokens inside `tina-http` and friends for real bug zones:

- HTTP/2 stream lifecycle
- DATA then trailers ordering
- gRPC final-status ownership
- WebSocket close handshake
- body stream open/eof/closed states

Do not expose fancy typestate unless users need it. Internal correctness first.

### Rock 5: Diagnostics

Add `#[diagnostic::on_unimplemented]` or equivalent friendly compile errors for:

- message not `Send`
- reply type mismatch
- missing callable handler
- wrong handle capability
- non-`'static` captured state

Trybuild tests must pin the good messages.

## Required Proof

- User-style compile-fail tests for wrong `handle` vs `handle_call`.
- Compile-fail tests for internal event sent from outside.
- Compile-fail tests for missing config caps.
- Runtime tests proving public behavior did not regress.
- Protocol tests proving impossible states are now unrepresentable internally.
- Docs show the default path and the escape hatch separately.

## Done Means

A cheap model can wire a service the boring way and the compiler blocks the
common wrong shapes before runtime.
