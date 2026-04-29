# 009 Mariner RestartChildren Execution Plan

Session:

- A

## Goal

Make `CurrentRuntime` execute `Effect::RestartChildren` for the current
isolate's direct children, using the restartable child records created in slice
008. This is the first real restart behavior, but it is still narrower than a
supervisor: no policies, no budgets, no `tina-supervisor`, and no recursive
restart tree.

## Context

Slice 006 made direct parent-child lineage runtime-owned. Slice 007 made
addresses generation-aware. Slice 008 added restartable child records and stored
private restart recipes. Right now `RestartChildren` is still observed-only.

This slice turns that effect into runtime behavior:

- direct restartable children are replaced
- direct non-restartable children are skipped visibly
- old child addresses become stale/closed
- replacement child records keep the same per-parent ordinal

## Mapping from spec diff to implementation

- Add restart-specific `RuntimeEventKind` variants.
- Change the `ErasedEffect::RestartChildren` dispatcher arm from
  `EffectObserved` to restart execution.
- Find child records where `record.parent == isolate_id`.
- Visit matching records in existing child-record order, which is already
  deterministic per-parent child-ordinal order.
- For each record:
  - emit a restart-attempt event
  - if there is no restart recipe, emit a skipped event and continue
  - if there is a restart recipe, stop the old child if still running
  - call the stored recipe to create a fresh child incarnation
  - update the child record to the fresh address identity while preserving
    parent and child ordinal
  - emit a restarted event
- Keep `RestartPolicy` and `RestartBudget` unused in this slice.

## Phase decisions

- `RestartChildren` means "restart this isolate's direct restartable children"
  until a later supervisor-policy slice adds narrower selection.
- Non-restartable direct children are skipped with trace. They are not silently
  ignored, and they do not panic the runtime.
- Child ordinals are stable across restart. The replacement occupies the same
  child-record slot and ordinal.
- Replacement children get fresh monotonic isolate ids and initial
  `AddressGeneration`.
- Old child entries remain in the runtime registry as stopped historical
  incarnations. No id reuse, no compaction.
- Old child addresses return `Closed`; new child addresses are visible only via
  crate-private test snapshots in this slice.
- Restart is non-recursive: grandchildren are unaffected unless their direct
  parent later returns `RestartChildren`.
- Restart factory panics propagate as runtime construction errors. They are not
  caught as `HandlerPanicked`, because no handler is running.
- If the old child is running, restart uses the existing stop-and-abandon path.
  If it is already stopped or panicked, restart does not emit duplicate
  `IsolateStopped`.
- Restarted children are registered during the current round but only run on a
  later `step`, preserving the existing spawn scheduling rule.
- `RestartChildren` on a parent with no direct children emits no restart events
  beyond the normal handler-finished event.
- Restart walks all current child records for the parent, including children
  created in an earlier step whose first handler step has not run yet.
- Runtime registry growth scales with initial spawn count plus restart
  replacement count in this no-id-reuse model.
- Restart makes the trace's tree shape explicit. The runtime trace is a
  deterministically ordered causal tree: each event has at most one cause, and
  one event may cause multiple direct consequences.

## Trace vocabulary

Add three event kinds:

```rust
RestartChildAttempted {
    child_ordinal: usize,
    old_isolate: IsolateId,
    old_generation: AddressGeneration,
}

RestartChildSkipped {
    child_ordinal: usize,
    old_isolate: IsolateId,
    old_generation: AddressGeneration,
    reason: RestartSkippedReason,
}

RestartChildCompleted {
    child_ordinal: usize,
    old_isolate: IsolateId,
    old_generation: AddressGeneration,
    new_isolate: IsolateId,
    new_generation: AddressGeneration,
}
```

Add `RestartSkippedReason::NotRestartable`.

Cause chain:

- `HandlerFinished { effect: RestartChildren }` causes each
  `RestartChildAttempted`.
- `RestartChildAttempted` causes either `RestartChildSkipped` or
  `RestartChildCompleted`.
