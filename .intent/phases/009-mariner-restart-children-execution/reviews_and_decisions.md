# Reviews and Decisions: 009 Mariner RestartChildren Execution

## Round 1: Spec Diff Review

Artifact reviewed:

- `.intent/phases/009-mariner-restart-children-execution/spec-diff.md`

Read against:

- `.intent/SYSTEM.md`
- `.intent/phases/006-mariner-parent-child-lineage/`
  (parent-child lineage groundwork)
- `.intent/phases/007-mariner-address-liveness/spec-diff.md`
  (address-as-incarnation, AddressGeneration, no-id-reuse)
- `.intent/phases/008-mariner-restartable-child-records/spec-diff.md`
  (the just-shipped restart-recipe storage; this slice consumes it)
- `tina-runtime-current/src/lib.rs` (current ChildRecord,
  ErasedRestartRecipe, ErasedEffect::RestartChildren arm)
- `tina-runtime-current/tests/` (existing 7-file integration test
  layout including the just-added `runtime_properties.rs`)

### Positive conformance review

Judgment: passes

- **P1.** Right slice in the right place. Three data slices
  (006 lineage, 007 address liveness, 008 restartable records) fed
  this execution slice. The payoff arrives here.
- **P2.** Trace vocabulary is well-designed:
  Attempted/Skipped/Completed split lets replay tools reason
  about both successful and skipped cases without inventing a
  rejection-shape variant. Each event carries old-and-new
  identity plus ordinal.
- **P3.** "Direct restartable children" is the narrowest
  meaningful semantics. No grandchild recursion, no policy, no
  budget. The slice executes the dispatch — supervision concerns
  defer to a later slice.
- **P4.** Skip-with-trace for non-restartable children is the
  right resolution to slice 008's deferred decision. Visible,
  non-panicking, replay-friendly.
- **P5.** Stable child ordinals across restart. Slot identity,
  not creation-order identity. This is the correct choice
  because future supervisor "restart child #2" semantics need
  slot identity to be load-bearing.
- **P6.** Old-child stop on restart reuses the slice 003
  stop-and-abandon path. Restart doesn't invent a parallel
  "stop-but-different" path.
- **P7.** Replacement children scheduled to run only on a later
  `step()`. Preserves slice 005's bounded-iteration invariant.
- **P8.** Factory panic propagates rather than becoming
  `HandlerPanicked`. Right call — the factory isn't running
  inside a handler context, so the slice 004 catch boundary
  doesn't apply.
- **P9.** Property test extension is in scope. Generated
  histories will exercise restart shapes too.
- **P10.** Old addresses become `Closed` via inherited slice 007
  semantics, not a new mechanism. Clean continuity.

### Negative conformance review

Judgment: passes with three things to pin

- **N1. No-children edge case is unstated.** Spec does not say
  what happens when a parent returns `RestartChildren` and has
  zero direct children (or zero restartable direct children).
  Implementation will naturally emit no restart events — the
  walk finds nothing — but a reader could assume otherwise.
  Pin: "RestartChildren on a parent with no direct children
  emits `HandlerFinished{RestartChildren}` and nothing else; no
  restart event subtree is created."
  *Class: agent-resolvable.*

- **N2. Spawn-this-round-then-restart edge case is unstated.**
  If isolate A spawns a child for parent P in round N (via a
  cross-isolate spawn pattern), and in the same round N parent
  P returns `RestartChildren`, the just-spawned child IS in
  `child_records` and will be restarted before it ever ran.
  This is a consistent behavior but surprising. Spec should
  acknowledge it: "RestartChildren walks all current
  child_records for the parent, including children whose own
  step has not yet run; their restart proceeds via the
  not-yet-running stop path."
  *Class: agent-resolvable.*

- **N3. Property test file scope.** Plan refers to
  `runtime_properties.rs` (extend, not create — the file exists
  from a prior slice). Spec should mention it explicitly so the
  proof surface is clear: "extending generated-history property
  tests is part of slice 009 acceptance."
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, with one structural change worth being
explicit about

