# 009 Mariner RestartChildren Execution

## In plain English

`CurrentRuntime` can now remember which children were spawned by a parent and
which of those children have a restart recipe. The next real runtime step is to
make `Effect::RestartChildren` use that stored runtime state.

This slice gives `RestartChildren` one narrow meaning:

When a parent handler returns `RestartChildren`, the runtime walks that parent's
direct child records in child-ordinal order. Restartable children are replaced
with fresh isolate state from their stored factory. Non-restartable children are
not restarted, but the skip is traced. The runtime does not apply supervision
policies, budgets, or sibling selection rules yet.

This keeps the behavior honest and visible. A restart request is no longer a
generic observed effect, but it also does not pretend to be a full supervisor.

## What changes

- `Effect::RestartChildren` is executed by `CurrentRuntime` for the current
  isolate's direct children.
- Restart execution consumes private child records created in slice 008.
- Direct child records are visited in deterministic per-parent child-ordinal
  order.
- Restartable child records create fresh replacement isolate state by calling
  their stored restart recipe.
- Replacement children receive fresh monotonic isolate ids and initial
  `AddressGeneration`.
- A restarted child keeps the same child ordinal. The child record is updated to
  point at the current replacement incarnation rather than gaining a new
  ordinal.
- The old child incarnation is stopped if it is still running. Its mailbox is
  closed and already-buffered messages are abandoned using the existing
  stop-and-abandon path.
- If the old child incarnation already stopped or panicked, restart still
  creates a replacement and does not emit a duplicate `IsolateStopped`.
- Old child addresses become stale/closed because they name the old isolate
  incarnation. New child addresses are available only through crate-private test
  snapshots in this slice.
- Non-restartable direct children are skipped with a trace event. They are not
  silently ignored and they do not make the restart request panic.
- Runtime trace events are extended with explicit restart-attempt, restart-skip,
  and restart-complete events carrying child ordinal and old/new address
  identity.
- Repeated identical runs produce the same restart trace, lineage snapshot, and
  child-record snapshot.

## What does not change

- This slice does not add `tina-supervisor`.
- This slice does not apply `RestartPolicy` or `RestartBudget`.
- This slice does not implement one-for-one, one-for-all, or rest-for-one
  sibling selection. `RestartChildren` restarts every restartable direct child
  of the isolate that returned the effect.
- This slice does not restart grandchildren recursively.
- This slice does not expose public child-record or fresh-address inspection
  APIs.
- This slice does not add logical names, registries, aliases, or address
  refresh APIs.
- This slice does not reuse isolate ids or implement compaction.
- This slice does not change `SpawnSpec<I>` or `RestartableSpawnSpec<I>`.
- This slice does not change local send, local spawn, stop-and-abandon,
  panic-capture, address-liveness, cross-shard, or I/O semantics except where
  restart execution deliberately uses those existing mechanisms.
- This slice does not catch panics from restart factories. A factory panic is a
  runtime construction error and propagates; it is not reported as
  `HandlerPanicked`.

## How we will verify it

- A parent with one restartable child returns `RestartChildren`; the runtime
  stops the old child, creates a fresh child, updates the child record, and
  records restart trace events.
- Sends to the old child address return `Closed`; sends to the replacement
  address from the crate-private snapshot can be delivered on a later step.
- A parent with a stopped or panicked restartable child can restart it without a
  duplicate `IsolateStopped` event for the old incarnation.
- A parent with multiple restartable children restarts them in child-ordinal
  order.
- A parent with non-restartable children records skip events and does not
  panic.
- Restart execution drains and traces abandoned messages from running old
  child mailboxes before replacement.
- Restart execution does not restart grandchildren.
- Repeated identical runs produce identical traces, lineage snapshots, and
  child-record snapshots.
- Generated-history property tests are extended so restart attempts have
  visible outcomes and causal links remain well formed.

## Autonomy note

This slice resolves the slice-008 deferred semantic choices as follows:

- non-restartable children are skipped with trace
- child ordinals are stable across restart
- `RestartChildren` means "restart direct restartable children of this parent"
  until a later supervisor/policy slice adds more selective behavior

These are runtime semantics, not public API shape. They should be reviewed, but
implementation can run autonomously if reviewers agree with this contract.
