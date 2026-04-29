# Plan: 006 Mariner Parent-Child Lineage

## Goal

Make parent-child lineage a real, runtime-owned fact in `CurrentRuntime`
without widening into restart execution yet.

## Decisions

- This slice changes `CurrentRuntime` only. `SingleIsolateRunner` stays
  untouched.
- `CurrentRuntime` stores one optional parent per registered isolate:
  `None` for root registrations and `Some(parent_isolate_id)` for spawned
  children.
- The spawn path is the only runtime path that sets a parent in this slice.
- Root registration continues to mean "runtime setup-time isolate with no
  parent," even if tests register several roots.
- Stored lineage is runtime-owned state, not runtime trace vocabulary. This
  slice does not add `ParentLinked` or similar events.
- Lineage is read-only in this slice. `RestartChildren` still does not execute.
- Lineage survives later stop and panic transitions. Stopping an isolate closes
  delivery and may abandon messages, but it does not erase the recorded parent.
- To keep the stable public API narrow, lineage inspection is crate-local test
  support rather than new user-facing runtime API. This slice uses unit tests
  inside `src/lib.rs` so the runtime can expose a `pub(crate)` lineage helper
  without widening the public surface or adding a feature flag.
- Spawned children still append to registration order and still run only on a
  later `step()`.
- Lineage entries persist after stop in this slice. Cleanup or compaction is a
  later supervision/runtime maintenance decision.
- "Set at spawn, never mutated" is a slice-006 rule, not a permanent invariant.
  A later restart/replacement slice must decide whether replacement children
  inherit lineage or receive a fresh registration identity.

## Scope

1. Add parent storage to `CurrentRuntime` registrations.
2. Set parent lineage on spawn and leave root registrations parentless.
3. Add narrow unit tests in `src/lib.rs` that prove lineage for roots, spawned
   children, and nested spawned children.
4. Prove stop/panic do not erase lineage.
5. Keep `RestartChildren` observed-only and prove that lineage does not widen
   execution semantics by itself.

## Acceptance

- Root-registered isolates prove `parent == None`.
- Spawned children prove `parent == Some(spawner_isolate_id)`.
- Nested spawned children prove direct-parent edges rather than a stored
  ancestry list or guessed relationship.
- Stored lineage remains available after stop and panic paths run.
- `RestartChildren` still remains observed-only after this slice.
- No new runtime trace events are required for lineage.
- `make verify` passes.

## Traps

- Do not execute `RestartChildren` in this slice.
- Do not infer lineage later from trace order instead of storing it now.
- Do not widen lineage into a stable public API just to make tests convenient.
- Do not erase lineage when an isolate stops or panics.
- Do not change spawn ordering or spawn ingress semantics while adding lineage.
- Do not add supervisor policy or restart-budget behavior here.

## Verify

```bash
make verify
```
