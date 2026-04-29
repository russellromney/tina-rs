# Reviews and Decisions: 006 Mariner Parent-Child Lineage

## Round 1: Spec Diff Review

Artifact reviewed:

- `.intent/phases/006-mariner-parent-child-lineage/spec-diff.md`

Read against:

- `.intent/SYSTEM.md`
- `.intent/phases/005-mariner-local-spawn-dispatch/` (the slice that
  deferred this work)
- `.intent/phases/005-mariner-local-spawn-dispatch/reviews_and_decisions.md`
  (Round 3 F1, Round 4 F1-resolution-by-deferral)
- `tina-runtime-current/src/lib.rs` (current registration and spawn
  paths)

### Positive conformance review

Judgment: passes

- **P1.** Lands the bookkeeping deferred in slice 005. This is the
  right slice in the right place — small, narrow, and load-bearing
  for the supervision work that comes next.
- **P2.** "Lineage is runtime-owned state, not runtime trace
  vocabulary." Disciplined choice. Resists the easy widening of
  "everything is observable through the trace." The trace records
  what the runtime *did*; lineage records what the runtime *knows*.
  Two different surfaces for two different purposes.
- **P3.** Negative-space discipline is excellent. The "What does not
  change" list explicitly forbids: execute `RestartChildren`, add
  trace events, add parent-child notifications to handlers, add
  child-address return ergonomics, change shard placement, change
  Send/Spawn/Stop/abandon/panic semantics. The slice refuses to
  drift into adjacent supervision concerns.
- **P4.** Verification names the three cases that matter:
  root-without-parent, spawned-with-parent, nested-chain. Plus the
  survival-of-stop-and-panic case. Plus the
  `RestartChildren`-still-observed-only regression check. Five
  testable criteria, each clearly a test.
- **P5.** Spec is explicit that this slice does not erase lineage
  when an isolate stops or panics. That is the right invariant
  (lineage is a record of who *spawned* whom, not who is *currently
  alive*).

### Negative conformance review

Judgment: passes with two things to pin

- **N1.** "Chain" in verification line 48 is loose. Pin: parent is
  the direct spawner only; ancestry is a walk, not a stored field.
  For nested spawn `A → B → C`, the data model is `C.parent ==
  Some(B)` and `B.parent == Some(A)` and `A.parent == None`. The
  acceptance test should assert each direct edge, not a derived
  chain field. Otherwise a future implementer might add a
  `ancestors: Vec<IsolateId>` field and call it conformant.
- **N2.** Lineage entries for stopped isolates persist forever. Spawn
  monotonically grows `next_isolate_id`, so the lineage table grows
  without bound under long-running spawn/stop churn. That is fine
  for this slice (no real-runtime work yet), but the spec should
  acknowledge it as a known follow-up rather than letting the next
  reader discover it. One sentence: "lineage entries persist past
  stop; cleanup belongs to a later supervision or compaction slice."

### Adversarial review

Judgment: passes, with two questions worth being honest about

- **A1.** Lineage data is somewhat redundant with the trace's
  `Spawned { child_isolate }` events from slice 005. Each `Spawned`
  event carries `isolate_id = parent` (the spawning isolate, who
  the event is attributed to) and `kind: Spawned { child_isolate }`.
  A trace walker can derive parent-child from those events alone.
  The slice's framing — "guessing later from trace order is fragile,
  storing it is solid" — is correct, but worth being honest about
  *why* stored state beats trace derivation here:
  - the trace can be truncated, replayed partially, or held by a
    different consumer than the runtime
  - live runtime decisions (a future `RestartChildren`) need a
    queryable lookup, not a stream walk
  - storing it once on spawn is O(1); deriving on restart is O(N)
  Consider one sentence in the spec to pin this rationale.
- **A2.** The slice asserts "stored lineage remains available after
  stop and panic transitions." But it does not address what happens
  to the lineage of a parent isolate that itself gets stopped or
  panics. The child still records `parent == Some(stopped_parent_id)`.
  That is correct (lineage records who *spawned*, not who is
  currently *alive*), but a future restart slice needs to decide
  whether RestartChildren on a stopped parent is meaningful. Out of
  scope for 006, but the spec should at least say: "lineage points
  to the spawner's id regardless of whether the spawner is still
  alive at lookup time."

