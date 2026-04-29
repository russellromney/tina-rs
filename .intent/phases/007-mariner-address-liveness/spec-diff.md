# 007 Mariner Address Liveness

## In plain English

Supervision is about replacing failed work without pretending the old worker is
still alive.

Right now, a typed `Address<M>` only contains a shard id and an isolate id. That
is enough for local send/spawn tests, but restart semantics need one more piece
of identity: an old address must not silently start targeting a replacement
isolate after a restart.

This slice defines the Rust liveness model before restart execution lands:
`Address<M>` names one physical isolate incarnation, not a logical role such as
"the current worker." It does that by adding an address generation token. The
full address identity becomes shard id + isolate id + generation.

If a child stops, panics, or is later replaced, its old address continues to
name the old incarnation and fails visibly with `Closed`. The replacement gets a
fresh address identity. This gives `tina-rs` Tina-Odin's important stale-handle
property without committing the runtime to "never compact or reuse ids forever."

## What changes

- `tina-rs` explicitly defines `Address<M>` as an address to one isolate
  incarnation, not as a stable logical service name.
- Add a generation token to the address model. An address is identified by
  shard id, isolate id, and generation.
- `Address::new(shard, isolate)` constructs an initial-generation address.
  Non-initial or runtime-issued generations use an explicit generation-aware
  constructor.
- Runtime-issued addresses carry the generation assigned to that isolate
  incarnation.
- Runtime lookup matches both isolate id and generation. A known isolate id with
  the wrong generation is stale, not a match for the current incarnation.
- `CurrentRuntime` may continue assigning fresh isolate ids monotonically in
  this slice, but correctness no longer depends on "ids are never reused
  forever." Future compaction or id reuse must preserve generation-based stale
  address rejection.
- `u64` isolate-id or generation wraparound is out of scope for this slice. The
  practical bound is the runtime lifetime, not the numeric type boundary.
- Stale addresses to stopped incarnations fail safely:
  - runtime ingress returns the underlying mailbox `Closed` error
  - dispatched `Send` records `SendRejected { reason: Closed }`
- Stale addresses with a known isolate id and wrong generation also fail safely
  as `Closed`; they do not target the current generation.
- Unknown isolate ids remain programmer errors. A synthetic address that never
  came from runtime registration or spawn still panics instead of becoming a
  normal stale-address outcome. Unknown-isolate panics propagate and do not
  record `SendRejected`. Runtime ingress to an unknown isolate records no trace
  event; dispatched sends may still retain the preceding handler and
  `SendDispatchAttempted` events before the programmer-error panic propagates.
- The trace/proof vocabulary distinguishes "stale but known and closed" from
  "unknown programmer error" using existing outcomes rather than a new trace
  event.
- Runtime send trace events include enough target identity to distinguish
  generations. Once generation is part of address identity, trace events that
  describe send targets must not drop it.
- Future restart slices must mint a fresh address identity through the existing
  registration or spawn paths. Reusing an isolate id is allowed only if the
  replacement uses a fresh generation and stale old-generation addresses remain
  closed.
- Logical naming is user-space. Applications that want "the current worker for
  tenant 7" should build a registry isolate that maps logical names to current
  incarnation addresses.

## What does not change

- This slice does not add compaction or id reuse.
- This slice does not require a replacement isolate to reuse the old isolate id.
  Fresh isolate ids are still acceptable.
- This slice does not execute `RestartChildren`.
- This slice does not add restartable child records.
- This slice does not add `tina-supervisor`.
- This slice does not add logical service names, aliases, registries, or
  address refresh APIs.
- This slice does not make unknown targets recoverable runtime events.
- This slice does not change cross-shard behavior; cross-shard sends remain out
  of scope.
- This slice does not change stop-and-abandon or panic-capture behavior except
  to document and prove how old addresses behave afterward.

## How we will verify it

- A stopped isolate's old address fails with `TrySendError::Closed` through
  runtime ingress.
- A dispatched send to a stopped isolate records `SendRejected` with
  `SendRejectedReason::Closed`, not `SendAccepted`.
- A registered isolate's address exposes the generation assigned by the runtime.
- Runtime ingress to a known isolate id with the wrong generation fails with
  `TrySendError::Closed`, not delivery to the current generation.
- A dispatched send to a known isolate id with the wrong generation records
  `SendRejected { reason: Closed }`, not `SendAccepted`.
- Runtime send trace events include the target generation.
- A synthetic address with an isolate id that was never registered still panics
  as a programmer error.
- Repeated identical runs produce the same ids, outcomes, and trace events.

## Autonomy note

This slice chooses the address-liveness model for later supervision work, so it
is an escalation-class semantic decision. After Session B reviews and the human
accepts this liveness model, the later restart-record and `RestartChildren`
slices can run in autonomous bucket mode unless a new escalation gate trips.
