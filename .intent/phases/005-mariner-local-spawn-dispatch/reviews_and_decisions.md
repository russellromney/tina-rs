# Reviews and Decisions: 005 Mariner Local Spawn Dispatch

## Round 1: Spec Diff Review

Artifact reviewed:

- `.intent/phases/005-mariner-local-spawn-dispatch/spec-diff.md`

Read against:

- `.intent/SYSTEM.md`
- `.intent/phases/004-mariner-panic-capture/` (the just-completed slice)
- `tina/src/lib.rs` (Effect, SpawnSpec, Isolate trait shape)
- `tina-runtime-current/src/lib.rs` (current ErasedEffect::Spawn arm,
  register signature, type-erasure machinery)

### Positive conformance review

Judgment: passes

- **P1.** Right slice to pick. Codex waited on Spawn until panic
  capture closed the round-survival gap (a runtime that loses a child
  to handler panic cannot honestly run a parent that owns children).
  Reply is still under-specified (no caller/request model). Spawn is
  the next most honest move toward supervision.
- **P2.** "Runtime-owned mailbox factory" is the right framing. It
  keeps `tina` from learning about specific mailbox crates and
  preserves the SYSTEM.md crate-boundary rule that mailbox behavior
  belongs in mailbox crates.
- **P3.** "Spawned child does not run in the same step round it was
  spawned in" preserves the slice-002 snapshot-then-deliver invariant
  and avoids recursive handler execution.
- **P4.** Parent-child recording is *groundwork* for supervision
  without committing to `RestartChildren` execution. That is the
  right discipline — make the data real before the policy is real.
- **P5.** `Reply` and `RestartChildren` are explicitly held as
  observed-only, with a verification criterion. Slice stays narrow.
- **P6.** The new runtime ingress API is correctly motivated: tests
  and later external drivers need a way to send to spawned children
  whose mailboxes they do not hold.

### Negative conformance review

Judgment: passes with four real holes

These are gaps where the spec leaves big implementation questions
implicit. Each one is load-bearing for slice 005, not bookkeeping.

- **N1. The mailbox factory abstraction is named but not
  characterized.** Is it one factory trait with a generic
  `fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>>`?
  Is it a factory per message type? Does it produce a typed
  `Mailbox<T>` or an `ErasedMailbox`? The choice has ripple effects
  on (a) the public surface of `tina-runtime-current`, (b) what
  tests can substitute, and (c) whether registered and spawned
  isolates use the same internal mailbox shape. Spec should pin at
  least the shape, even if not the exact signature.
- **N2. The path from `Effect::Spawn(SpawnSpec<I>)` to real child
  registration is not addressed.** Today `ErasedEffect::Spawn` is a
  `Box<dyn Any>` — the runtime cannot pull a concrete `I` out of it
  generically. Real registration requires either (a) the parent's
  `register()` call to provide a "spawn adapter" closure that knows
  how to construct a child of type `I`, or (b) `SpawnSpec` to carry
  enough type info to type-erase its own registration. This is a
  major piece of new type machinery (parallel to the existing
  `MailboxAdapter` / `HandlerAdapter`). Spec should at least sketch
  it.
- **N3. The relationship between `register()` and `Spawn` is not
  pinned.** `register()` is the existing public setup-time API.
  `Spawn` is the new runtime-time path. Are spawned children
  indistinguishable from registered ones except for the parent link?
  Are root-registered isolates considered to have no parent? Spec
  needs one sentence.
- **N4. Shard placement of spawned children is not stated.**
  Implicitly "same shard as parent" because the runtime is
  single-shard, but a future cross-shard slice will need this rule.
  One sentence.

### Adversarial review

Judgment: passes, with two structural risks worth being honest about

- **A1. A parent's `type Spawn` is a single type.** Per
  `tina/src/lib.rs:128`, `Isolate::Spawn` is one associated type. A
  parent that wants to spawn different child types in different
  branches cannot, because `SpawnSpec<ChildA> ≠ SpawnSpec<ChildB>`.
  This is a Sputnik-level constraint, not a 005 defect, but the spec
  should acknowledge what "real spawn" can and cannot do in this
  slice, so future readers (and the next slice that wants
  multi-child-type parents) know to lift the constraint
  deliberately, not by accident.