What I checked and found defended:

- Self-spawn (a handler returns `SpawnSpec::new(SelfClone, cap)`):
  the new child gets a different id; lineage is well-defined as
  `child.parent == Some(self.id)`. No ambiguity.
- Recursive spawn over multiple steps: each spawning isolate is the
  parent of its respective child. Chain shape is unambiguous.
- Multi-root setups: each root has `parent == None`. Plan explicitly
  handles this.
- Failure of spawn (capacity 0 panic from slice 005): no id
  incremented, no lineage written. Atomic at the runtime boundary.
- Trace shape unchanged: no new events, no causal-edge rewrites.
  Slice 005's `Spawned` event is sufficient for the trace surface.

---

## Round 2: Plan Review

Artifact reviewed:

- `.intent/phases/006-mariner-parent-child-lineage/plan.md`

### Positive conformance review

Judgment: passes

- **P1.** Decisions correctly pin: `Option<IsolateId>` per entry,
  `None` for root, `Some(spawner_id)` for spawned, lineage survives
  stop/panic, `RestartChildren` stays observed-only, no new trace
  events.
- **P2.** Plan correctly states "the spawn path is the only runtime
  path that sets a parent in this slice." That is a sharp invariant
  for the implementer.
- **P3.** Plan correctly forbids inferring lineage from trace order
  (Trap line 54). That defends the spec's framing.
- **P4.** Plan correctly forbids widening lineage into a stable
  public API just to make tests convenient (Trap line 55). Right
  call — `pub(crate)` keeps the surface narrow.
- **P5.** Plan correctly forbids erasing lineage on stop/panic (Trap
  line 56), reinforcing the spec's "lineage records spawn history,
  not current liveness" stance.

### Negative conformance review

Judgment: passes with one structural thing to pin

- **N1.** `pub(crate) helper used by unit tests or another equally
  narrow internal test hook` (plan line 24-26) breaks the
  established slice-001-through-005 pattern of putting slice proofs
  in `tina-runtime-current/tests/<slicename>.rs` integration tests.
  Integration tests are external test crates per Rust convention, so
  `pub(crate)` is not visible to them. The plan implies the lineage
  proofs live as **unit tests inside `src/lib.rs`** (in a
  `#[cfg(test)] mod tests {}` block), which would be the first slice
  to do this. Pin the choice: either:
  1. put lineage proofs in `src/lib.rs#[cfg(test)] mod tests`, or
  2. expose lineage behind `#[cfg(any(test, feature = "test-support"))]`
     so existing integration-test files can use it (more complex), or
  3. expose a `pub fn parent(&self, id: IsolateId) -> Option<IsolateId>`
     and accept the public-surface commitment (the plan currently
     forbids this).
  Recommend (1) — smallest change, no new feature flags, keeps the
  surface narrow. But the plan should say so explicitly so the
  implementer does not invent the structure on the fly.

### Adversarial review

Judgment: passes, with one thing worth being explicit about

- **A1.** Plan does not address what happens during a future
  `RestartChildren` slice when a child is replaced. Does the
  replacement isolate inherit the original's parent? Slice 006's
  rule is "lineage is set at spawn and never mutated." Future
  restart-replace logic might break that invariant by either
  (a) updating the existing entry's parent unchanged + replacing
  the handler, or (b) creating a new entry with a new id and
  copying the parent. This is a slice 007+ decision, but the
  current slice's invariant ("set at spawn, never mutated") may
  need to be revisited then. Worth one sentence acknowledging that
  the immutability rule is set for *this slice*, not as a permanent
  invariant.

What I checked and found defended:

- Plan does not change spawn ordering, ingress semantics, or any
  prior slice's behavior.
- The `pub(crate)` choice keeps lineage out of the stable public
  API surface.
- No new trace events, no new effect kinds, no widening of
  scheduling rules.
