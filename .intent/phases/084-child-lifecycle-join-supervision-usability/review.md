# 084 Plan Review

## Plan Review 1

### Finding 1 — `spawn_observed` needs an outcome, not only an address

The first plan sketched `spawn_observed(...).reply(ChildStarted)` but
did not pin what happens when spawn cannot produce a child or the parent
stops before the continuation is delivered. That would let an
implementation invent either silent drop or panic semantics. The plan now
requires a typed `Result<ChildRef<_, _>, SpawnObservedError>` shape, or an
explicit proof that the current spawn path has no such failure mode.

### Finding 2 — Type honesty must come from the spawn site

The plan correctly rejected host-side `observe_child_started::<M>()`,
but it did not say where `spawn_observed` gets its type truth. The fix is
to state it plainly: type truth comes from the typed child definition at
the spawn site, not from a later host turbofish.

### Finding 3 — Required proof mixed PR 1 and PR 2 scope

The first proof list required child join, restart replacement, owner-stop
cleanup, and simulator parity even though the phase shape says PR 1 may
stop after `ChildRef + spawn_observed`. That would make the first
implementation session either too large or falsely incomplete. The proof
section now separates PR 1 proof from PR 2 proof.

### Finding 4 — Supervision rewrite risk

The plan's title includes supervision, but the load-bearing pain is child
address/result usability. The non-goals now explicitly reject a broad
supervision rewrite, and the shape says two PRs max.

## Decision

Proceed with 084 after 081 if current request-context examples need it.
The first implementation should ship only `ChildRef`, `spawn_observed`,
tests, docs, and specimen cleanup unless join/stop falls out naturally.

## Implementation Review Follow-Up

Hostile review found an important correction: parent-delivery failure for the
`spawn_observed` continuation cannot itself always be reported by delivering a
second message to that same parent mailbox. If the mailbox is full or closed,
that second message would hit the same failure. The implementation therefore
keeps typed `SpawnObservedError` for spawn construction rejection
(`ZeroMailboxCapacity`) and records parent-delivery rejection through the
existing `SendRejected` trace path. No hidden queue, no mailbox bypass.

The macro surface also changed from implicit to opt-in. `#[tina::isolate]`
and `#[tina_runtime::isolate]` default `SpawnObserved = Infallible`; isolates
that use `spawn_observed` declare the associated type with
`spawn_observed = tina::SpawnObserved<...>`. This avoids requiring every custom
spawn payload to implement `SpawnAddress`.
