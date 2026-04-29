# 008 Mariner Restartable Child Records Plan

Session:

- A

## Goal

Teach `CurrentRuntime` to record restartable child metadata at spawn time without
executing restart behavior yet. After this slice, the runtime should know which
spawned children are restartable, what address identity they were created with,
their parent/ordinal relationship, and how to create fresh isolate state later.

## Context

Slice 006 made direct parent-child lineage runtime-owned. Slice 007 made
address identity generation-aware. The next supervision step needs restartable
child records so `RestartChildren` can be implemented from runtime state instead
of trace reconstruction or ad hoc test setup.

This is a medium-class slice by the slice 001-007 standard. It adds one public
trait-crate type, a private restart recipe erasure path, a child-record table,
per-parent ordinal counters, and spawn metadata plumbing.

The key design issue is that existing `SpawnSpec<I>` is one-shot. It carries one
isolate value and a mailbox capacity. That is enough to spawn once, but not
enough to recreate a child predictably. This slice therefore adds a separate
restartable spawn payload backed by a factory.

## Mapping from spec diff to implementation

- Add a public `RestartableSpawnSpec<I>` type in `tina`.
- `RestartableSpawnSpec<I>` stores:
  - a factory callable that creates fresh `I`
  - mailbox capacity
- Keep `SpawnSpec<I>` unchanged as the one-shot spawn payload.
- Extend `tina-runtime-current`'s private spawn erasure so each spawn returns:
  - the new child address identity
  - optional restart recipe metadata
  - mailbox capacity
  - restartability flag
- Add a private child-record table to `CurrentRuntime`.
- Add crate-private test snapshot helpers for child records.
- Preserve existing parent-child lineage behavior.
- Do not expose child records publicly.

## Phase decisions

- Name the new public type `RestartableSpawnSpec<I>`.
- Keep restartability as a separate public payload type instead of adding a mode
  flag to `SpawnSpec<I>`. This keeps one-shot spawn simple, makes restartability
  visible at the call site, and avoids imposing a repeatable factory concept on
  children that are not restartable.
- The factory is `Fn() -> I + 'static`, not `FnOnce`, because restart may need
  to create more than one replacement over time.
- The factory is `Fn`, not `FnMut`; mutable state shared across restarts must
  use interior mutability such as `Rc<Cell<_>>` in current-thread tests or
  `Arc<Mutex<_>>` in future cross-thread contexts.
- Parameterized isolates capture their inputs in the factory closure, for
  example `RestartableSpawnSpec::new(move || Worker::new(tenant_id), 16)`.
- The factory creates the initial spawned isolate as well as future replacement
  isolates. The runtime must not clone the running child state for restart.
- `SpawnSpec<I>` remains supported and records `restartable = false`.
- `RestartableSpawnSpec<I>` records `restartable = true` and stores an erased
  restart recipe privately in the runtime.
- In slice 008, `RestartableSpawnSpec<I>` has no externally observable runtime
  behavior beyond `SpawnSpec<I>`; both spawn one child. The difference arrives
  when slice 009 consumes the private restart recipe.
- Child records store the complete slice-007 address identity: shard id, isolate
  id, and `AddressGeneration`.
- Per-parent child ordinals start at `0` and increment by direct child creation
  order for that parent. Ordinals are not global registration order.
- Ordinal stability across a future restart is a slice-009 decision. Slice 008
  only assigns ordinals at initial spawn time.
- Child records persist after stop/panic in this slice.
- `RestartChildren` remains observed-only; it must not consult or execute child
  records yet.
- Non-restartable behavior under future `RestartChildren` execution is a
  slice-009 decision.
- `tina/tests/sputnik_api.rs` changes are API-surface only: name and construct
  the new type without changing existing one-shot behavior tests.

## Private runtime shapes

Use a small private return struct for erased spawn execution:

```rust
struct SpawnOutcome<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    child: RegisteredAddress,
    mailbox_capacity: usize,
    restart_recipe: Option<Box<dyn ErasedRestartRecipe<S, F>>>,
}
```

