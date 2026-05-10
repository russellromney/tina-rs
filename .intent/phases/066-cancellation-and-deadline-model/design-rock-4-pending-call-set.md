# Rock 4 design note — bounded `PendingCallSet`

## Status

Design only. Not shipped this phase.

## Why deferred

A bounded helper that owns a fixed-capacity table of `CallHandle`s
keyed by user-chosen `RequestId` would let isolates write the
"cancel-all-my-pending-calls" pattern without rolling their own
storage. The specimen cancellation chain example shows the cost of *not*
having it: the driver stores `Vec<CallHandle<WorkerReply>>` and drains
it on cancel.

The plan's hard rules for this helper are:

- bounded storage (fixed-capacity slab/table), not `HashMap`;
- duplicate key is typed error;
- full table is typed error;
- removal on completion is *explicit* — no `Drop` impl that auto-removes;
- cancel-all returns one effect per stored handle, surfacing the
  per-call `CancelOutcome` truth;
- helper does not own the workflow.

A correct implementation needs a small per-isolate slab that stays
alive across handler turns and survives generation bumps. That requires
either:

1. A new isolate-state primitive in `tina` (a real bounded slab type), or
2. A `tina_runtime`-side helper that wraps `Vec` plus an explicit
   capacity check.

Option 2 is what the specimen currently does inline; promoting
it to a helper would shave ~10 lines per cancel-aware isolate but adds
an API to maintain. Option 1 is the load-bearing primitive but bumps
phase 066 from "first-form cancel" to "bounded-cancel-set + first-form
cancel," which dilutes the rock.

## Decision

Ship the first-form primitive (Rocks 2 + 3) plus the specimen update
(Rock 6) without the helper. Promote the helper in phase 067 alongside
the bounded pool work — pools and pending-call sets share the same
"bounded handle table with explicit cleanup" shape, and 067 will pin
the slab primitive once for both.

## What an honest helper looks like

```rust
let mut calls = PendingCallSet::with_capacity(64);
calls.insert(request_id, handle)?;        // typed Full / DuplicateKey
let handle = calls.remove(&request_id);   // None if completed/cancelled
let effects = calls.cancel_all_for::<I>(); // Effect::Batch
```

`cancel_all_for` would have to thread the per-call translator
(`fn(CancelOutcome) -> M`) so each cancel still produces an ordinary
later message. The cleanest shape is to require the user supply the
translator at `insert` time — but that complicates the storage cell.
Worth a separate phase.

## Hard rules carried forward

- bounded storage;
- explicit cleanup on completion / timeout / cancel;
- no hidden retry, no hidden queue;
- cancel-all returns visible effects, not silent reclamation;
- helper does not own the workflow.