- `make verify` is the verification command, consistent with prior
  slices.

---

## Open Decisions

Three things you (the human) need to weigh in on before this slice
is implementable as written. Two are real, one is bookkeeping.

1. **Test-file location.** Pin where lineage proofs live: as unit
   tests inside `src/lib.rs#[cfg(test)] mod tests`, OR introduce a
   `test-support` cargo feature so existing integration tests can
   use the helper. Recommend the unit-tests-inside-lib.rs option —
   smallest change, no feature flags, lineage stays
   crate-private. (Round 2 N1.)

2. **Lineage-cleanup scope.** Decide whether to acknowledge in the
   spec that lineage entries persist past stop indefinitely, with
   cleanup deferred to a future supervision or compaction slice.
   Recommend yes — a one-sentence acknowledgment prevents the next
   reader from discovering the unbounded-growth path themselves.
   (Round 1 N2.)

3. **Restart-replacement future invariant.** Add one sentence saying
   the "set at spawn, never mutated" rule is for this slice, not a
   permanent invariant — the future `RestartChildren` slice will
   need to decide whether replaced children inherit lineage.
   Bookkeeping but worth pinning. (Round 2 A1.)

Smaller, optional pins:

- Spec could explain *why* stored lineage beats trace-derived
  lineage (truncation, partial replay, O(1) lookup vs O(N) walk).
  Round 1 A2.
- Spec could add: "lineage points to the spawner's id regardless of
  whether the spawner is still alive at lookup time." Round 1 A2.
- "Chain" in spec verification line 48 should clarify: parent is
  the direct spawner; ancestry is a walk, not a stored field.
  Round 1 N1.

If you take the three Open Decisions, slice 006 is ready to
implement. The smaller pins are clarifications, not blockers.

---

## Round 3: Implementation Review

Artifacts reviewed:

- `tina-runtime-current/src/lib.rs` (single-file delta, +349 lines:
  ~30 lines of runtime code + ~310 lines of unit-test module
  including isolate fixtures and helpers)
- `.intent/phases/006-mariner-parent-child-lineage/spec-diff.md`
  (revised to address smaller pins)
- `.intent/phases/006-mariner-parent-child-lineage/plan.md` (revised
  to address all three Open Decisions)
- `make verify`: green (6 lib.rs unit tests + 8 spawn + 6 local_send +
  7 panic + 7 stop + 5 trace-core + 4 loom + clippy + docs)

### Headline

Slice 006 is the cleanest slice in the project so far. Ready to
commit.

All three Open Decisions are addressed in artifacts and reflected
in code. All three smaller pins are addressed in artifacts. The
implementation is small, the tests are sharp, and the pattern
break (unit tests inside lib.rs) is documented in the plan rather
than discovered in code.

### Findings

- **F1.** All three Round 1/2 Open Decisions are pinned in artifacts:
  - **Open Decision 1 (test-file location).** Plan lines 23-26 pin
    "uses unit tests inside `src/lib.rs` so the runtime can expose a
    `pub(crate)` lineage helper without widening the public surface
    or adding a feature flag." Implementation matches: the
    `lineage_snapshot()` helper is `#[cfg(test)] pub(crate)`; tests
    live in `#[cfg(test)] mod tests {}` inside the runtime crate.
  - **Open Decision 2 (lineage cleanup unbounded).** Spec lines
    47-48 and plan lines 29-30 acknowledge "Lineage entries persist
    after stop in this slice. Cleanup or compaction is a later
    supervision/runtime maintenance problem."
  - **Open Decision 3 (restart-replacement invariant).** Plan lines
    31-33 say "'Set at spawn, never mutated' is a slice-006 rule,
    not a permanent invariant. A later restart/replacement slice
    must decide whether replacement children inherit lineage or
    receive a fresh registration identity."

- **F2.** All three smaller-pin items are addressed:
  - "Chain" clarification: spec lines 56-57 say "parent is the
    direct spawner, and ancestry is a walk across repeated parent
    lookups."
  - Why stored beats trace-derived: spec lines 18-20 explain
    truncation, partial replay, and O(1) vs O(N) lookup.
  - Lineage points to spawner regardless of liveness: spec lines
    45-46.

