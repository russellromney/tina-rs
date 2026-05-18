# Phase 109: Typed Config And Protocol-State Safety

## Status

- IDD implementation phase.
- Follows the first compile-time safety rails pass.
- First PR slice shipped the split event/request service rail:
  `event = ...`, `request = ...`, `ServiceMessage`, split service handles,
  `send_event`, `call_request`, positive runtime proof, and trybuild lane
  diagnostics.

## Grug Truth

LLMs write the code now.

The best error is the one they cannot write. The second best error is a compiler
message that says the actual mistake.

This is core Tina, not polish. Bounded + DST tells users what happened.
Compile-time rails stop users from writing code where the truth can disappear.

## Goal

Move broad silent runtime mistakes into compile-time structure:

- public request vs internal event split
- caller authority must be replied, rejected, or carried
- cancelable deferred work must be admitted before its effect can run
- callable vs send-only handles by default
- typed config/budget manifests
- replay-affecting config must be visible and hard to omit
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

Wrong lane fails at compile time on the default path. Escape hatches are explicit
and noisy.

Internal continuation events must be unconstructable or unsendable from outside
the service module on the copied path. Use private constructors, sealed traits,
capability handles, or macro-generated visibility. The user-facing result is
simple: outside code cannot send internal events.

First shipped slice: public events and public requests are separate types and
separate capability handles. Fully private internal continuation constructors
remain the next part of this rock.

### Rock 2: Caller Authority Obligation

Make request caller authority a must-consume type on the copied path:

- reply now
- reject now
- defer into a request context
- defer into bounded cancelable admission

The default path must make "forgot to reply" a compile error. Any escape hatch
that can abandon authority must be named, loud, traced, and absent from copied
docs/specimens.

Public authority/token types must use `#[must_use]` where Rust can help:

- caller authority
- deferred request context
- cancelable pending token
- cancel handle
- admission permit

`#[must_use]` is not the whole proof. It is the cheap early warning.

### Rock 3: Cancelable Deferred Admission Gate

Fix the token/effect footgun:

- a cancelable pending token cannot silently be dropped
- child effect is not returned for dispatch until bounded admission succeeds
- failed admission returns caller authority and child effect for deliberate reply
  or reject
- duplicate/full errors are typed
- ABA-safe ticket/key behavior remains explicit

The copied path must not let a service strand a caller by starting child work
without owning the pending token.

### Rock 4: Capability Handles By Default

Make capability handles the default copied path:

- send-only
- callable
- spawn-observed-capable
- internal-only for continuation/event messages in the new split model

Raw `Address<M, R>` remains available only where needed.

The phase must leave an escape-hatch inventory in docs:

- raw `Address`
- raw `Effect`
- raw internal-event send
- raw abandoned-authority path
- raw untyped config path

Each entry says why it exists, what it can break, and what test covers it.

### Rock 5: Typed Config And Budget Manifests

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

Env/file config still validates at runtime. Normal Rust service config makes
missing required caps hard to compile.

### Rock 6: Replay-Visible Config

Make replay-affecting config visible on the copied path:

- simulator seed
- simulator/runtime config
- mailbox/pending/pool/body/bridge caps
- capacity scopes
- deadlines/timer policy
- retry/backoff policy
- protocol limits

Saved replay cases must not depend on ambient defaults. Changing visible config
must change or invalidate the replay case deliberately.

### Rock 7: Protocol Typestate

Use private state tokens inside `tina-http` and friends for real bug zones:

- HTTP/2 stream lifecycle
- DATA then trailers ordering
- gRPC final-status ownership
- WebSocket close handshake
- body stream open/eof/closed states

Keep protocol typestate private unless user code truly needs the state token.

### Rock 8: Diagnostics

Add `#[diagnostic::on_unimplemented]` or equivalent friendly compile errors for:

- message not `Send`
- reply type mismatch
- missing callable handler
- wrong handle capability
- unconsumed caller authority
- cancelable effect requested before admission
- non-`'static` captured state

Trybuild tests must pin the good messages.

## User Proof

Add a compile-fail specimen suite:

- wrong public request handled only in internal `handle`
- internal continuation sent from outside
- callable request sent through send-only handle
- caller authority dropped without reply/reject/defer on the copied path
- cancelable deferred child effect dispatched before bounded admission
- missing required config cap in typed builder
- replay case missing visible config
- invalid WebSocket/gRPC/body state transition inside crate tests

Update two real systems to the new default authoring path and remove old
escape-hatch imports:

- `system_cache_with_fill` for request/internal/defer authority
- `system_job_queue` for cancelable deferred admission

Each README must say what the compiler now catches.

## Required Proof

- User-style compile-fail tests for wrong `handle` vs `handle_call`.
- Compile-fail tests for internal event sent from outside.
- Compile-fail tests for unconsumed caller authority on the copied path.
- Compile-fail tests for cancelable effect before bounded admission.
- Compile-fail tests for missing config caps.
- Compile-fail or constructor-fail tests for replay cases missing visible config.
- Macro-path tests prove the attribute/helper macro emits the safe model, not
  only hand-written trait impls.
- Cross-shard tests prove typed handles/authority preserve caller truth and
  rejection/cancel causes.
- Runtime tests proving public behavior did not regress.
- Protocol tests proving impossible states are now unrepresentable internally.
- Two system specimens compile using the new default safety rails.
- Docs show the default path and the escape hatch separately.

## Done Means

A cheap model can wire a service the boring way and the compiler blocks the
common wrong shapes before runtime.
