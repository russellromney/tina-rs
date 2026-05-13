# 084 Child Lifecycle / Join / Supervision Usability

## Status

- Done: plan + review.
- Done: `ChildRef`, `spawn_observed`, typed spawn construction failure,
  traced parent-delivery rejection, live/sim proof, docs/specimen cleanup.
- Closed: first form merged. Child join/stop/restart polish moves to later
  work when a real caller proves the exact shape.
- Deferred: cross-shard child ownership, distributed supervision,
  supervisor strategy matrix, timed restart windows.

## Goal

Parent spawns child. Parent gets typed child address. Parent can use it.

Today `spawn(...)` creates a child but does not hand the parent a typed
address as message data. Users work around this with Boot messages and
trace spelunking.

Grug truth:

```text
parent owns child.
spawn makes address.
parent should see address.
restart makes new generation.
old address stale.
```

## Non-Goals

- No host `observe_child_started::<M>()` until spawn events carry type
  truth.
- No caller-guessed child address types.
- No broad supervision rewrite.
- No global kill-all.
- No cross-shard child ownership.
- No macro.

## Shape

One PR.

Ship `ChildRef` + `spawn_observed` + docs/specimen proof. Add
join/stop/restart polish in the same PR only if the shape is already
boring. If it gets fuzzy, defer it.

## Rock 0 — Read First

- `examples/FINDINGS.md` finding 14.
- `064 .../design-rock-2-initial-child-spawn-observation.md`.
- `specimen_supervised_worker`.
- `specimen_dynamic_worker_pool`.
- current `observe_child_restarted` tests.

Decision: first form is `spawn_observed`, not host
`observe_child_started`.

## Rock 1 — ChildRef

Add:

```rust
pub struct ChildRef<M, R = ()> {
    pub address: Address<M, R>,
    pub generation: AddressGeneration,
}
```

Rules:

- lives in `tina` if it is part of `Effect` surface;
- typed by child message/reply;
- generation visible;
- no liveness promise.

## Rock 2 — spawn_observed

Add:

```rust
spawn_observed(ChildDefinition::new(child, cap))
    .reply(ParentMsg::ChildStarted)
```

Continuation gets:

```rust
Result<ChildRef<ChildMsg, ChildReply>, SpawnObservedError>
```

Rules:

- same spawn semantics as `spawn`;
- old `spawn(...)` unchanged;
- child address type comes from the child definition at spawn site;
- parent receives result as ordinary later message;
- spawn construction failure is typed and traced;
- parent-delivery rejection is traced through the normal bounded-mailbox
  send rejection path; it is not delivered through a hidden queue;
- no hidden queue beyond existing mailboxes.

Use `Effect::SpawnObserved` if that is the clean shape.

## Rock 3 — Join / Stop Child

Only ship if obvious in this PR.

Candidate:

```rust
observe_child_complete(child_ref).reply(ParentMsg::ChildDone)
stop_child(child_ref).reply(ParentMsg::ChildStopped)
```

Must distinguish:

- stopped;
- stopped with typed result;
- stale generation;
- already stopped;
- not this parent's child.

If existing `observe_result` + `observe_isolate_complete` is enough,
document that and defer helper.

## Rock 4 — Restart Address

`observe_child_restarted(parent)` exists. Make refresh easy:

- new generation visible;
- parent can replace stored `ChildRef`;
- old ref is stale/closed visibly;
- no type-guessing waiter unless event carries type truth.

## Rock 5 — Parent Stop

Prove or document:

- direct children stop, or they do not;
- child waiters settle;
- waiter/result capacity is reclaimed;
- no hidden orphan claim.

Do not claim tree shutdown unless runtime does tree shutdown.

## Specimens / Docs

Update:

- `specimen_supervised_worker`;
- `specimen_dynamic_worker_pool` if it fits PR scope;
- supervision guide;
- finding 14.

Remove Boot/self-address workaround where `spawn_observed` replaces it.

## Proof

Required:

- parent gets `ChildRef` as message;
- parent sends follow-up to child;
- spawn construction failure is typed;
- abandoned parent delivery is traced without a hidden queue;
- sim mirrors live, or records explicit follow-up;
- old `spawn` still works.

If join/stop/restart ships:

- parent observes child stop/result;
- stale generation does not silently deliver;
- restart gives new generation;
- parent stop settles child truth;
- waiter capacity is reclaimed.

## Done

- common child-address Boot pattern is gone;
- child refs are typed and generation-aware;
- at least one specimen gets simpler;
- docs teach spawn -> child ref -> follow-up -> join/stop;
- no host child-start waiter ships unless type-honest.