- **F3. Implementation matches the approved design exactly.**
  - `RegisteredEntry` gains `parent: Option<IsolateId>`. Threaded
    cleanly through `register_entry(parent, ...)` and
    `spawn_isolate(parent, ...)`.
  - Public `register()` passes `parent: None`. Spawn path passes
    `Some(spawning_isolate_id)`.
  - `ErasedSpawn::spawn(self, runtime, parent: IsolateId)` cleanly
    threads the parent through type erasure. Parent is determined
    by the dispatcher arm (where `isolate_id` of the current
    handler is in scope), then handed to the spawn adapter at
    invocation time. Clean separation: the spawn request payload
    does not need to know its own parent.
  - `lineage_snapshot()` is `#[cfg(test)] pub(crate)`. Narrowest
    possible test surface.
  - `register_entry` is atomic with respect to `next_isolate_id`:
    capacity-0 panic in `spawn_isolate` fires before any id is
    incremented. No half-registered children.

- **F4. Test coverage is comprehensive and whole-state.** Six unit
  tests, each asserting `lineage_snapshot()` equality (whole-state
  assertion, not spot checks):
  - `root_registered_isolates_have_no_parent` (multi-root case
    explicit).
  - `nested_spawns_record_direct_parent_edges` (root → child →
    grandchild chain, asserted edge by edge).
  - `child_lineage_survives_when_parent_stops` (slice 003
    interaction).
  - `child_lineage_survives_when_parent_panics` (slice 004
    interaction; also asserts a `HandlerPanicked` event is in the
    trace, proving the panic path actually fired).
  - `restart_children_remains_observed_only_after_lineage_lands`
    (regression check; correctly load-bearing — this test will
    *intentionally* break when slice 007 makes RestartChildren
    real).
  - `identical_runs_produce_identical_trace_and_lineage` (asserts
    both trace AND lineage are deterministic across reruns).

### Bookkeeping note (not a finding)

- Unit-tests-inside-`lib.rs` is a new pattern for tina-rs. Slices
  001-005 all use `tests/<slicename>.rs` integration test files.
  The plan documents the choice (lines 23-26) but ROADMAP.md and
  SYSTEM.md do not yet acknowledge that runtime-private state is
  proven via unit tests rather than integration tests. Worth one
  line in either of those when the slice's CHANGELOG entry lands,
  so a future reader who searches `tests/` for slice-006 evidence
  knows where to look.

### What I checked and found defended

- The spawn dispatcher arm at lib.rs:606 passes `isolate_id` (the
  parent) to `spawn.spawn(self, isolate_id)`. ✓
- Self-spawn (a handler returns `SpawnSpec::new(SelfClone, cap)`):
  the new child's `parent` is `self.id`. Lineage well-defined.
  Implementation is structurally correct here even without an
  explicit test.
- Capacity-0 spawn: still panics atomically (no id increment, no
  lineage written). Slice-005 invariant preserved.
- No new public surface, no Tokio dependency, no cross-shard work,
  no supervisor mechanism, no trace event additions. Negative-space
  discipline holds.
- `make verify` passes end-to-end including loom and clippy. The 6
  new unit tests show up as a new `Running unittests src/lib.rs
  (...)` block under `tina_runtime_current`.

### Final judgment

**Ready to commit.**

Suggested commit shape:

- `feat(runtime): record parent-child lineage on spawn`
  — covers `tina-runtime-current/src/lib.rs` (the parent field, the
  threaded threading, the `#[cfg(test)] pub(crate) lineage_snapshot`
  helper, and the unit-test module).
- `docs(intent): add 006 mariner parent-child-lineage phase`
  — covers `.intent/phases/006-mariner-parent-child-lineage/`.

After commit:
- Update `CHANGELOG.md` with the slice entry.
- Update `commits.txt` in the phase folder.
- Optionally: one line in `ROADMAP.md` or `SYSTEM.md` noting that
  runtime-private state is proven via unit tests, since this is the
  first slice to do so.