- **A1. Slice 009 introduces trace branching.** Until now, the
  runtime trace has been linear chains: each event's `caused_by`
  points to one parent, and each event has at most one
  downstream successor. The data model already permits multiple
  events to share one `caused_by`, but no slice has used it.
  Slice 009 changes this:
  ```
  RestartChildAttempted
  ├── IsolateStopped     (when the old child was running)
  │   └── MessageAbandoned*
  └── RestartChildCompleted
  ```
  A single `RestartChildAttempted` is the cause of *both*
  `IsolateStopped` and `RestartChildCompleted` in the
  running-old-child case. This is a real semantic change in
  trace shape that downstream replay tools, simulators, and
  property tests will need to handle. Spec should explicitly
  acknowledge: "this slice introduces trace branching: a single
  runtime event may be the cause of multiple direct downstream
  events. Replay tooling that previously assumed linear chains
  must be updated."
  *Class: human-escalation.* Affects every future trace consumer
  and is the slice's most architecturally meaningful commitment.

- **A2. Registry growth scales with restart count, not just
  spawn count.** Slice 007 committed to no-id-reuse + monotonic
  registry growth. Slice 009 makes that growth coupled to
  restart frequency: a single restartable child slot that
  restarts N times produces N+1 registry entries. For
  long-running runtimes with frequent restarts, this is a real
  footprint concern. Slice 007 already accepted monotonic
  growth, but slice 009 amplifies it. Worth flagging in the
  spec: "registry growth now scales with total restart count
  plus initial spawn count; long-running supervised workloads
  will see registry size grow with restart frequency."
  *Class: agent-resolvable.* This is a clarification of
  slice 007's existing commitment, not a new commitment.

What I checked and found defended:

- Cross-isolate same-round restart scenarios (A spawns child
  for P, P restarts in same round) work via the
  bounded-iteration rule from slice 005: just-spawned children
  exist in `child_records` and will be visited by the walk.
- Two parents both returning `RestartChildren` in the same
  round process sequentially in registration order (slice 002
  invariant). No interaction between their walks.
- Same-round abandonment of old-child mailbox messages
  preserves slice 003 FIFO order; trace events for the old
  child's stop happen mid-restart, before the
  `RestartChildCompleted` for that child.
- Old/new generation on replacement: each replacement gets
  a fresh isolate id (slice 007 monotonic) and that fresh id
  has its own initial `AddressGeneration`. Generation is
  per-isolate-id, not per-child-slot. Trivially correct because
  ids never reuse.
- Single-cause-per-event invariant preserved: each event still
  has at most one `caused_by`. The branching is in how many
  events share a cause, not in any event having multiple
  causes.