- **A2. The spawning handler does not receive the child address
  back.** This is the visible API gap that real Tina (Odin) and
  Erlang OTP do not have. Slice 005 closes the *runtime dispatch*
  gap (children get created for real) but leaves the *parent-side
  ergonomics* gap. That is acceptable for one slice, but the spec
  should explicitly say *why* this is acceptable here (runtime
  behavior is load-bearing, ergonomics can come later) so the next
  slice has to reopen intent before adding a "child address return"
  ergonomics layer that quietly invents new semantics.

What I checked and found defended:

- Single-cause trace shape: `Spawned` has exactly one cause
  (`HandlerFinished{effect: Spawn}`).
- Spawned children are appended to normal registration order, no
  special priority lane.
- Reply / RestartChildren remain observed-only.
- Trace still carries runtime facts only; no user payloads.
- Same-step-round non-execution preserved.

---

## Round 2: Plan Review

Artifact reviewed:

- `.intent/phases/005-mariner-local-spawn-dispatch/plan.md`

Read against:

- the slice 005 spec diff and Round 1 above
- `tina-runtime-current/src/lib.rs` (current entries vec, step loop,
  ErasedEffect machinery)

### Positive conformance review

Judgment: passes

- **P1.** Decisions correctly pin the causal link (`Spawned` caused
  by `HandlerFinished{effect: Spawn}`).
- **P2.** Plan correctly states children append to registration order
  and run only on later `step()`.
- **P3.** Plan correctly forbids spawn from depending on
  `tina-mailbox-spsc` for correctness tests (matches the SYSTEM.md
  rule that runtime correctness tests should not couple two fresh
  implementations unless coupling is the point).
- **P4.** Plan correctly forbids `Reply` and `RestartChildren`
  execution and forbids handing the child address back to the parent.
- **P5.** Plan correctly says the factory lives in
  `tina-runtime-current`, not `tina`.

### Negative conformance review

Judgment: passes with five things to pin before implementation

- **N1. Factory trait shape is undecided.** Same as Round 1 N1, from
  the plan side. Without pinning the shape, the implementation will
  invent it under the hood and the next reviewer has to backfill the
  intent. Recommend: one trait method like `fn create<T: Send +
  'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>>`, with
  `Send + 'static` as the only bounds tina cares about.
- **N2. Spawn-execution type machinery is undecided.** Same as Round
  1 N2. The plan needs to say *how* the runtime turns
  `ErasedEffect::Spawn(Box<dyn Any>)` into a registered child. The
  natural answer (parallel to existing adapters) is that
  `register()` captures a "spawn adapter" closure of type `Fn(Box<dyn
  Any>, &dyn MailboxFactory) -> RegisteredEntry<S>` for the parent's
  declared `type Spawn = SpawnSpec<ChildI>`. Plan should sketch.
- **N3. Runtime ingress API signature is unpinned.** Likely
  `pub fn try_send<M: Send + 'static>(&self, address: Address<M>,
  msg: M) -> Result<(), TrySendError<M>>` — but the plan doesn't say.
  The API needs to look up the mailbox by `IsolateId`, which means
  the runtime gains a small new lookup table or method. Pin the
  signature so reviewers can check it.
- **N4. Constructor change is unaddressed.** The current
  `CurrentRuntime::new(shard)` takes only the shard. Adding a
  factory parameter breaks every existing test. Either bump to
  `CurrentRuntime::new(shard, factory)` (breaks tests, fine,
  pre-launch — but say so) or add `with_factory(...)` and keep `new`
  with a default (silent coupling to a default mailbox impl —
  contradicts Trap "do not make spawn depend on tina-mailbox-spsc
  specifically"). Plan should pick.
- **N5. Test factory injection is unpinned.** Without a
  `TestMailboxFactory` (or some way for tests to inject a factory
  that produces `TestMailbox<T>`), the only available factory is one
  that produces SPSC mailboxes, which the Traps explicitly forbid
  for correctness tests. Plan needs to say "tests use a
  `TestMailboxFactory`."

### Adversarial review

Judgment: passes, with three things worth pinning

- **A1. Iteration bound during spawn.** The current `step()` loop
  iterates `self.entries`. If iteration uses a recomputed bound
  (`for i in 0..self.entries.len()` evaluated each pass), a
  mid-round spawn IS visited this round, contradicting the
  "spawned-child-runs-only-on-later-step" Decision. The runtime
  needs to capture the bound before the round begins. Pin: "the
  step loop captures `entries.len()` before the round begins;
  spawned children appended during the round are not visited until
  the next step."
- **A2. Spawn capacity edge cases.** `SpawnSpec::mailbox_capacity()
  == 0` — programmer error or defined behavior? Mailbox::new(0) on
  the SPSC mailbox at minimum produces something useless and
  probably panics. Pin: "spawn with capacity 0 is programmer error
  and the runtime panics" (matches the slice-002 unknown-target
  precedent) or "the runtime requires `capacity >= 1` and rejects
  0 with a documented panic."