`restartable` is derived from whether `restart_recipe` is `Some`.
`RegisteredAddress` is the private runtime-issued address identity containing
shard id, isolate id, and `AddressGeneration`.

Use a sealed private restart-recipe trait parallel to the existing erased spawn
machinery:

```rust
trait ErasedRestartRecipe<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn create(&self, runtime: &mut CurrentRuntime<S, F>, parent: IsolateId)
        -> SpawnOutcome<S, F>;
}
```

The concrete adapter owns the `Fn() -> I + 'static` factory and mailbox
capacity. Slice 008 stores this trait object but does not call it after initial
spawn.

## Proposed implementation approach

1. Update `tina/src/lib.rs`:
   - add `RestartableSpawnSpec<I>`
   - provide `new(factory, mailbox_capacity)`
   - provide `mailbox_capacity()`
   - provide a consuming method for runtime crates to take the factory and
     capacity
   - document that it creates fresh isolate state and is separate from one-shot
     `SpawnSpec<I>`
2. Update `tina-runtime-current/src/lib.rs`:
   - add private `ChildRecord`
   - add private `Restartability` or equivalent marker
   - add private erased restart recipe trait/object
   - extend erased spawn execution to return metadata needed for child records
   - implement erased spawn for both `SpawnSpec<I>` and
     `RestartableSpawnSpec<I>`
   - assign per-parent child ordinal when the child record is written
   - add `#[cfg(test)]` snapshot helpers for record proofs
3. Add or extend crate-local unit tests in `tina-runtime-current/src/lib.rs`.
   Records are private runtime state, so unit tests are preferred over
   integration tests.
4. Keep existing integration tests passing unchanged except for imports if the
   new public type affects docs.

## Acceptance

- Root registrations do not create child records.
- One-shot `SpawnSpec<I>` children create non-restartable child records.
- `RestartableSpawnSpec<I>` children create restartable child records.
- Child records include parent id, child isolate id, child `AddressGeneration`,
  shard id, child ordinal, mailbox capacity, and restartability.
- Per-parent child ordinals are deterministic and direct-parent scoped.
- Restartable factories are called once for initial spawn in this slice.
- `RestartChildren` remains observed-only and does not call restart factories.
- Child records survive parent/child stop and panic.
- Repeated identical runs produce identical traces, lineage snapshots, and child
  record snapshots.
- `sputnik_api.rs` covers the new type as public API surface only.

## Tests and evidence

- Unit tests in `tina-runtime-current/src/lib.rs` for private child-record
  snapshots.
- Existing runtime integration tests should continue to pass.
- Run `make fmt`.
- Run `make test`.
- Run `make verify` before implementation acceptance.

## Traps

- Do not execute `RestartChildren`.
- Do not apply restart policies or budgets.
- Do not add `tina-supervisor`.
- Do not infer restartability from trace events.
- Do not clone the running child state as the restart recipe.
- Do not make every `SpawnSpec<I>` implicitly restartable.
- Do not make `RestartableSpawnSpec<I>` externally behave differently from
  `SpawnSpec<I>` before slice 009.
- Do not decide `RestartChildren` behavior for non-restartable children.
- Do not decide ordinal reuse or stability across restart.
- Do not add public runtime child-record inspection APIs.
- Do not change address generation semantics from slice 007.
- Do not add id reuse, compaction, logical names, or registries.
- Do not add new trace events for child records.

## Files likely to change

- `tina/src/lib.rs`
- `tina/tests/sputnik_api.rs`
- `tina-runtime-current/src/lib.rs`

## Areas that should not be touched

- `tina-mailbox-spsc`
- Tokio/current-thread driver work
- `tina-supervisor`
- simulator/Voyager code
- README/ROADMAP changes outside the already-pending docs cleanup

## Planning resolution

The review-resolved decisions are captured above in Phase decisions and Traps.