- Slice 008's `restart_children_does_not_consult_child_records_-
  or_call_restart_factories_yet` test will *intentionally*
  break when slice 009 lands. That regression catch fires
  exactly when expected.

---

## Round 2: Plan Review

Artifact reviewed:

- `.intent/phases/009-mariner-restart-children-execution/plan.md`

### Positive conformance review

Judgment: passes

- **P1.** Plan correctly maps spec-diff items to implementation:
  three new event variants, child-record-by-parent walk in
  ordinal order, in-place record update preserving ordinal.
- **P2.** "Trace vocabulary" section pins exact event shapes
  with field types. `RestartSkippedReason::NotRestartable` is
  named explicitly (and is the only variant for now — extension
  to other skip reasons is a future-slice concern).
- **P3.** "Cause chain" subsection (lines 99-106) explicitly
  shows the branching trace shape, even though the spec
  doesn't (Round 1 A1).
- **P4.** Phase decisions resolve all three slice-008 deferrals
  (non-restartable behavior, ordinal stability, scope) in
  writing. No surprise semantics in the implementation.
- **P5.** Traps are sharp and mirror the spec's What-doesn't-
  change list. Notable additions: "Do not reuse isolate ids,"
  "Do not allocate a new child ordinal on restart," "Do not
  catch restart factory panics as handler panics," "Do not
  reconstruct parent-child state from trace events." Each
  forbids a plausible-but-wrong implementation choice.

### Negative conformance review

Judgment: passes with two small things to tighten

- **N1. Plan does not pin the in-place record update mechanism.**
  Plan line 41-42 says "update the child record to the fresh
  address identity while preserving parent and child ordinal."
  The implementation could:
  1. mutate the existing `ChildRecord` in place, replacing
     the `child` and `restart_recipe` fields, OR
  2. swap in a fresh record with the same parent and ordinal,
     dropping the old one.
  Both produce the same observable state. (1) preserves the
  slot's vec index; (2) might not. Recommend pinning (1):
  "the existing `ChildRecord` is mutated in place; its vec
  index is preserved." This matters for any future code that
  caches indices into `child_records`.
  *Class: agent-resolvable.*

- **N2. Plan does not address restart-recipe ownership.** The
  current `ChildRecord.restart_recipe` is `Option<Box<dyn
  ErasedRestartRecipe<S, F>>>`. To call `create()` on it during
  restart, the code needs to either:
  1. take `&self` (the trait method allows this), keeping the
     recipe alive across restarts, OR
  2. take `Box<self>` and replace it with a fresh recipe (not
     possible since the recipe is the source of fresh isolates).
  Plan should pin (1): "the restart recipe is kept alive via
  `&self.restart_recipe.create(...)`; the same recipe Box
  serves multiple restarts." This is implicit in the
  `ErasedRestartRecipe::create` signature (`&self`, not
  `self: Box<Self>`) but worth pinning to prevent confusion.
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, with one note

- **A1.** Plan line 175-178 says "maybe a new
  `tina-runtime-current/tests/restart_children.rs`" — using
  "maybe" is fine for "we'll decide based on test
  organization," but the slice's whole-trace assertions for
  restart shapes really do need somewhere to live. Either:
  - integration test file `tests/restart_children.rs` (parallel
    to the slice 003-007 pattern), or
  - unit tests in `src/lib.rs` (parallel to slice 006/008's
    crate-private patterns since restart visits private state).
  Plan should commit to one. The slice 008 unit-test pattern
  worked well for child-record snapshots — slice 009 might want
  to extend the same `mod tests` block rather than a new
  integration file.
  *Class: agent-resolvable.*

What I checked and found defended:

- No public API changes. `RestartableSpawnSpec<I>` and
  `SpawnSpec<I>` are unchanged. New trace events are private
  runtime vocabulary (no `pub` exports beyond the existing
  `RuntimeEventKind` enum, which already exports its variants).
- Plan correctly forbids `tina-supervisor`, restart policies,
  restart budgets, recursive restart, public child-record
  inspection APIs.
- `make miri` mentioned in acceptance line 145 — slice 009
  introduces no unsafe code, so miri should pass trivially.
  Worth confirming the project already runs miri (slice 005
  added similar).
- Plan acknowledges restart factories are called outside a
  handler (line 62-63). Same call site as initial spawn
  (slice 008's `RestartableSpawnAdapter::spawn`), so factory
  panic semantics are consistent.

---

## Open Decisions

One human-escalation decision before slice 009 is implementable
as written. Everything else is agent-resolvable bookkeeping.

1. **Trace branching is now part of the trace contract.** Until
   slice 009, the runtime trace has been linear chains: each
   event's cause points to one parent, and each event has at
   most one downstream successor. Slice 009 changes that. A
   single `RestartChildAttempted` is the cause of *both*
   `IsolateStopped` (when the old child was running) and
   `RestartChildCompleted`. The data model already permits
   this, but downstream replay tools, simulators, and property
   tests have implicitly assumed linear chains. The spec should
   acknowledge that trace branching is now part of the
   trace contract — not just the cause-link semantics. Decide:
   accept and document, or push back and ask for a different
   trace shape (e.g., chain `RestartChildCompleted` after
   `IsolateStopped`'s last `MessageAbandoned`, making it
   linear). Recommend accepting the branching shape — it
   represents the actual causal structure honestly. (Round 1
   A1.)

Agent-resolvable findings codex can fold into the slice during
implementation:

- Pin in spec: "RestartChildren on a parent with no direct
  children emits `HandlerFinished{RestartChildren}` and nothing
  else." (Round 1 N1.)
- Pin in spec: the first-turn edge case is acknowledged
  ("walk includes current child records whose child has not yet
  run"). The stronger cross-parent same-round wording is not
  representable with the current spawn API because spawn records
  the current handler as parent. (Round 1 N2.)
- Pin in spec: extending `runtime_properties.rs` is part of
  slice 009 acceptance. (Round 1 N3.)
- Pin in spec: registry growth now scales with restart count
  plus spawn count for long-running supervised workloads.
  (Round 1 A2.)
- Pin in plan: `ChildRecord` is mutated in place during
  restart; vec index is preserved. (Round 2 N1.)
- Pin in plan: `restart_recipe` is kept alive across restarts
  via `&self.create(...)`; the same private recipe handle serves
  multiple restarts. (Round 2 N2.)
- Pin in plan: commit to either a new integration test file
  `tests/restart_children.rs` or extending the unit-test
  module in `src/lib.rs`. Recommend the unit-test extension
  given the private-state proof surface. (Round 2 A1.)

---

## Round 3: Decision Pass

Response to Round 1/2 findings:

- **A1 accepted.** Trace branching is now an explicit part of the trace
  contract. More precisely, the runtime trace is a deterministically ordered
  causal tree: each event still has at most one cause, but one event may cause
  multiple direct downstream events. The spec and plan now document the
  `RestartChildAttempted -> IsolateStopped` and
  `RestartChildAttempted -> RestartChildCompleted` branching shape.
- **N1 accepted.** The spec and plan now pin the no-direct-children edge case:
  `RestartChildren` emits no restart events beyond the normal handler-finished
  event.
- **N2 accepted with precision.** The spec and plan now pin that restart walks
  all current child records, including children whose first handler step has
  not run yet. The earlier "spawned earlier in the same round" phrasing was
  narrowed because the current spawn API always records the current handler as
  parent and a handler only runs once per round.
- **N3 accepted.** The spec and plan now make the `runtime_properties.rs`
  extension part of slice 009 acceptance.
- **A2 accepted.** The spec and plan now state that registry growth scales with
  spawn count plus restart replacement count under the no-id-reuse model.
- **Round 2 N1 accepted.** The plan now pins in-place `ChildRecord` mutation,
  preserving the record's `Vec` index, parent, and child ordinal.
- **Round 2 N2 accepted.** The plan now pins that restart recipes are called by
  shared reference and kept alive across restarts.
- **Round 2 A1 accepted.** Tests for restart execution will extend the
  `tina-runtime-current/src/lib.rs` unit-test module because the proof surface
  depends on private child-record snapshots.

Implementation status after this pass: ready under autonomous bucket mode.

---

## Round 4: Implementation Pass

Implemented in:

- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/local_send_dispatch.rs`
- `tina-runtime-current/tests/runtime_properties.rs`