- **A3. Parent-child relationship has no observable surface.** The
  plan says "records parent-child relationships internally" but
  this slice does not execute `RestartChildren`. So the data is
  recorded but unused and untested. Either expose a private
  accessor for tests (`#[cfg(test)]` getter on
  `CurrentRuntime`) so the relationship is verified, or accept
  that the parent-child table is unverified until slice 006 lands
  `RestartChildren` execution. Plan should pick.

What I checked and found defended:

- The slice does not change `tina`; the factory lives in
  `tina-runtime-current`.
- Reply and RestartChildren stay observed-only.
- No supervision policy execution is introduced.
- No cross-shard or Tokio-loop scope creep.

---

## Open Decisions

Six things you (the human) need to weigh in on before this slice is
implementable as written. They are all real, all load-bearing, and
none of them are bookkeeping.

1. **Mailbox factory trait shape.** Recommend: one trait with a
   single generic method `fn create<T: Send + 'static>(&self,
   capacity: usize) -> Box<dyn Mailbox<T>>`. Lives in
   `tina-runtime-current`. Tests provide a `TestMailboxFactory`
   that returns `TestMailbox<T>`. (Round 1 N1, Round 2 N1, Round 2 N5.)

2. **Spawn-execution type machinery.** Recommend: extend `register()`
   so it captures a "spawn adapter" closure of type
   `Box<dyn Fn(Box<dyn Any>, &dyn MailboxFactory) -> RegisteredEntry<S>>`
   keyed off the parent's `type Spawn = SpawnSpec<ChildI>`. The
   adapter knows how to construct the child and its mailbox.
   This is the parallel to the existing `MailboxAdapter` /
   `HandlerAdapter` pattern. (Round 1 N2, Round 2 N2.)

3. **Runtime ingress API signature.** Recommend:
   `pub fn try_send<M: Send + 'static>(&self, address: Address<M>,
   msg: M) -> Result<(), TrySendError<M>>`. Looks up the mailbox by
   `IsolateId`. Same `Full`/`Closed` semantics as direct mailbox
   usage. (Round 2 N3.)

4. **Constructor change.** Recommend: bump
   `CurrentRuntime::new(shard, factory)` and update the existing
   tests. We are pre-launch; breaking the test signature is fine
   and the explicitness is worth it. (Round 2 N4.)

5. **Iteration bound during spawn.** Pin: "the step loop captures
   `entries.len()` before the round begins; spawned children
   appended during the round are not visited until the next step."
   This is what makes the "spawned-child-runs-only-on-later-step"
   property true by construction. (Round 2 A1.)

6. **Parent-child observability.** The slice records parent-child
   relationships but has no path to use them. Either expose a
   `#[cfg(test)]` getter so the relationship is verified by a test
   in this slice, or accept that the table is unverified until slice
   006 (which will execute `RestartChildren`). I recommend the
   `#[cfg(test)]` getter — it makes the recording load-bearing now,
   not later. (Round 2 A3.)

Smaller, optional pins:

- Spec should explicitly note that a parent's `type Spawn` is a
  single type today, so multi-child-type parents are out of scope
  for 005. (Round 1 A1.)
- Spec should say *why* the spawning handler does not receive the
  child address back in this slice (runtime first, ergonomics
  later). (Round 1 A2.)
- Spec should pin spawn capacity edge cases (capacity 0 = programmer
  error). (Round 2 A2.)
- Spec should pin the spawned child's shard (same as parent).
  (Round 1 N4.)

If you take all six, the slice is implementable. If you skip any of
1, 2, 3, or 4, the implementer will be making intent decisions on
the fly that should land in artifacts first.

---

## Round 3: Implementation Review

Artifacts reviewed:

- `tina-runtime-current/src/lib.rs` (312-line delta)
- `tina-runtime-current/tests/spawn_dispatch.rs` (new, 6 tests)
- `tina-runtime-current/tests/local_send_dispatch.rs` (slice-002
  observed-only test narrowed to Reply + RestartChildren)
- `tina-runtime-current/tests/stop_and_abandon.rs` and
  `panic_capture.rs` (constructor-bump pass-through changes only)
