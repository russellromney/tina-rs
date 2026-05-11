# 084 Child Lifecycle / Join / Supervision Usability

## Status

- Done: plan created after 070 sharded ergonomics and 079 cancellation
  round 2 landed. 081 multi-turn request context is in progress and
  should land first if this phase needs request-shaped examples.
- In progress: none.
- Open: implement child-start observation inside the isolate model,
  typed child refs, join/stop helpers, and specimen upgrades.
- Deferred: cross-shard child ownership, distributed supervision,
  full Erlang/OTP strategy matrix, timed restart-budget windows.

## Goal

Make parent/child ownership boring.

Current Tina can spawn and supervise children, and the host can observe
supervised restarts. But a parent that spawns a child does not get the
fresh typed child address as normal message data. Real services want to:

```text
spawn child
get child's typed address
send follow-up work
observe child stopped/result
replace child after restart
stop children when owner stops
```

Grug truth:

```text
parent owns child.
spawn creates address.
parent should see address.
child stop is a fact.
restart gives new generation.
old address becomes stale.
```

## Non-Goals

- No host-side `observe_child_started::<M>()` unless spawn events carry
  enough type truth. Caller-guessed address types are not OK.
- No broad supervision rewrite.
- No ambient child registry with hidden refresh.
- No global kill-all.
- No cross-shard child ownership in first form.
- No restart strategy expansion unless required by the new address/result
  surface.
- No macro syntax.

## Shape

Two PRs max:

1. `ChildRef` + `spawn_observed` + specimen/docs updates. This is the
   load-bearing user win.
2. Child stop/join/restart polish only where PR 1 or existing specimens
   prove the exact shape. If this gets fuzzy, stop after PR 1 and leave
   the follow-up recorded.

## Rock 0 — Re-read Existing Truth

Read before code:

- `examples/FINDINGS.md` finding 14;
- `.intent/phases/064-service-bootstrap-and-fanout-ergonomics/design-rock-2-initial-child-spawn-observation.md`;
- `examples/specimen_supervised_worker`;
- `examples/specimen_dynamic_worker_pool`;
- current `observe_child_restarted` implementation/tests.

Decision: first form is `spawn_observed`, not host
`observe_child_started`. The typed address stays inside the isolate
model.

## Rock 1 — Typed ChildRef

Add a tiny typed value:

```rust
pub struct ChildRef<M, R = ()> {
    pub address: Address<M, R>,
    pub generation: AddressGeneration,
}
```

Maybe include parent id if it is already cheap and true. Do not add
fields just because they are interesting.

Rules:

- API home is the `tina` trait crate if the value is part of the
  effect surface; runtime-only waiters stay in `tina-runtime`;
- typed by child message/reply;
- copy/clone if `Address` shape allows it;
- generation is visible;
- no hidden liveness claim.

## Rock 2 — spawn_observed

Add a new effect helper:

```rust
spawn_observed(ChildDefinition::new(child, cap))
    .reply(ParentMsg::ChildStarted)
```

The continuation receives a typed outcome:

```rust
Result<ChildRef<ChildMsg, ChildReply>, SpawnObservedError>
```

Rules:

- same runtime spawn semantics as `spawn`;
- no child runs before the spawn effect is committed;
- parent receives the typed child address as an ordinary later message;
- if spawn fails or parent stops before delivery, outcome is typed and
  trace-visible;
- no hidden queue beyond existing child/message mailboxes.
- type honesty comes from the typed child definition at the spawn site,
  not from a host turbofish guessed after the fact.

If implementing this as `Effect::SpawnObserved` is cleaner than
bolting it onto `Effect::Spawn`, do that. Keep old `spawn(...)`.

## Rock 3 — Join / Stop Specific Child

Design and implement only if the shape is already obvious after Rock 2.
Otherwise record the shape and defer.

Candidate shape:

```rust
observe_child_complete(child_ref).reply(ParentMsg::ChildDone)
stop_child(child_ref).reply(ParentMsg::ChildStopped)
```

Returned facts should distinguish:

- stopped normally;
- stopped with typed result;
- panicked/stopped by supervisor if already represented;
- wrong generation / stale address;
- already stopped;
- parent does not own this child.

Do not overbuild. If result join already composes cleanly from
`observe_result::<T>(child.address)` plus `observe_isolate_complete`,
ship docs/helper only where repetition proves it.

## Rock 4 — Restart Replacement Address

Today `observe_child_restarted(parent)` exists. Make the replacement
address easy to use from the parent/host story:

- restarted child carries generation;
- parent can refresh its stored `ChildRef`;
- stale old address gives `Closed`/typed stale outcome, not silent
  delivery;
- docs show "old child ref out, new child ref in".

If the restart event needs child type identity to be honest, keep the
host typed waiter deferred and make `spawn_observed` the shipped win.

## Rock 5 — Owner Stop Cleanup

Prove what happens when parent stops:

- direct children stop predictably, or docs say they are not stopped;
- pending child joins/observations settle visibly;
- no child result waiter leaks capacity;
- no hidden orphan child continues unless that is the explicit policy.

If current runtime behavior is weaker than desired, either fix it here
or narrow the public claim loudly. Do not imply tree shutdown if the
runtime only stops one isolate.

## Rock 6 — Specimens / Docs

Update at least:

- `specimen_supervised_worker`;
- `specimen_dynamic_worker_pool`;
- user-guide supervision page;
- `examples/FINDINGS.md` finding 14.

Remove Boot/self-address workarounds where `spawn_observed` makes them
unnecessary. Leave a note where a workaround remains because the helper
does not cover that shape.

System specimens that should use this once it lands:

- `system_job_queue`;
- `system_realtime_rooms`;
- `system_media_ingest_pipeline` if process workers are child isolates.

## Required Proof

PR 1 proof:

- parent spawns child and receives typed `ChildRef` via ordinary message;
- parent sends a follow-up message to that child using the ref;
- spawn failure / parent stopped before continuation is typed and
  trace-visible, or the plan explains why current spawn has no such
  failure path;
- simulator mirrors `spawn_observed`, or the PR explicitly defers sim
  with a follow-up finding;
- old `spawn(...)` behavior remains unchanged.

PR 2 proof, only if Rock 3/4/5 land:

- parent observes child stop/result;
- stale generation does not deliver silently;
- supervised restart produces a new generation and docs/test show refresh;
- parent stop settles child lifecycle truth;
- waiter/result capacity is reclaimed after child stop, parent stop, and
  waiter timeout.

## Done Means

- parent no longer needs a child `Boot { self_addr }` message just to
  learn the child's address in the common case;
- child refs are typed, generation-aware, and trace-honest;
- at least one existing specimen gets simpler; two if Rock 3/4 land;
- docs teach spawn -> child ref -> follow-up -> join/stop;
- no host-side typed child-start waiter ships unless it is type-honest.
