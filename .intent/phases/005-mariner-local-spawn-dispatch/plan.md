# Plan: 005 Mariner Local Spawn Dispatch

## Goal

Make local child creation a real runtime behavior in `CurrentRuntime` without
widening into reply delivery or restart execution yet.

## Decisions

- This slice changes `CurrentRuntime` only. `SingleIsolateRunner` stays as the
  narrow trace-core helper from slice 001.
- `CurrentRuntime` gains a typed mailbox factory owned by the runtime crate.
  The factory lives in `tina-runtime-current`, not in `tina`.
- The mailbox factory shape is one generic trait on the runtime side:
  `fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>>`.
  This slice does not add a `Send` bound because `tina-runtime-current` is
  still a single-threaded runtime and current test mailboxes are not `Send`.
- `CurrentRuntime` executes `Effect::Spawn(SpawnSpec<I>)` by:
  1. creating a mailbox from the factory using the requested capacity,
  2. assigning the next `IsolateId`,
  3. registering the child at the end of registration order,
  4. recording `Spawned { child_isolate }`.
- Spawn erasure parallels the existing handler/mailbox erasure pattern, but it
  uses a sealed runtime-private trait pair rather than storing a closure per
  isolate. `HandlerAdapter` turns `SpawnSpec<I>` into
  `ErasedEffect::Spawn(Box<dyn ErasedSpawn<S, F>>)`, and the sealed
  `IntoErasedSpawn<S, F>` / `ErasedSpawn<S, F>` pair is implemented for
  `SpawnSpec<I>` and `Infallible`.
- `Spawned` has exactly one cause: the spawning isolate's
  `HandlerFinished { effect: Spawn }` event.
- Spawned children stay on the current shard in this slice.
- A spawned child is eligible to run only on a later `step()`, never in the
  same round that created it.
- `CurrentRuntime` gains a public typed ingress API for external code to
  enqueue a message to a registered isolate:
  `try_send<M: 'static>(&self, address: Address<M>, message: M) -> Result<(), TrySendError<M>>`.
  This is the runtime-side ingress surface for tests and later drivers; it is
  not a `tina` trait change.
- The runtime ingress API uses the same bounded mailbox semantics as direct
  mailbox usage: `Full` and `Closed` stay explicit.
- `CurrentRuntime::try_send` panics if `address.shard() != self.shard.id()`.
  Cross-shard runtime ingress is out of scope in this slice; silent misrouting
  would be worse than panic.
- Missing-target runtime ingress remains a programmer-error panic, matching
  the existing unknown-target `Send` behavior.
- The spawning handler does not receive the child address back directly in this
  slice. Child creation becomes real before child-address ergonomics do.
- `CurrentRuntime::new` becomes `CurrentRuntime::new(shard, mailbox_factory)`.
- Step-round iteration is bounded to the pre-round registry snapshot. A child
  spawned mid-round is appended to registration order but cannot run until a
  later `step()`.
- `SpawnSpec` mailbox capacity is forwarded to the factory, but capacity `0`
  is programmer error in this slice and the runtime panics instead of creating
  a child that can never receive.
- This slice does not commit to internal parent-child bookkeeping yet. That
  data should land when restart or supervision behavior makes it externally
  testable.

## Scope

1. Add a mailbox-factory abstraction to `tina-runtime-current`.
2. Update `CurrentRuntime` construction so the runtime owns a mailbox factory.
3. Execute `Effect::Spawn` in `CurrentRuntime` and emit `Spawned`.
4. Add a typed runtime ingress method for external message injection.
5. Add integration tests in `tina-runtime-current/tests/` for spawn dispatch,
   child ordering, later-step child execution, deterministic reruns, and the
   continued observed-only behavior of `Reply` and `RestartChildren`.

## Acceptance

- A `Spawn` effect no longer ends at `EffectObserved`; it creates a child and
  records `Spawned { child_isolate }`.
- The spawned child gets the next `IsolateId` and is appended to registration
  order.
- The spawned child stays on the current shard.
- The spawned child does not run in the same step round that spawned it.
- A later external message sent through the runtime ingress API reaches the
  spawned child and produces exactly one handler invocation.
- The runtime ingress API returns `Full` and `Closed` as typed
  `TrySendError<M>` values, and still panics on unknown isolate IDs and
  cross-shard addresses.
- A spawn request with mailbox capacity `0` panics instead of creating an
  unreachable child.
- The trace for identical runs stays deterministic, including spawn events.
- `Reply` and `RestartChildren` remain observed-only.
- `make verify` passes.

## Traps

- Do not add mailbox-factory helpers to `tina`; this is runtime-crate surface.
- Do not force a `Send` bound onto the mailbox factory just to mirror future
  runtimes; this runtime is still single-threaded.
- Do not execute `Reply` or invent a caller/request model in this slice.
- Do not execute `RestartChildren` or sneak in restart policy behavior.
- Do not make spawned children run recursively in the same round.
- Do not give spawned children special ordering rules; they append to normal
  registration order.
- Do not make spawn depend on `tina-mailbox-spsc` specifically for correctness
  tests.
- Do not pretend the spawning handler receives the child address directly; that
  is still out of scope here.
- Do not silently accept cross-shard runtime ingress.
- Do not silently create a child with mailbox capacity `0`.

## Verify

```bash
make verify
```