- `make verify` (passes; 6 spawn + 6 local_send + 7 panic + 7 stop +
  5 trace-core + 4 loom + clippy clean)

Read against:

- the slice 005 spec diff and plan
- the eight Open Decisions from Round 1 + Round 2 above

### Headline

Implementation is correct, `make verify` is green, and the proof-shape
refinement (dedicated `spawn_dispatch.rs` + slice-002 test narrowed to
Reply + RestartChildren) is a strict improvement.

**Not ready for commit yet** — one real artifact-vs-implementation gap
and three undocumented design choices need to land in the artifacts
first.

### Findings

- **F1. Parent-child recording is missing in the implementation, but
  required by both spec-diff and plan.**
  - Spec-diff "What changes" line 31-32: "Record the parent-child
    relationship internally in `CurrentRuntime` for later supervision
    work."
  - Plan Decisions line 23-24: "CurrentRuntime records parent-child
    relationships internally in spawn order, but this slice does not
    yet execute `RestartChildren`."
  - Implementation: `RegisteredEntry` carries `id`, `stopped`,
    `mailbox`, `handler`. There is no `parent_id` field. There is no
    parent→children map on `CurrentRuntime`. `grep -n "parent\|child"
    src/lib.rs` finds only doc comments referring to mailboxes, never
    an actual parent linkage.

  This is the only true drift. Either the artifacts overcommitted
  ("record it now even though we won't use it until 006") or the
  implementation undershot. Pick one before commit.