- If the old child is stopped by restart, `RestartChildAttempted` causes
  `IsolateStopped`, and `IsolateStopped` causes any `MessageAbandoned` events.
- In the running-old-child case, `RestartChildAttempted` also causes
  `RestartChildCompleted`. Replay and property tests must allow multiple direct
  children for one cause event.

## Proposed implementation approach

1. Extend trace types:
   - add `RestartSkippedReason`
   - add the three restart event variants
2. Add private helpers on `CurrentRuntime`:
   - find entry index by address identity
   - stop old child if still running
   - execute restart for one child-record index
   - execute restart for all direct children of a parent
3. Adjust child-record storage enough to update a record after replacement.
   The existing `ChildRecord` is mutated in place; its `Vec` index, parent, and
   child ordinal are preserved.
4. Change the `ErasedEffect::RestartChildren` arm to call restart execution
   instead of emitting `EffectObserved`.
5. Extend crate-private child record snapshots to expose current replacement
   address identity for tests.
6. Update generated-history property tests so restart attempts have visible
   outcomes once restart events exist.

Restart-recipe ownership: store restart recipes as
`Rc<dyn ErasedRestartRecipe<S, F>>`, call them through `&self`, and keep the
same private restart recipe handle alive across restarts. `Rc` is sufficient
for this current-thread runtime; cross-shard work will need to revisit `Arc` or
an equivalent cross-thread recipe handle. The recipe is the source of fresh
child state and must not be consumed by the first restart.

## Acceptance

- A parent with one restartable child returns `RestartChildren`; the trace shows
  attempt, old child stop if needed, optional abandonment, and completed
  restart.
- The child record keeps the same parent and child ordinal but points to a new
  isolate id with initial generation.
- Sending to the old child address returns `Closed`.
- Sending to the replacement address from the crate-private snapshot succeeds
  and the replacement handles the message on a later step.
- Restarting an already stopped or panicked child creates a replacement without
  duplicate `IsolateStopped`.
- Multiple restartable direct children restart in child-ordinal order.
- Non-restartable direct children emit `RestartChildSkipped` with
  `NotRestartable` and do not panic.
- Parents with no direct children emit no restart event subtree.
- Current child records whose children have not handled a message yet are still
  included in the restart walk.
- Grandchildren are not restarted by a grandparent's `RestartChildren`.
- Restarted children do not run until a later `step`.
- Repeated identical runs produce identical trace, lineage snapshot, and
  child-record snapshot.
- `make test`, `make verify`, and `make miri` pass.

## Tests and evidence

- Unit tests in `tina-runtime-current/src/lib.rs` for private child-record
  updates, replacement addresses, trace shape, and edge cases. This follows the
  slice 006/008 pattern because the load-bearing evidence depends on private
  child-record snapshots.
- Extend `runtime_properties.rs` so restart attempts have visible outcomes and
  causal links stay well formed over generated histories.
- Existing tests for stop-and-abandon, panic capture, spawn scheduling, address
  liveness, and local send dispatch must remain green.

## Traps

- Do not add `tina-supervisor`.
- Do not use `RestartPolicy` or `RestartBudget` yet.
- Do not restart grandchildren recursively.
- Do not silently skip non-restartable children.
- Do not panic for non-restartable children.
- Do not allocate a new child ordinal on restart.
- Do not reuse isolate ids.
- Do not expose public child-record or fresh-address lookup APIs.
- Do not make stale old addresses route to replacement children.
- Do not run replacement children in the same round they are created.
- Do not catch restart factory panics as handler panics.
- Do not reconstruct parent-child state from trace events.
- Do not assume trace cause graphs are linear.
- Do not model slice 009 traces as DAGs with multi-cause events; every event
  still has at most one cause.

## Files likely to change

- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/runtime_properties.rs`

## Areas that should not be touched

- `tina` public API
- `tina-mailbox-spsc`
- `tina-supervisor`
- Tokio/current-thread driver work
- simulator/Voyager code
- README/ROADMAP unless review finds a real roadmap contradiction