What landed:

- `Effect::RestartChildren` now executes for direct child records in
  deterministic child-ordinal order.
- Restartable child records stop the old running incarnation through the
  existing stop-and-abandon path, create a fresh child via the stored restart
  recipe, and mutate the existing child record in place.
- Non-restartable children emit `RestartChildSkipped { reason:
  NotRestartable }`.
- No-direct-child restarts emit no restart subtree beyond
  `HandlerFinished { effect: RestartChildren }`.
- Old child addresses stay stale/closed; replacement addresses are visible only
  through crate-private test snapshots.
- The running-child case proves the new trace-tree shape:
  `RestartChildAttempted` directly causes both `IsolateStopped` and
  `RestartChildCompleted`.
- Generated-history property tests now include restart requests and assert that
  every restart attempt has a visible skipped/completed outcome.

Verification:

- `make test` passed.
- `make verify` passed: fmt, check, workspace tests, Loom, docs, and clippy.
- `make miri` passed for the SPSC mailbox Miri suite.

Implementation note:

- The restart recipe is held behind a shared private handle so it can be
  cloned out of the child record long enough to call into the runtime mutably,
  while preserving one recipe across repeated restarts.

---

## Round 5: Final Bookkeeping

Review response:

- The plan now names the concrete storage choice:
  `Rc<dyn ErasedRestartRecipe<S, F>>`. This is intentionally current-thread;
  cross-shard restart work must revisit `Arc` or an equivalent cross-thread
  recipe handle.
- `restart_child_record` now comments the clone-and-reassign shape so a future
  cleanup does not accidentally consume the recipe on the first restart.

---

## Round 5: Implementation Review

Artifacts reviewed:

- `tina-runtime-current/src/lib.rs` (+580 lines: 3 new
  `RuntimeEventKind` variants, `RestartSkippedReason::NotRestartable`,
  `restart_direct_children` + `restart_child_record` + helpers,
  recipe storage migrated `Box → Rc`, 8 new unit tests + 6
  augmentations to existing tests)
- `tina-runtime-current/tests/runtime_properties.rs` (+111 lines:
  `RestartParent` / `RestartChild` fixtures, `EnqueueParentRestart`
  operation, `assert_restart_attempts_have_visible_outcomes`, new
  property test)
- `tina-runtime-current/tests/local_send_dispatch.rs`: split the
  old `reply_and_restart_children_remain_observed_and_not_executed`
  into two tests because the behaviors diverged
