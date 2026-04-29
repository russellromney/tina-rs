# 008 Mariner Restartable Child Records

## In plain English

`CurrentRuntime` can spawn children and remember direct parent-child lineage, but
it still cannot restart a child. The missing piece is not the restart action
yet; it is the runtime-owned record that says which children are restartable and
how a fresh replacement should be created.

This slice records that information at spawn time.

There are two kinds of spawn payloads after this slice:

- one-shot spawn payloads, which create a child once and are not restartable
- restartable spawn payloads, which create the initial child from a factory and
  keep that factory as the child's restart recipe

These are deliberately two public types rather than one type with a mode flag.
`SpawnSpec<I>` keeps meaning "spawn this concrete child once."
`RestartableSpawnSpec<I>` makes restartability visible at the call site and
confines the repeatable `Fn() -> I` factory requirement to children that are
actually restartable.

The runtime still does not execute `RestartChildren`. It only records enough
private state for the next slice to execute restart behavior without guessing
from trace order, cloning a running child, or pretending every child can be
restarted.

This is a medium-size IDD slice by this project's standards. It adds one public
trait-crate type and several private runtime mechanisms, so review should budget
attention accordingly even though restart execution remains out of scope.

## What changes

- Add a restartable spawn payload to the trait crate. It represents "create this
  child from this factory with this mailbox capacity."
- Keep the existing `SpawnSpec<I>` as a one-shot spawn payload.
- `CurrentRuntime` records a private child record for each spawned child.
- A child record includes:
  - the direct parent isolate id
  - the child address identity from slice 007: shard id, isolate id, and
    `AddressGeneration`
  - a deterministic per-parent child ordinal
  - mailbox capacity
  - whether the child is restartable
  - the restart recipe when the child is restartable
- Restartable child records use a factory to create fresh isolate state.
  Restart must not clone or reuse the running child state.
- One-shot children are recorded as non-restartable. They remain valid spawned
  children, but the runtime does not claim it can replace them later.
- In this slice, `RestartableSpawnSpec<I>` has no externally observable runtime
  behavior beyond `SpawnSpec<I>`: both spawn one child. The difference is private
  recorded restartability for slice 009.
- Parameterized restartable children use normal Rust closure captures, for
  example `RestartableSpawnSpec::new(move || Worker::new(tenant_id), 16)`.
- Child records are runtime-owned state. Future restart execution must consume
  these records rather than reconstructing restartability from the trace.
- Child records persist after a child stops or panics in this slice.

## What does not change

- This slice does not execute `RestartChildren`.
- This slice does not decide what `RestartChildren` does with non-restartable
  children. Slice 009 must decide whether they are skipped, rejected with trace,
  or treated as a runtime configuration error.
- This slice does not decide whether child ordinals remain stable across a
  future restart. Slice 008 only assigns ordinals at initial spawn time.
- This slice does not restart children after panic or stop.
- This slice does not add `tina-supervisor`.
- This slice does not apply `RestartPolicy` or `RestartBudget`.
- This slice does not add public runtime inspection APIs for child records.
- This slice does not change address liveness or generation semantics.
- This slice does not add id reuse or compaction.
- This slice does not add logical names, registries, aliases, or address
  refresh APIs.
- This slice does not change local send, stop-and-abandon, panic-capture, or
  spawn scheduling semantics.
- This slice does not add new runtime trace events.
- This slice does not change cross-shard behavior.

## How we will verify it

- A root-registered isolate creates no child record.
- A child spawned from the existing one-shot `SpawnSpec<I>` records direct
  parent, child address identity, child ordinal, mailbox capacity, and
  `restartable = false`.
- A child spawned from the new restartable spawn payload records the same child
  metadata plus `restartable = true`.
- Public API-surface checks prove the new restartable payload can be named and
  constructed without changing `SpawnSpec<I>`.
- The restartable factory is called exactly once for the initial spawn in this
  slice; `RestartChildren` remains observed-only and does not call it again.
- Nested spawns record direct parent records and deterministic per-parent child
  ordinals.
- Child records persist after a child stops or panics.
- Repeated identical runs produce the same trace, lineage snapshot, and child
  record snapshot.

## Autonomy note

This slice proposes a new public spawn payload type, so spec and plan review
should treat the API shape as a human-escalation decision. Once that type shape
is accepted, implementation can run autonomously unless another pause gate
trips.
