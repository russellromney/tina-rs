# Rock 2 Design Note — Initial Child-Spawn Observation

## Status

Design only. Not shipped in 064.

## Goal

Remove the host-side `Arc<Mutex<Option<Address<ChildMsg>>>>` slot and
the `WorkerMsg::Boot { ctx.me() }` variant from
`specimen_supervised_worker`.

Candidate API:

```rust
let waiter = runtime.observe_child_started::<WorkerMsg>(parent);
runtime.try_send(parent, ParentMsg::Spawn)?;
let child: Address<WorkerMsg> = waiter.wait(timeout)?;
```

`observe_child_restarted(parent)` already exists and is the
proven shape. `observe_child_started` would be the same registry
pattern, scoped to the *first* spawn after registration.

## What The Runtime Already Knows

- `RuntimeEventKind::Spawned { child_isolate }` fires when a spawn
  effect creates a child. Carries `IsolateId`, not the child's
  typed `Message`.
- The parent's shard is known (children always land on the parent
  shard in the first form), so `Address::new(parent.shard(),
  child_isolate)` reconstructs the address — *but only as
  `Address<M>` for some `M` chosen by the caller*. The runtime
  has no way to type-check that `M` matches the spawned child's
  `Isolate::Message`.

## What's Missing For An Honest Helper

To make `observe_child_started::<M>(parent)` type-honest, one of
the following must hold:

1. **`Spawned` carries a `TypeId` for the child's `Message`.** The
   runtime would have to thread `TypeId::of::<I::Message>()` down
   through `Effect::Spawn(I::Spawn)` and the
   `IntoErasedSpawn`-handler's spawn closure. The waiter keyed
   on `parent_id` checks the incoming `TypeId` against its own
   `TypeId::of::<M>()`. Type mismatch → `WaitError::TypeMismatch`,
   matching `observe_result::<T>`'s shape.

2. **The Parent's `Spawn` associated type bounds `M`.** A
   constraint of the form
   `where P::Spawn = RestartableChildDefinition<C>, C: Isolate<Message = M>`
   on the helper. This compiles but is brittle for parents that
   spawn more than one child kind (a parent that spawns
   `Worker` and `Logger` cannot be expressed here without
   widening the constraint).

3. **Caller-asserted `M`.** Helper takes `M` as turbofish and the
   waiter resolves to `Address<M>` without a type check. Equivalent
   to `Address::new(parent.shard(), child_isolate)` with extra
   ceremony. Not honest under the LLM rule — a reader sees
   `observe_child_started::<WorkerMsg>` and assumes the runtime
   verified the type.

Option 1 is the only one that meets the rule "ergonomics may not
remove truth". Options 2 and 3 either over-constrain the parent
or hide a soundness gap.

## Why 064 Does Not Ship This

- Adding `TypeId` to `Spawned` ripples through:
  - `RuntimeEventKind::Spawned`;
  - `IntoErasedSpawn`;
  - the supervisor's restart-spawn path (so the same field exists
    for restarted incarnations and `ChildRestarted` could carry
    it too — symmetric, but more surface);
  - the simulator's spawn dispatch (must record the same
    `TypeId` in trace order).
- Finding 14 (`spawn(...)` surfaces the child's address) covers
  the same pain from a different direction: an
  `Effect::SpawnObserved(def, |addr| MyMsg::ChildSpawned(addr))`
  delivers the typed address back to the *parent*, not to the
  host. That keeps observation inside the isolate model rather
  than adding a host-side waiter.
- 064 is supposed to "ergonomics may remove bookkeeping" without
  changing the model. Adding a typed runtime event is a model
  change. Pick one shape — observation waiter vs.
  `SpawnObserved` continuation — in a phase that owns the
  supervisor/spawn surface, not in the bootstrap-helpers phase.

## Decision For 064

Leave the slot+`Boot` pattern in `specimen_supervised_worker`. The
example already documents it explicitly:

> Until Tina ships an observe-child-spawned waiter, this is the
> documented pattern for "host needs to know the fresh worker's
> address" (FINDINGS.md).

Do not file a new finding; finding 14 already names this exact
gap. Update finding 14's `Build:` list to record:

- the typed `Spawned { ..., child_message_type: TypeId }` event
  is the precondition for any host-side `observe_child_started`
  helper;
- the `SpawnObserved` continuation form is an alternative that
  keeps the typed address inside the isolate model, at the cost
  of a new effect variant.

Pick the form in the phase that lands the supervisor/spawn API
revisit.

## Rule Check

- "ergonomics may not remove truth" — none of the three options
  *except* Option 1 preserves type safety. 064 does not implement
  Option 1.
- "must not require the child to publish its own address through
  `Boot`" — the rejected helper would meet this; the deferred
  decision still requires `Boot` for now.
- "bounded observation registry rules apply" — the existing
  `ChildRestartedWaiter` registry shape already enforces this.
- "dropped/timed-out waiters must not leak cap" — same registry,
  same rule.