- **F2. Spawn type machinery deviates from the approved Decision 2.**
  - Approved (Round 2 N2 / Open Decision 2): "extend `register()` so
    it captures a 'spawn adapter' closure of type
    `Box<dyn Fn(Box<dyn Any>, &dyn MailboxFactory) -> RegisteredEntry<S>>`."
  - Implemented: a sealed-trait pair `IntoErasedSpawn<S, F>` (impl'd
    for `SpawnSpec<I>` and for `Infallible`) plus
    `ErasedSpawn<S, F>::spawn(self: Box<Self>, runtime: &mut
    CurrentRuntime<S, F>) -> IsolateId`. Conversion happens at
    handler-return time inside `HandlerAdapter::handle_boxed`, not at
    `register()` time.

  The trait-based approach is arguably better — no closure stored per
  isolate, type-driven correctness, `Infallible` blanket impl covers
  non-spawning isolates. But it required `#[allow(private_bounds)]`
  on `register` because `IntoErasedSpawn` is private (sealed). That
  is fine semantically (callers can only use `SpawnSpec<I>` or
  `Infallible` as `I::Spawn`, both of which are pre-impl'd) but it is
  a real Rust-flavored choice that was not in the approved plan.

- **F3. New intent: cross-shard ingress panics.**
  `try_send` panics if `address.shard() != self.shard.id()`. This is
  the right call (cross-shard is out of scope; silent misrouting
  would be worse than panic), but it is new semantics not in the
  approved Decision 3.

- **F4. `Send` bound dropped on `MailboxFactory::create<T>` and
  `try_send<M>`.** Approved spec recommended `T: Send + 'static`.
  Implementation uses `T: 'static`. This matches the existing
  slice-002 `register()` bound (no `Send` either), so it is "matches
  prior art" rather than fresh drift — but worth pinning that
  `tina-runtime-current` is intentionally `Send`-free until
  cross-shard work begins.

- **F5. Spawn capacity `0` is silently accepted.** `SpawnSpec::new(
  child, 0)` produces a mailbox of capacity 0; every `try_send` to it
  returns `Full`; the child can never receive. No panic, no error.
  Smaller-pin item from the spec review that was not addressed.

### Proof-shape refinement (good)

- `spawn_dispatch.rs` matches the slice-003 (`stop_and_abandon.rs`)
  and slice-004 (`panic_capture.rs`) pattern. Slice-boundary clarity
  is consistent across phases.
- Narrowing the slice-002
  `non_stop_effects_are_observed_and_traced_without_execution` test
  to Reply + RestartChildren is the right move now that Spawn is no
  longer observed-only. Renamed to
  `reply_and_restart_children_remain_observed_and_not_executed`.
  Accurate, and removes a stale claim.

### Decisions to record before commit

1. **Parent-child recording.** Either:
   - **Add** a `parent: Option<IsolateId>` field on `RegisteredEntry`,
     set it on the spawn path (`None` for root-registered isolates),
     and add a `#[cfg(test)]` getter so the property is verified by
     one test in this slice; **or**
   - **Remove** the recording requirement from spec-diff "What
     changes" line 31-32 and plan Decisions line 23-24, and
     explicitly defer parent-child tracking to slice 006 where
     `RestartChildren` will use it.
   - Recommend the first — Round 2 A3 / Open Decision 6 explicitly
     recommended the `#[cfg(test)]` getter to make the recording
     load-bearing now, not later.

2. **Spawn type machinery — record the actual design.** Update plan
   Decisions to say: "spawn execution uses a sealed
   `IntoErasedSpawn<S, F>` trait pair impl'd for `SpawnSpec<I>` and
   `Infallible`; conversion from `Effect::Spawn(spec)` to
   `ErasedEffect::Spawn(Box<dyn ErasedSpawn<S, F>>)` happens in
   `HandlerAdapter::handle_boxed`. `register()` requires
   `I::Spawn: IntoErasedSpawn<S, F> + 'static` and uses
   `#[allow(private_bounds)]` because the trait is sealed inside the
   runtime crate." This deviation is acceptable, but it should not
   be discovered by the next reviewer in code without a written
   rationale.

3. **Cross-shard ingress.** Add to plan Decisions: "`CurrentRuntime::
   try_send` panics if `address.shard() != self.shard.id()`.
   Cross-shard runtime ingress is out of scope in this slice; silent
   misrouting would be worse than panic." Add an acceptance line and
   a test — the existing
   `runtime_ingress_to_unknown_isolate_still_panics` test covers
   unknown-isolate but not cross-shard.

4. **Spawn capacity edge case.** Pick: capacity 0 is programmer error
   and the runtime panics at spawn time, **or** capacity 0 is
   silently accepted and the child is unreachable. The current
   implementation does the latter. Recommend the former (panic at
   `spawn_isolate` if `mailbox_capacity == 0`) — matches the
   slice-002 unknown-target-panic precedent and avoids dysfunctional
   silent behavior.

Smaller, optional:

- Note in Decisions that `MailboxFactory::create<T>` and `try_send<M>`
  use `T: 'static` (no `Send`), consistent with slice-002 register
  bounds. Pin "tina-runtime-current is intentionally `Send`-free
  until cross-shard work begins."

### Final judgment

**Update artifacts first, then commit.**

The implementation is solid and conforms to the spirit of the slice.
Five test files, 6 new spawn tests, all green. The
`IntoErasedSpawn`/`ErasedSpawn` trait pair is arguably the better
design choice. The proof-shape change is a strict improvement.

But shipping the slice with the parent-child recording silently
dropped from `RegisteredEntry` while the spec-diff and plan still
say it is recorded would put the artifacts out of sync with the code
on day one. That is exactly the kind of intent-debt the IDD process
is meant to prevent. Resolve F1 first, decide F2/F3/F4 explicitly in
artifacts, then commit.

---

## Round 4: Re-review After Session A Response

Artifacts reviewed (current state):

- `tina-runtime-current/src/lib.rs` (new: zero-capacity panic in
  `spawn_isolate`)
- `tina-runtime-current/tests/spawn_dispatch.rs` (now 8 tests, +2:
  cross-shard ingress panic, zero-capacity panic)
- `.intent/phases/005-mariner-local-spawn-dispatch/spec-diff.md`
  (revised)
- `.intent/phases/005-mariner-local-spawn-dispatch/plan.md` (revised)
- `make verify`: green (8 spawn + 6 local_send + 7 panic + 7 stop +
  5 trace-core + 4 loom + clippy + docs)

### Headline

Slice 005 is ready to commit.

F2, F3, F4, F5 are each addressed in artifacts AND in code AND in
tests where applicable. F1 is resolved — but by intent change in the
artifacts, not by the framing codex offered ("stale review text").
That distinction matters; the system worked.

### Findings against Round 3 F1-F5

- **F1 resolved by intent change.** Spec-diff line 50-51 and plan
  line 55 now explicitly defer parent-child bookkeeping to a later
  slice when "restart or supervision behavior makes it externally
  meaningful." That is a legitimate Session A move under SESSION_A.md
  rule 6 ("If the decisions change intended behavior, update
  spec-diff.md"). The artifacts now match the implementation. F1 is
  closed.

  One small framing note for the record: F1 was *not* a reading of an
  earlier or stale artifact version. The Round 3 review quoted line
  numbers from the artifacts as they existed at that moment ("What
  changes" line 31-32 and Decisions line 23-24 explicitly said
  "record parent-child relationships internally"). The artifacts at
  that moment contradicted the implementation. Session A had two
  options: implement the recording, or change the artifacts. Choosing
  the second is fine — and that is what closed F1 — but recording it
  as "the review was reacting to stale text" rewrites the history of
  the gap. The healthier framing for the IDD record is: "F1 was a
  real artifact-vs-implementation gap; we resolved it by deferring
  parent-child bookkeeping in spec-diff and plan rather than by
  adding the recording in code." That is the success story this slice
  should carry forward, because it is what makes adversarial IDD
  load-bearing.

- **F2 addressed in artifacts.** Plan lines 23-27 now describe the
  sealed `IntoErasedSpawn<S, F>` / `ErasedSpawn<S, F>` trait pair,
  the `HandlerAdapter`-time conversion, and the `#[allow(private_-
  bounds)]` on `register`. Spec-diff line 33-35 captures the
  one-sentence intent ("runtime-private sealed spawn erasure"). The
  next reviewer will not have to derive the design from code.

- **F3 addressed in artifacts and tests.** Spec-diff line 60-61 and
  plan line 81 pin the cross-shard ingress panic. Plan Trap line 102
  forbids silent acceptance. New test `runtime_ingress_to_other_-
  shard_still_panics` proves it.

- **F4 addressed in artifacts.** Plan lines 16-17 explicitly justify
  the `T: 'static` (no `Send`) bound on the factory: "tina-runtime-
  current is still a single-threaded runtime and current test
  mailboxes are not `Send`." Plan Trap line 91 forbids forcing the
  bound for future-proofing.

- **F5 addressed in artifacts, code, and tests.** Spec-diff lines
  55-57 and 78-79 pin the zero-capacity panic. Plan lines 52-54 and
  82-83 pin it. Plan Trap line 103 forbids silent acceptance.
  Implementation: `spawn_isolate` panics on `mailbox_capacity == 0`
  before creating the mailbox. New test `spawn_with_zero_capacity_-
  panics_instead_of_creating_unreachable_child` proves it; the
  assertion also confirms no `Spawned` event is emitted on the panic
  path, which is the right trace shape.

### Bonus tightening I noticed

- **Spec-diff line 36-37 + plan line 31 + plan acceptance line 75:**
  "Spawn execution stays on the current shard. The spawned child is
  local to the parent's shard." This was implicit in the Round 1
  review but never explicitly committed. Codex pinned it as part of
  this pass. Good — closes the smaller-pin item from Round 1 N4.
- **Plan line 27:** explicitly notes `Infallible` blanket impl of
  `IntoErasedSpawn` so non-spawning isolates compile without
  ceremony. Good — that is the reason `register()` does not break for
  isolates declaring `type Spawn = Infallible`.

### What I checked and found defended

- Spec-diff and plan are internally consistent on every previously
  drifting axis (parent-child, sealed erasure, cross-shard, capacity,
  Send bound).
- The implementation at `lib.rs` matches the artifacts on every
  pinned property: zero-capacity panic, cross-shard ingress panic,
  cross-shard send panic (was already in slice 002), sealed-trait
  spawn erasure, append-to-registration-order via `register_entry`,
  `entries.len()`-bounded step iteration via the `round_messages`
  snapshot.
- The runtime trace shape is preserved: `MailboxAccepted →
  HandlerStarted → HandlerFinished{Spawn} → Spawned{child_isolate}`
  for the spawn path; `EffectObserved` for Reply / RestartChildren.
- The spawn capacity-0 test asserts no `Spawned` event in the trace
  on the panic path, which is the correct partial-trace shape.
- Determinism preserved by
  `identical_runs_produce_identical_spawn_sequences_and_causal_links`.

### Final judgment

**Ready to commit.**

Suggested commit shape:

- `feat(runtime): execute Effect::Spawn with sealed runtime erasure
  and runtime ingress API` — covers `lib.rs`,
  `tests/spawn_dispatch.rs`, and the constructor migration in
  `local_send_dispatch.rs`, `panic_capture.rs`,
  `stop_and_abandon.rs`.
- `docs(intent): add 005 mariner local-spawn-dispatch phase` —
  covers `.intent/phases/005-mariner-local-spawn-dispatch/`.

After commit: update `CHANGELOG.md` and `commits.txt` per the
SESSION_A end-of-phase loop. `SYSTEM.md` may want one line about
spawn becoming a real runtime behavior; if so, do it as a separate
commit so the phase commits stay clean.
