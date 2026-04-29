# 006 Mariner Parent-Child Lineage

## In plain English

Right now, `CurrentRuntime` can create children for real, but it does not
remember who created them.

That is fine for spawn by itself, but it is not fine for the next supervision
work. If `RestartChildren` is going to mean anything real, the runtime needs a
real parent-child record instead of guessing later from trace order or from how
tests were set up.

This slice teaches `CurrentRuntime` one narrow new fact: every registered
isolate either has no parent because it was registered at the root, or has one
parent because another isolate spawned it. The runtime stores that lineage so a
later restart slice can use it directly.

Stored lineage beats re-deriving parenthood from the trace later. The runtime
may not hold a full trace forever, replay may be partial, and later restart
logic needs an O(1) runtime lookup instead of an O(N) event walk.

This slice is still intentionally narrow. It does not execute
`RestartChildren`, it does not restart anything, and it does not add new trace
events. It only makes runtime-owned lineage real and testable.

## What changes

- Add runtime-owned parent-child lineage to `tina-runtime-current`.
- Root-registered isolates record no parent.
- Spawned children record the isolate that spawned them.
- Add crate-local proof access so tests can inspect stored lineage without
  turning lineage into a stable public runtime API yet.

## What does not change

- This slice does not execute `RestartChildren`.
- This slice does not restart children automatically.
- This slice does not add new runtime trace events.
- This slice does not change `Send`, `Spawn`, `Stop`, stop-and-abandon, or
  panic-capture semantics.
- This slice does not add parent-child notifications to handlers.
- This slice does not add child-address return ergonomics to `Spawn`.
- This slice does not change shard placement; spawned children still stay on
  the current shard.
- Lineage points to the spawner's isolate id even if that spawner later stops
  or panics.
- Lineage entries persist after stop in this slice. Cleanup or compaction is a
  later supervision/runtime maintenance problem.
- No Tokio poll loop yet.
- No cross-shard routing yet.

## How we will verify it

- A root-registered isolate records no parent.
- A spawned child records the isolate that spawned it.
- Nested spawning proves direct-parent edges only: parent is the direct
  spawner, and ancestry is a walk across repeated parent lookups.
- Stopping or panicking an isolate does not erase already-recorded lineage.
- `RestartChildren` still remains observed-only after lineage lands.
- Repeated identical runs produce the same runtime trace and the same stored
  lineage.