- spec-diff and plan revisions (Round 3 Session A response above)
- `make verify`: green (17 runtime unit tests + 5 runtime_properties
  + 8 spawn + 7 stop + 7 panic + 7 address-liveness + 6 send + 5
  trace-core + 5 sputnik + 7 supervision + 5 spsc + 4 loom + 4 drop
  + 2 allocation + 3 miri + clippy + docs)

### Headline

Slice 009 is correct. Ready to commit.

The branching trace shape is pinned in spec/plan AND directly
tested. The slice-008 regression catches fired exactly as
predicted (the "doesn't execute yet" tests are gone, replaced
with "executes correctly" tests). All seven Round 1/2
agent-resolvable findings are folded into artifacts. Two small
implementation observations worth folding in before commit;
neither blocks ship.

### Findings

- **F1.** All seven Round 1/2 agent-resolvable items folded into
  spec/plan as recorded in Round 3, AND reflected in code:
  - No-children edge → tested by `restart_children_with_no_-
    children_emits_no_restart_subtree` (whole-trace assertion
    ending at `HandlerFinished{RestartChildren}`, no
    EffectObserved, no restart subtree).
  - Spawn-this-round-then-restart edge → tested by
    `restart_children_can_restart_child_before_its_first_turn`.
  - Trace branching → pinned in spec/plan AND directly tested
    by `restart_children_replaces_restartable_child_and_-
    preserves_ordinal` lines 2049-2064: the test filters the
    trace for events whose `cause()` points to the
    `RestartChildAttempted` id, collects them, and asserts BOTH
    `IsolateStopped` AND `RestartChildCompleted` are direct
    consequences. This is the branching invariant proven by
    direct trace walk, not inference.
  - In-place ChildRecord update → verified by code: `child`,
    `mailbox_capacity`, and `restart_recipe` fields are mutated;
    vec index, parent, and ordinal are preserved.
  - Recipe lifetime → verified: recipe is `Rc<dyn ...>`, cloned
    out (refcount bump), used, and reassigned to the slot.

