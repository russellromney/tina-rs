# Reviews and Decisions: 008 Mariner Restartable Child Records

## Round 1: Spec Diff Review

Artifact reviewed:

- `.intent/phases/008-mariner-restartable-child-records/spec-diff.md`

Read against:

- `.intent/SYSTEM.md`
- `.intent/phases/006-mariner-parent-child-lineage/` (parent-child
  lineage groundwork)
- `.intent/phases/007-mariner-address-liveness/spec-diff.md`
  (the just-pinned address-as-incarnation, no-id-reuse model)
- `tina/src/lib.rs:SpawnSpec<I>` (current one-shot spawn payload)
- `tina-runtime-current/src/lib.rs` (current `IntoErasedSpawn` /
  `ErasedSpawn` machinery, lineage_snapshot helper)

### Positive conformance review

Judgment: passes

- **P1.** Right slice in the right place. Records the supervision
  data without executing the supervision policy. Slice 006 added
  parent-child lineage; slice 007 pinned address liveness; this
  slice closes the *third* groundwork piece — restart recipes —
  before slice 009 executes `RestartChildren`. Three small data
  slices feeding one execution slice is the right shape.
- **P2.** The two-spawn-payload distinction is honest.
  `SpawnSpec<I>` stays one-shot; `RestartableSpawnSpec<I>` is
  factory-based. Not every child is restartable, and the type
  system should reflect that. The alternative ("every spawn is
  implicitly restartable, just don't restart some") would silently
  promise something the runtime cannot honor.
- **P3.** `Fn() -> I + 'static` (not `FnOnce`) is correct. Restart
  must produce many replacements over a runtime's lifetime.
- **P4.** "Restart must not clone or reuse the running child
  state" (spec line 36-37) pins the right invariant. Cloning a
  running isolate would carry forward the corruption that caused
  the restart in the first place. Factory-creates-fresh is the
  Erlang/Tina-Odin discipline.
- **P5.** Per-parent child ordinals (not global registration
  order) is the right granularity for future restart semantics
  (e.g., "restart this child's siblings" requires per-parent
  scope).
- **P6.** Child records are private runtime state. No public
  inspection API yet. Continues the slice-006 pattern: build the
  data, prove it via `#[cfg(test)] pub(crate)` snapshots, decide
  the public surface only when a consumer demands it.
- **P7.** Slice correctly self-classifies as escalation-class for
  the public API shape (autonomy note at spec lines 76-81). After
  the type shape is accepted, implementation can run
  autonomously.

### Negative conformance review

Judgment: passes with four things to pin

- **N1. "Generation" appears in the child record schema, but
  slice 007 deferred generation tokens.** Spec line 31 lists
  "shard id, isolate id, and generation" as the child address
  identity stored in the record. Plan line 89 repeats this. But
  slice 007 explicitly says "this slice does not add a generation
  field to `Address<M>`," and its forward gate says "if a future
  runtime ever wants to reuse isolate ids, it must first add a
  generation or equivalent liveness token." Slice 008 introduces
  a generation concept — even just as a record field — without
  passing through that gate. Either:
  1. Remove "generation" from the child record schema for now
     (the no-reuse rule from slice 007 makes it always
     trivially "first incarnation"), or
  2. Explicitly pin that child records store a generation field
     defaulted to a single first-incarnation value, and that
     real generation tokens remain deferred per slice 007.
  *Class: human-escalation.* This touches slice 007's
  just-accepted commitment and could quietly invent semantics
  the user has not signed off on.

- **N2. Non-restartable behavior under RestartChildren is
  unstated in the spec.** Plan acknowledges this at lines 142-144
  ("the next slice must decide whether non-restartable children
  are skipped, rejected with trace, or treated as a runtime
  configuration error"), but the spec itself does not say so. A
  reader of the spec alone might assume non-restartable means
  "silently skipped" or "treated identically to restartable."
  Spec should explicitly say: "this slice records the data;
  restart policy for non-restartable children is the next slice's
  decision and is intentionally undefined here."
  *Class: agent-resolvable.*

- **N3. Factory ergonomics for parameterized isolates is not
  mentioned.** `Fn() -> I + 'static` requires users to capture
  configuration in the closure (e.g.,
  `RestartableSpawnSpec::new(move || Worker::new(tenant_id), 16)`).
  This is idiomatic Rust but spec/plan do not show it. One
  sentence prevents users from trying to pass a non-Fn `I` and
  hitting a confusing trait-bound error.
  *Class: agent-resolvable.*

- **N4. Ordinal stability across restart is unstated.** Plan
  defines per-parent ordinals as "direct child creation order"
  (line 53-54). When slice 009 restarts a child, does the
  replacement get the same ordinal (slot identity) or a new one
  (creation-order identity)? Slice 008 does not decide this, but
  the spec should pin: "ordinal stability across restart is
  slice 009's decision; slice 008 only assigns ordinals at
  initial spawn time."
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, with two angles worth weighing

- **A1. Two public types vs one with two modes.** Adding
  `RestartableSpawnSpec<I>` doubles the spawn-payload surface in
  the trait crate. An alternative shape — making `SpawnSpec<I>`
  itself optionally restartable (e.g., carrying
  `Option<Box<dyn Fn() -> I>>`) — keeps the public API to one
  type and dispatches at field level. Pros of two types: type-
  level distinction makes "this child is restartable" visible at
  the call site; the `Fn() -> I` constraint only applies where
  it's needed (some `I` values may not be `Fn`-constructible).
  Pros of one type: smaller public surface, fewer concepts in the
  user's head. Codex picked two types but the spec/plan does not
  justify the choice over the alternative. Worth at least one
  paragraph defending it before committing.
  *Class: human-escalation.* Affects what tina-rs feels like to
  use and is hard to reverse later.

- **A2. `RestartableSpawnSpec<I>` ships with no externally
  observable behavior beyond `SpawnSpec<I>` in slice 008.** Both
  types call the factory once, both produce a child, both append
  to registration order. The only observable difference in this
  slice is via the `#[cfg(test)] pub(crate)` snapshot helper,
  which external users cannot see. Public users get a new type
  that does nothing different until slice 009 lands. That is
  fine for forward groundwork, but the spec should be explicit:
  "in this slice, `RestartableSpawnSpec<I>` has no observable
  behavior beyond `SpawnSpec<I>`. The difference arrives in
  slice 009 when `RestartChildren` consumes the recorded
  restart recipe." Otherwise users adopting tina-rs at this
  point may pick `RestartableSpawnSpec<I>` expecting some
  immediate benefit and find none.
  *Class: agent-resolvable.*

What I checked and found defended:

- The trait crate (`tina`) gains one new type but no trait
  changes. `Isolate::Spawn` continues to be one associated type
  per isolate (the parent's `type Spawn` decides what spawn
  payloads it can produce).
- Single-cause trace shape preserved. No new trace events.
- Slice 006's lineage and slice 007's address-liveness
  invariants are not weakened: spawned children still get fresh
  ids, parent records persist after stop/panic.
- Public APIs from prior slices are not changed.

---

## Round 2: Plan Review

Artifact reviewed:

- `.intent/phases/008-mariner-restartable-child-records/plan.md`

### Positive conformance review

Judgment: passes

- **P1.** Plan correctly maps spec-diff items to implementation:
  add `RestartableSpawnSpec<I>` in `tina`, extend the runtime's
  private spawn erasure to return metadata, store private child
  records, add `#[cfg(test) pub(crate)]` snapshot helper.
- **P2.** Plan correctly forbids the wrong moves: don't execute
  `RestartChildren`, don't apply restart policies/budgets, don't
  add `tina-supervisor`, don't infer restartability from trace
  events, don't clone running child state, don't make every
  `SpawnSpec<I>` implicitly restartable, don't add public
  inspection APIs, don't add new trace events, don't change
  slice-007 generation semantics, don't add id reuse.
- **P3.** Plan correctly chooses unit tests over integration
  tests for private state, matching the slice-006 precedent.
- **P4.** Plan explicitly names its three known ambiguities at
  lines 134-144 (public API shape, factory ergonomics,
  non-restartable behavior). That kind of self-awareness is
  exactly what plan review is for.
- **P5.** Plan correctly notes that `RestartableSpawnSpec<I>`
  lives in `tina` (the trait crate), keeping spawn payload types
  consistent with `SpawnSpec<I>`'s location.

### Negative conformance review

Judgment: passes with three things to tighten

- **N1. Plan does not pin the new private erasure shape.** Plan
  line 33 says spawn erasure "returns: the new child address
  identity, optional restart recipe metadata, mailbox capacity,
  restartability flag." But the actual return type is unstated.
  Today `ErasedSpawn::spawn(self, runtime, parent) -> IsolateId`.
  After 008 it becomes... a struct, a tuple, a side-effect plus
  IsolateId? Recommend a small private return struct (e.g.,
  `SpawnOutcome { child_id: IsolateId, restart_recipe:
  Option<Box<dyn ErasedRestartRecipe>>, mailbox_capacity: usize,
  restartable: bool }`) and pin it in the plan so the
  implementer doesn't invent the shape mid-implementation.
  *Class: agent-resolvable.*

- **N2. The `ErasedRestartRecipe` trait shape is not pinned.**
  The runtime needs an erased-trait analogue to its existing
  `ErasedSpawn` for the *factory* itself, so the runtime can
  store `Box<dyn ErasedRestartRecipe>` without knowing the
  isolate type at the call site. This is the same sealed-trait
  pattern slice 005 used (`IntoErasedSpawn`/`ErasedSpawn`). Plan
  should sketch it: `trait ErasedRestartRecipe { fn create(&self,
  runtime: &mut CurrentRuntime<S, F>, parent: IsolateId) ->
  IsolateId }` (or similar), implemented privately for
  `RestartableSpawnSpec<I>` via an adapter.
  *Class: agent-resolvable.*

- **N3. Plan does not address the `tina/tests/sputnik_api.rs`
  delta.** Plan line 123 lists it as a likely-changed file, but
  the implication is "doc/import updates only." The plan should
  pin: "no behavioral test changes are required in
  `sputnik_api.rs`; only API-surface tests for the new
  `RestartableSpawnSpec<I>` type are added." Otherwise it is
  unclear whether existing one-shot spawn tests stay intact.
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, with one structural risk

- **A1.** `RestartableSpawnSpec<I>` plus a private
  `ErasedRestartRecipe` plus a private `ChildRecord` table plus
  per-parent ordinal counters plus extended spawn erasure metadata
  is a meaningfully larger design than slice 006 or 007. It is
  still smaller than slice 005, but the plan should be honest
  about the surface area: ~60-100 lines of runtime code plus the
  new public type plus tests. Worth flagging that this slice is
  "medium" by the slice 001-007 standard and budgeting reviewer
  attention accordingly.
  *Class: agent-resolvable* (just spec/plan honesty).

What I checked and found defended:

- Plan does not extend `tina` beyond one new public type.
- Runtime extension stays in `tina-runtime-current` private
  surface.
- No `tina-supervisor` work, no policy execution.
- Test pattern (unit tests for private state) follows slice 006.
- Existing trace shape and existing public APIs are preserved.

---

## Open Decisions

Three human-escalation decisions before slice 008 is ready to
implement. Two are real semantic commitments; one is a public-API
shape decision.

1. **Two spawn-payload types vs one with two modes.** Codex
   proposes a new public `RestartableSpawnSpec<I>` alongside the
   existing `SpawnSpec<I>`. The alternative is to extend
   `SpawnSpec<I>` itself to optionally carry a factory. Two
   types makes the distinction visible at the call site and
   confines the `Fn() -> I` bound to where it's needed; one type
   keeps the public surface smaller but pushes the
   restartable-vs-not distinction into a field check. Decide
   which API shape ships. (Round 1 A1.)

2. **"Generation" in child records vs slice 007's deferred
   generation tokens.** Slice 008's child record schema
   (spec line 31, plan line 89) lists "shard id, isolate id, and
   generation" as the child's address identity. Slice 007
   explicitly deferred generation tokens. Either:
   (a) remove "generation" from the child record schema in slice
   008, since the no-reuse invariant from slice 007 makes it
   trivially "first incarnation" everywhere, or
   (b) explicitly pin that child records store a generation
   field that is always a default first-incarnation value in
   slice 008, with real generation tokens still deferred. Either
   resolution is fine, but the slice cannot ship without picking
   one. (Round 1 N1.)

3. **Slice size honesty.** Slice 008 is meaningfully larger
   than 006 or 007 (new public type + new private erasure trait
   + new private record table + new ordinal counters + extended
   spawn metadata). Still smaller than 005, but worth being
   honest in the plan that this is a medium-class slice rather
   than a small one, so reviewer attention can be budgeted. Not
   blocking — just bookkeeping. (Round 2 A1.)

Agent-resolvable findings codex can fold into the slice
autonomously without re-escalation:

- Pin in spec: "non-restartable behavior under `RestartChildren`
  is intentionally undefined in this slice; slice 009 decides
  whether non-restartable children are skipped, rejected, or
  treated as configuration error." (Round 1 N2.)
- Pin in spec or plan: factory ergonomics example for
  parameterized isolates (`move || Worker::new(tenant_id)`).
  (Round 1 N3.)
- Pin in spec: "ordinal stability across restart is slice 009's
  decision; slice 008 only assigns ordinals at initial spawn
  time." (Round 1 N4.)
- Pin in spec: "in slice 008, `RestartableSpawnSpec<I>` has no
  observable behavior beyond `SpawnSpec<I>` for external users;
  the difference arrives in slice 009." (Round 1 A2.)
- Pin in plan: the private spawn-erasure return shape
  (recommend a small struct: `SpawnOutcome { child_id,
  restart_recipe, mailbox_capacity, restartable }`).
  (Round 2 N1.)
- Pin in plan: the `ErasedRestartRecipe` trait shape
  (sealed-trait pattern paralleling `ErasedSpawn`). (Round 2 N2.)
- Pin in plan: `sputnik_api.rs` changes are API-surface tests
  for the new type only; no behavioral test changes. (Round 2
  N3.)

---

## Round 3: Re-review After Session A Response

Artifacts reviewed (current state):

- `.intent/phases/008-mariner-restartable-child-records/spec-diff.md`
  (revised)
- `.intent/phases/008-mariner-restartable-child-records/plan.md`
  (revised, including new `Private runtime shapes` section with
  code sketches)
- `.intent/phases/007-mariner-address-liveness/spec-diff.md`
  (Session A rewrote slice 007 to commit to address generation
  tokens; this resolves Round 1 N1 from above)

### Positive conformance review

Judgment: passes — ready to implement

- **P1.** Every Round 1 and Round 2 finding is now addressed in
  artifacts:
  - **Round 1 N1** (generation in child records vs slice 007's
    deferral) is moot: Session A rewrote slice 007 to commit to
    address generation tokens (shard id + isolate id + generation).
    Slice 008's child record correctly references that as
    `AddressGeneration`. No drift.
  - **Round 1 N2** (non-restartable behavior under RestartChildren)
    pinned in spec line 63-65: slice 009 decides.
  - **Round 1 N3** (factory ergonomics for parameterized isolates)
    pinned in spec line 54-55 and plan line 56-57 with the
    `move || Worker::new(tenant_id)` example.
  - **Round 1 N4** (ordinal stability across restart) pinned in
    spec line 66-67 and plan line 70-71: slice 009 decides.
  - **Round 1 A1** (two types vs one with mode flag) justified in
    spec line 18-22 and plan line 50-53: keeps one-shot spawn
    simple, makes restartability visible at the call site, confines
    the repeatable `Fn() -> I` requirement to where it's needed.
  - **Round 1 A2** (no observable behavior in slice 008) pinned in
    spec line 51-53, plan line 63-65, and Trap line 175-176.
  - **Round 2 N1** (private spawn-erasure return shape) pinned in
    plan's new `Private runtime shapes` section (line 80-94) with
    a `SpawnOutcome { child, mailbox_capacity, restart_recipe }`
    struct sketch.
  - **Round 2 N2** (ErasedRestartRecipe trait shape) pinned in
    the same section (line 100-112) with a sealed-trait sketch
    parallel to the existing `ErasedSpawn` machinery.
  - **Round 2 N3** (`sputnik_api.rs` is API-surface only) pinned
    in plan line 77-78 and acceptance line 156.
  - **Round 2 A1** (slice size honesty) pinned in spec line 29-31
    and plan line 21-23 as "medium-class slice."

- **P2.** The new `Private runtime shapes` section in the plan is a
  meaningful improvement. Code sketches for `SpawnOutcome` and
  `ErasedRestartRecipe` make the implementation shape concrete
  before the implementer starts. The `restartable` flag is
  derived from `restart_recipe.is_some()` rather than being a
  separate field — clean.

- **P3.** The slice-007 integration is clean: child records carry
  the full slice-007 address identity (shard + isolate +
  AddressGeneration), no new generation semantics are invented
  in 008.

- **P4.** "RestartableSpawnSpec<I> has no externally observable
  runtime behavior beyond SpawnSpec<I> in slice 008" is now
  pinned in the spec In Plain English, spec What Changes, plan
  Phase Decisions, and plan Traps. Hard to misread.

### Negative conformance review

Judgment: passes with two small bookkeeping items

- **N1. Stale "Ambiguities noticed during planning" section.**
  Plan lines 198-208 still list three ambiguities (public API
  shape, factory ergonomics, non-restartable behavior). All three
  are resolved in the updated artifacts. The whole section could
  be removed or replaced with a one-line note saying "the three
  ambiguities listed in earlier drafts are now resolved by the
  Phase Decisions section."
  *Class: agent-resolvable.*

- **N2. Acceptance vs spec field-list mismatch.** Plan acceptance
  line 148 lists child record fields as "parent id, child isolate
  id, child generation, shard id, child ordinal, mailbox
  capacity, and restartability." Spec line 39-46 uses different
  language ("the child address identity from slice 007: shard
  id, isolate id, and AddressGeneration"). Same fields, but the
  spec is more precise about referencing the slice-007 type
  name. Worth aligning: use the same field-list language in both
  places, with the slice-007 type name (`AddressGeneration`)
  rather than the generic word "generation."
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, with three small implementation notes

- **A1. `Fn` not `FnMut`.** The factory bound is `Fn() -> I +
  'static`. That means the closure cannot mutate captured state
  directly. For "every restart should advance a counter" use
  cases, users need interior mutability (`Arc<Mutex<u32>>` or
  similar). This is the more conservative choice (`FnMut` would
  introduce thread-safety questions for cross-shard restart
  later), but the spec/plan does not mention it. One sentence:
  "the factory is `Fn`, not `FnMut`; mutable state across
  restarts requires interior mutability."
  *Class: agent-resolvable.*

- **A2. `IntoErasedSpawn` blanket impls.** Slice 005 added
  blanket impls of `IntoErasedSpawn` for `SpawnSpec<I>` and
  `Infallible`. Slice 008 will need a third blanket impl for
  `RestartableSpawnSpec<I>`. Plan does not mention this directly
  but it is an obvious consequence of plan line 133-134
  ("implement erased spawn for both `SpawnSpec<I>` and
  `RestartableSpawnSpec<I>`"). Worth noting explicitly so the
  implementer doesn't miss the existing pattern.
  *Class: agent-resolvable.*

- **A3. Slice cohesion check.** The slice contains three tightly
  coupled changes: public type, private record table, private
  restart-recipe erasure. Could it be split into 008a/008b/008c?
  No — the public type is meaningless without something to
  consume it, and the private record without the public type is
  dead state. Slice 005 set the precedent for medium-size
  tightly-coupled slices and worked. The "medium-class" honesty
  in the spec/plan is sufficient.
  *Class: defended (no action needed).*

What I checked and found defended:

- Single-cause trace shape preserved.
- No new public API surface beyond `RestartableSpawnSpec<I>`.
- `tina-supervisor` not introduced.
- Slice 006's lineage and slice 007's address-liveness invariants
  hold unchanged.
- Existing integration tests should keep passing without
  behavior changes.
- Plan correctly forbids: cloning running child state, inferring
  restartability from trace events, making every `SpawnSpec<I>`
  implicitly restartable, deciding non-restartable
  RestartChildren behavior, deciding ordinal stability,
  expanding public APIs beyond the new type.

### Autonomy assessment

Slice 008 is now ready to implement under autonomous bucket
mode. The two human-escalation findings from Round 1 (two-types-
vs-one, generation-vs-deferred-from-007) are both resolved:
codex committed to two types with justification; slice 007 was
rewritten to include generation tokens.

The remaining findings (Round 3 N1, N2, A1, A2) are all
agent-resolvable bookkeeping that codex can fold into the slice
during implementation without re-escalation.

---

## Open Decisions

No human-escalation decisions remain. Slice 008 is ready to
implement under autonomous bucket mode.

The Round 3 findings are all agent-resolvable cleanup that
codex can fold in during implementation:

- Drop the stale "Ambiguities noticed during planning" section
  in plan.md or replace it with a one-line note that the
  ambiguities were resolved during plan revision. (N1)
- Align the spec and plan child-record field-list language; use
  the slice-007 type name `AddressGeneration` consistently.
  (N2)
- Note that the factory bound is `Fn`, not `FnMut`; mutable
  state across restarts requires interior mutability. (A1)
- Add the third `IntoErasedSpawn` blanket impl for
  `RestartableSpawnSpec<I>` (parallel to the existing
  `SpawnSpec<I>` and `Infallible` impls). (A2)

---

## Round 4: Implementation Review

Artifacts reviewed:

- `tina/src/lib.rs` (+60 lines: new `RestartableSpawnSpec<I>` public
  type with `Debug`, `new`, `mailbox_capacity`, `into_parts`)
- `tina/tests/sputnik_api.rs` (+30 lines: new API-surface test +
  `Worker::new(tenant_id)` parameterization)
- `tina-runtime-current/src/lib.rs` (+386 lines: `SpawnOutcome`,
  `ChildRecord`, `ChildRecordSnapshot`, `ErasedRestartRecipe`,
  `RestartableSpawnAdapter`, `child_record_snapshot()` helper,
  `record_child()` helper, ordinal counter, plus 5 new unit tests
  and augmentations to existing tests)
- `README.md` and `ROADMAP.md` (factual updates: project name
  spelling Banugo → Mbanugo; "current evidence" snapshot table)
- `make verify`: passes (11 runtime unit tests + 7 panic + 7
  stop + 7 lineage + 7 address-liveness + 6 send + 5 trace-core +
  5 sputnik + 7 supervision + 5 spsc + 4 loom + 4 drop + 2
  allocation + clippy + docs)

### Headline

Slice 008 is correct, ready to commit.

All the approved Round 1-3 decisions land in code. The
`SpawnOutcome` and `ErasedRestartRecipe` shapes match the plan
sketches exactly. Two-types public API committed. Generation
tokens from slice 007 are used end-to-end. 11 unit tests prove
child records via whole-state `child_record_snapshot()`
assertions, including the load-bearing
"RestartChildren-doesn't-consult-records-yet" regression catch.

Three small agent-resolvable bookkeeping items remain that codex
can fold in before commit; none are blockers.

### Findings

- **F1.** All approved Round 1-3 decisions land:
  - **Two public types** committed in `tina/src/lib.rs` —
    `RestartableSpawnSpec<I>` is a separate sibling to
    `SpawnSpec<I>`, with the `Fn() -> I + 'static` factory bound
    confined to where it's needed.
  - **`SpawnOutcome` shape** matches plan sketch exactly:
    `{ child: RegisteredAddress, mailbox_capacity, restart_recipe:
    Option<Box<dyn ErasedRestartRecipe<S,F>>> }`. `restartable` is
    derived from `restart_recipe.is_some()`, not stored.
  - **`ErasedRestartRecipe<S,F>` sealed trait** matches plan
    sketch with `fn create(...) -> SpawnOutcome<S,F>`. The trait
    is implemented for `RestartableSpawnAdapter<I,Outbound>` and
    stored alongside the spawn outcome; `#[allow(dead_code)]` on
    `create` is honest — slice 009 will call it.
  - **`AddressGeneration` from slice 007** is the child-record
    generation field. No new generation semantics invented.
  - **`IntoErasedSpawn` blanket impl** for
    `RestartableSpawnSpec<I>` added (Round 3 A2 fixed).
  - **Per-parent ordinal counter** computed correctly: filter
    child_records by parent, count = next ordinal. O(n) per
    spawn — fine at this scale.

- **F2.** `RestartableSpawnAdapter` cleanly implements both
  `ErasedSpawn` (for initial spawn) and `ErasedRestartRecipe`
  (for slice 009's future restart). The same boxed adapter
  instance serves both roles — the `spawn` impl creates the
  initial isolate via the factory, then moves `self` into
  `outcome.restart_recipe = Some(self)`. Efficient and obvious.

- **F3. Test coverage is comprehensive and whole-state.** 11
  unit tests:
  - `root_registered_isolates_have_no_parent` (now also asserts
    `child_record_snapshot() == Vec::new()`)
  - `one_shot_spawn_records_non_restartable_child_metadata`
  - `restartable_spawn_records_restartable_child_metadata_and_-
    calls_factory_once` (asserts `factory_calls.get() == 1`)
  - `per_parent_child_ordinals_increment_by_direct_spawn_order`
    (two spawns from same parent → ordinals 0, 1)
  - `nested_spawns_record_direct_parent_edges` (root→child→
    grandchild produces three records with correct parent edges)
  - `child_lineage_survives_when_parent_stops` (record + lineage)
  - `child_lineage_survives_when_parent_panics` (record + lineage)
  - `child_record_survives_when_child_stops_or_panics` (records
    survive both transitions for the *children* themselves)
  - `restart_children_remains_observed_only_after_lineage_lands`
    (kept from slice 006)
  - `restart_children_does_not_consult_child_records_or_call_-
    restart_factories_yet` (NEW: asserts factory_calls stays at
    1 after RestartChildren is sent, plus the EffectObserved
    trace event — this will *intentionally* break when slice 009
    lands)
  - `identical_runs_produce_identical_trace_and_lineage`
    (extended to compare `child_record_snapshot()` too)

- **F4. The factory ergonomics example works in practice.** The
  `RestartableRootIsolate` test fixture uses
  `move || { factory_calls.set(factory_calls.get() + 1);
  ChildIsolate { ... } }` — capturing `factory_calls` (an
  `Rc<Cell<usize>>`) by move, mutating via interior mutability.
  This is exactly the pattern that adversarial Round 3 A1 noted
  ("Fn not FnMut; mutable state across restarts requires
  interior mutability"). The implementation works, but the spec
  doc note for this is still missing.

- **F5. README/ROADMAP touch is wider than plan said.** Plan
  Areas-not-touched (line 196) lists "README/ROADMAP changes
  outside the already-pending docs cleanup." This commit
  includes README updates (Banugo → Mbanugo, runtime-status
  refresh) and ROADMAP updates (current-evidence snapshot
  table). These are factual updates rather than slice-008
  semantic changes — the README is being honest about what's
  shipped, the ROADMAP table reflects current state — but they
  are still wider than the plan's "files likely to change" list
  (line 184-188). Worth deciding whether to keep them in this
  commit or split them out.
  *Class: agent-resolvable.*

### Negative findings (small bookkeeping)

- **N1. Three Round 3 agent-resolvable items not folded.**
  - Round 3 N1: stale "Ambiguities noticed during planning"
    section in plan.md still lists three resolved ambiguities.
  - Round 3 N2: plan acceptance line 148 still says "child
    generation" rather than `AddressGeneration` (spec uses the
    type name; plan should match).
  - Round 3 A1: spec/plan doesn't pin "the factory is `Fn`, not
    `FnMut`; mutable state across restarts requires interior
    mutability." The implementation does the right thing, but
    the doc note is missing.
  - Round 3 A2: ✓ Done (third `IntoErasedSpawn` blanket impl for
    `RestartableSpawnSpec<I>` added).
  *Class: agent-resolvable.*

- **N2. README/ROADMAP scope.** See F5 above.
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, no defects worth blocking on

- The same boxed adapter serving both initial-spawn execution
  and restart-recipe storage is sound. The factory is `Fn` (not
  FnOnce), so calling it once for initial spawn doesn't prevent
  calling it again later via the restart-recipe path.
- Per-parent ordinal counting via filter+count is O(n) per
  spawn. At slice-008 scale (test workloads with a few children
  per parent), this is fine. If a future slice has parents with
  thousands of children, an O(1) per-parent counter would be
  worth doing — but not yet.
- `register_entry` returns `RegisteredAddress` which now carries
  `shard: ShardId`. The public `register()` method's
  `Address::new_with_generation(...)` call now uses
  `address.shard` instead of `self.shard.id()` — same value,
  cleaner data flow. No behavior change.
- `ChildRecordSnapshot` is `pub(crate)` test-only; no public
  API surface is exposed for child records.
- `RestartableSpawnSpec<I>` has a custom `Debug` impl using
  `finish_non_exhaustive()` because `Fn() -> I` closures don't
  implement `Debug`. Pragmatic; this is the right move.

### Final judgment

**Ready to commit after three small agent-resolvable items
fold in.**

The implementation is clean, the tests are sharp, and the
slice-009 regression catch (RestartChildren-doesn't-call-factory)
is correctly load-bearing.

Suggested commit shape:

- `feat(runtime): record restartable child metadata at spawn
  time` — covers `tina/src/lib.rs`, `tina/tests/sputnik_api.rs`,
  `tina-runtime-current/src/lib.rs`.
- `docs: refresh README and ROADMAP to reflect slices 005-007
  status` — covers `README.md` and `ROADMAP.md` separately if
  you want the docs split per F5/N2; or fold them into the feat
  commit if you accept the broader scope.
- `docs(intent): add 008 mariner restartable-child-records
  phase` — covers `.intent/phases/008-mariner-restartable-child-
  records/`.

Before commit, codex can fold these in autonomously:

- Drop or one-line the stale "Ambiguities noticed during
  planning" section in plan.md.
- Use `AddressGeneration` consistently in plan acceptance
  line 148.
- Add a one-line note in spec or plan: "the factory is `Fn`,
  not `FnMut`; mutable state across restarts requires interior
  mutability (e.g., `Rc<Cell<...>>` for single-thread, or
  `Arc<Mutex<...>>` for cross-thread later)."

### Autonomy assessment

No human-escalation findings remain. The slice can complete
under autonomous bucket mode: codex folds the three remaining
bookkeeping items, commits, and updates `CHANGELOG.md` /
`commits.txt`.

---

## Round 3: Decision Pass

Response to Round 1/2 findings:

- **A1 accepted, recommended API shape retained.** Keep two public payload
  types: `SpawnSpec<I>` for one-shot concrete child spawn and
  `RestartableSpawnSpec<I>` for factory-backed restartable child spawn. The
  spec and plan now justify this as a call-site clarity choice and as a way to
  avoid imposing repeatable factory semantics on non-restartable children.
  This remains the one public API-shape pause gate for the user to approve
  before implementation.
- **N1 corrected against current slice-007 intent.** The review's generation
  concern was based on an earlier 007 premise. Slice 007 now includes
  `AddressGeneration` in `Address<M>` identity and runtime lookup. The 008
  artifacts now say child records store the complete slice-007 address identity:
  shard id, isolate id, and `AddressGeneration`.
- **A2 accepted.** The spec now says `RestartableSpawnSpec<I>` has no externally
  observable runtime behavior beyond `SpawnSpec<I>` in slice 008. The
  difference is private recorded restartability for slice 009.
- **A1 size-honesty finding accepted.** The spec and plan now classify 008 as a
  medium-class slice by the slice 001-007 standard.
- **N2/N4 accepted.** The spec and plan now explicitly defer non-restartable
  `RestartChildren` behavior and ordinal stability across restart to slice 009.
- **N3 accepted.** The spec and plan now show the parameterized-isolate factory
  pattern: `move || Worker::new(tenant_id)`.
- **Round 2 N1/N2 accepted.** The plan now pins a private `SpawnOutcome<S, F>`
  return shape and a sealed private `ErasedRestartRecipe<S, F>` trait parallel
  to the existing erased spawn machinery.
- **Round 2 N3 accepted.** The plan now pins `tina/tests/sputnik_api.rs` changes
  as API-surface checks only, not behavior changes.

Implementation status after this pass: ready once the user accepts the two-type
public API shape.

---

## Round 4: Implementation Pass

Implementation result: passes.

- Added public `RestartableSpawnSpec<I>` in `tina` as a distinct factory-backed
  spawn payload beside one-shot `SpawnSpec<I>`.
- Extended `CurrentRuntime`'s private spawn path to return `SpawnOutcome` and
  store private child records in spawn order.
- Child records now capture parent isolate id, child shard/id/generation,
  per-parent child ordinal, mailbox capacity, and restartability.
- Added private `ErasedRestartRecipe` storage for restartable children without
  executing restart behavior.
- Kept `RestartChildren` observed-only; tests prove it does not consult child
  records or call restart factories in slice 008.
- Added API-surface coverage in `tina/tests/sputnik_api.rs` for naming and
  constructing `RestartableSpawnSpec<I>`.
- Added runtime unit tests for root records, one-shot records, restartable
  records, factory call count, direct-parent ordinals, nested records,
  persistence after stop/panic, and deterministic trace/lineage/child-record
  snapshots.

Verification:

- `make fmt`
- `make test`
- `make verify`

All passed.

---

## Round 5: Bookkeeping Pass

Response to final review bookkeeping:

- Replaced the stale "Ambiguities noticed during planning" section in
  `plan.md` with a short planning-resolution note. The resolved decisions now
  live in Phase decisions and Traps.
- Updated the acceptance wording to use `AddressGeneration` explicitly instead
  of the looser phrase "child generation."
- Added the `Fn`, not `FnMut`, note to the plan and to the public
  `RestartableSpawnSpec<I>` docs. Mutable state across restarts requires
  interior mutability.

Verification after this bookkeeping pass:

- `git diff --check`
