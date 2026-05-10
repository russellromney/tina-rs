# Rock 7 Design Note — External Cancellation

## Status

Design only. Domain `Stop` stays the blessed pattern.

## Three Different Cancels

The word "cancel" hides three things. Naming them prevents the
helper from blurring them.

1. **Stop an isolate.** Domain `Stop` message → handler returns
   `stop()`. Pending calls the isolate owned close. Late
   replies land as `CallReplyRejected { RequesterClosed }`.
2. **Cancel one caller-owned pending call.** The requester
   wants this one call dropped without stopping itself. No
   public API today.
3. **Cancel all calls owned by an isolate without stopping
   it.** Composes from form 2.

## Candidate APIs

```rust
runtime.cancel_isolate(addr) -> CancelOutcome
runtime.cancel_call(handle)  -> CancelOutcome
ctx.call_with_handle(addr, msg, timeout) -> (Effect<Self>, CallHandle)
```

## Hard Rules

- Worker work already accepted may still finish.
- Late replies become typed rejected facts in the trace.
- Cancel reclaims pending capacity.
- A cancel against a driver-backed call only cancels if the
  driver cancels. Today none do; the helper rejects rather
  than lying.
- Trace must distinguish caller timeout, explicit cancel,
  isolate stop, resource close, and cross-shard requester
  closed. Five facts, five events.
- Simulator parity required before shipping runtime behavior.

## Why Form 1 Is Not An API Yet

Domain `Stop` already does the job. A `cancel_isolate(addr)`
helper would be one line of host code replacing one domain
`Stop` send. Not a clear win in a typed-message system where
named lifecycle messages are already the shape.

The form earns its keep once forms 2 or 3 ship — at that point
one vocabulary covers all three shapes.

## Why Forms 2 And 3 Are Deferred

Form 2 needs:

- public `CallHandle` value;
- runtime maps handle → pending call internally;
- typed `CallReplyRejected { ExplicitCancel }`;
- handle-after-requester-stops semantics.

Form 3 = form 2 applied to all calls. Cheap once form 2 ships.
"Stop the isolate" already covers "drop everything I owned"
through the lifecycle path.

Sequencing: design + ship form 2 in one phase. Form 1 and
form 3 fall out as small wrappers. Not bootstrap-and-fanout
work; the runtime event vocabulary work is non-trivial.

## Decision

`specimen_cancellation_chain`'s `DriverMsg::Stop` stays. No
helper this phase.

This note locks in:

- the three forms must stay distinct;
- a helper that blurs them is not honest;
- `cancel_call` is the load-bearing primitive — ship it before
  the wrappers.