- **F2.** Slice 008 regression catches fired correctly. Two
  tests had to break when slice 009 landed:
  - Slice 008's `restart_children_does_not_consult_child_-
    records_or_call_restart_factories_yet` (lib.rs unit test):
    correctly removed.
  - Slice 002's `reply_and_restart_children_remain_observed_-
    and_not_executed` (integration test): correctly split into
    `reply_remains_observed_and_not_executed` and
    `restart_children_with_no_children_emits_no_restart_-
    subtree`, because Reply still emits `EffectObserved` and
    RestartChildren does not.

- **F3. Property test extension is on point.** The new
  `assert_restart_attempts_have_visible_outcomes` walks every
  `RestartChildAttempted` in the generated-history trace and
  asserts at least one downstream event with matching cause —
  either `RestartChildSkipped` or `RestartChildCompleted`. This
  is exactly the "trace branching is well-formed" invariant the
  spec asked for, validated over arbitrary operation sequences.

- **F4. Test coverage is comprehensive.** The 8 new unit tests
  cover every spec acceptance criterion:
  - no-children edge
  - restartable replacement preserving ordinal + branching
  - precollected-old-message abandonment (slice 003 path reused)
  - already-stopped/panicked old children (no duplicate Stopped)
  - multiple children in child-ordinal order
  - non-restartable skip with trace
  - no grandchild recursion
  - spawn-this-round-then-restart
  Plus the property test for the branching invariant over
  generated histories.

### Negative findings (small bookkeeping)

- **N1. Recipe storage migrated `Box → Rc` without an explicit
  artifact note.** Slice 008's plan had `restart_recipe:
  Option<Box<dyn ErasedRestartRecipe>>`. Slice 009 changed to
  `Option<Rc<dyn ErasedRestartRecipe>>` (lib.rs:1162 and 1175)
  so the recipe can be cloned across multiple restarts without
  ownership games. Round 4's "Implementation note" mentions
  "shared private handle" but doesn't name `Rc` explicitly. The
  plan should pin the type change so a future reviewer doesn't
  have to read code to discover it. One sentence:
  "the recipe is stored as `Rc<dyn ErasedRestartRecipe<S, F>>`,
  not `Box`, so it can be cloned across restarts. `Rc` is
  sufficient because tina-runtime-current is single-threaded;
  a future cross-shard slice may need `Arc`."
  *Class: agent-resolvable.*

- **N2. The recipe clone-and-reassign idiom in
  `restart_child_record` reads slightly oddly without context.**
  Lines 803-805 clone the recipe out of the slot, then line 838
  reassigns it back. The semantics are correct (Rc refcount
  bumps and unbumps, slot ends up with the same recipe), but a
  one-line comment like
  `// recipe is preserved across restarts; we re-bind to keep
  the borrow story clean` would clarify intent for future
  readers.
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, no defects worth blocking on

- **A1. `Rc<dyn ...>` is `!Send`.** Consistent with slice 005's
  no-Send-bound choice for the runtime crate. Future cross-shard
  work will need `Arc`, but that's local to tina-runtime-current
  private types — no public API impact today.
- **A2. Borrow-checker dance via index-collection.**
  `restart_direct_children` collects child-record indices into a
  `Vec<usize>` then iterates. Standard pattern to avoid
  iterator-invalidation conflicts when calling methods that
  need `&mut self`. ✓
- **A3. The `precollected` mechanism for already-snapshotted
  round messages.** `restart_child_record` takes the old
  child's already-snapshotted round message (if any) via
  `round_messages.get_mut(...).and_then(Option::take)`, then
  threads it through `stop_entry_with_precollected`. The slice
  003 abandonment path then drains that message plus anything
  else in the closed mailbox. Tested explicitly by
  `restart_children_abandons_precollected_old_child_message`.
- **A4. Trace branching invariant verified at multiple scales.**
  Test 10 (`restart_children_replaces_...`) verifies branching
  in a single concrete restart. The property test verifies it
  across generated histories. Different proof shapes converging
  on the same invariant.

What I checked and found defended:

- All 8 new unit tests pass via `make verify`.
- `RestartChildAttempted` correctly causes both `IsolateStopped`
  (when running) and `RestartChildCompleted`. Both downstream
  events have `caused_by = attempted_id`. Verified by
  `direct_consequences` walk in test 10.
- Slice 003's stop-and-abandon path reused; `MessageAbandoned`
  events still chain `caused_by = IsolateStopped`. The tree
  shape is `Attempted → {Stopped → Abandoned*, Completed}`.
- No public API changes. `RuntimeEventKind` got three new
  variants (already pub-visible enum); `RestartSkippedReason`
  is new but pub.
- Slice 008's `RestartableSpawnAdapter` continues to impl both
  `ErasedSpawn` (for initial spawn) and `ErasedRestartRecipe`
  (for restart calls). Unchanged.
- Single-cause-per-event invariant preserved: every
  `push_event` call passes one `cause` argument.
- Trace event ID determinism preserved across reruns
  (`identical_runs_produce_identical_trace_and_lineage`).
- The new `tests/miri_spsc.rs` is running in CI; this is
  outside slice 009's scope but is a healthy addition to the
  project's proof surface.

### Final judgment

**Ready to commit after two small bookkeeping items fold in.**

Suggested commit shape:

- `feat(runtime): execute Effect::RestartChildren with restart
  factory dispatch` — covers `tina-runtime-current/src/lib.rs`,
  `tina-runtime-current/tests/runtime_properties.rs`, and
  `tina-runtime-current/tests/local_send_dispatch.rs`.
- `docs(intent): add 009 mariner restart-children execution
  phase` — covers `.intent/phases/009-mariner-restart-children-
  execution/`.

Before commit, codex can fold these in autonomously:

- Plan: pin "the recipe is stored as `Rc<dyn ErasedRestartRecipe>`,
  not `Box`, so it can be cloned across restarts. `Rc` is
  sufficient because tina-runtime-current is single-threaded;
  cross-shard work will need `Arc`."
- Code: one-line comment on the recipe clone-and-reassign in
  `restart_child_record`.

After commit, per the slice-006/008 pattern:
- Update `CHANGELOG.md` with the slice entry.
- Update `commits.txt` in the phase folder.
- Update `SYSTEM.md` in a separate commit with the trace-tree
  invariant: "the runtime trace is a deterministically ordered
  causal tree; each event has at most one cause, and one event
  may cause multiple direct consequences." This is the slice
  where the tree shape becomes a load-bearing public-facing
  property of the runtime.

### Autonomy assessment

No human-escalation findings remain. The slice can complete
under autonomous bucket mode: codex folds the two bookkeeping
items, commits, and updates `CHANGELOG.md` / `commits.txt` /
`SYSTEM.md`.
