# 005 Mariner Local Spawn Dispatch

## In plain English

Right now, `CurrentRuntime` can execute `Send`, `Stop`, and panic capture, but
`Spawn` is still only traced as an observed effect.

That leaves Mariner stuck right before supervision. A runtime cannot restart
children honestly if it cannot create children honestly first.

This slice makes local spawn real on one shard.

When a handler returns `Effect::Spawn`, the runtime should create the child,
give it a real mailbox from a runtime-owned mailbox factory, record the child
in the trace, and append it to the runtime's registration order. The child does
not run in the same round it was spawned.

This slice is still intentionally narrow: it does not execute `Reply`, it does
not execute `RestartChildren`, and it does not promise that the spawning
handler gets the child address back directly. It just makes child creation a
real runtime behavior instead of a traced placeholder.

## What changes

- Add a runtime-owned typed mailbox factory abstraction to
  `tina-runtime-current` so `CurrentRuntime` can create mailboxes for spawned
  children without hard-coding SPSC into `tina`.
- Execute `Effect::Spawn(SpawnSpec<I>)` in `CurrentRuntime`.
- Add a new runtime trace event, `Spawned { child_isolate }`, for the child
  created by the runtime.
- Add a runtime ingress API for external code to enqueue a typed message to a
  registered isolate through `CurrentRuntime` without holding the raw mailbox.
- Use runtime-private sealed spawn erasure so `SpawnSpec<I>` becomes a
  type-safe erased spawn request inside `tina-runtime-current` without adding
  new trait surface to `tina`.
- Spawn execution stays on the current shard. The spawned child is local to the
  parent's shard in this slice.

## What does not change

- This slice does not execute `Reply`.
- This slice does not execute `RestartChildren`.
- This slice does not introduce supervision policy execution, restart budgets,
  or parent notification yet.
- Explicit `Effect::Stop` behavior from slice 003 stays the same.
- Panic capture from slice 004 stays the same.
- The trace still carries runtime facts only, not user payloads.
- The spawning handler does not receive the child address back directly in this
  slice.
- This slice does not yet commit to internal parent-child bookkeeping. That
  lands when restart or supervision behavior makes it externally meaningful.
- The parent isolate's single associated `Spawn` type stays the constraint for
  what it can request. If one parent needs several child shapes, that remains
  an isolate-level modeling problem such as an enum, not a runtime feature.
- `SpawnSpec` mailbox capacity is forwarded to the runtime mailbox factory, but
  capacity `0` is programmer error in this slice and the runtime panics rather
  than creating a child that can never receive.
- A spawned child does not run in the same step round that created it.
- No Tokio poll loop yet.
- No cross-shard routing yet. Cross-shard runtime ingress also stays out of
  scope and panics rather than silently misrouting.
- No mailbox semantics changes.
- `SingleIsolateRunner` stays the narrow trace-core helper from slice 001.

## How we will verify it

- A handler that returns `Effect::Spawn` produces a `Spawned { child_isolate }`
  runtime event.
- The spawned child gets the next `IsolateId` and is appended to registration
  order.
- The spawned child is created on the current shard, not routed elsewhere.
- The spawned child does not run in the same step round that spawned it.
- External code can enqueue a typed message to the spawned child through the
  runtime ingress API, and the child handles it on a later step.
- Cross-shard runtime ingress panics instead of silently misrouting.
- Sends to an unknown isolate ID through the runtime ingress API still panic as
  programmer error, matching existing runtime send behavior.
- A spawn request with mailbox capacity `0` panics instead of creating an
  unreachable child.
- Repeated identical runs produce the same event sequence and the same causal
  links, including spawn events.
- `Reply` and `RestartChildren` remain observed and traced, not executed.
