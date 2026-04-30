# Reviews and Decisions: 010 Mariner Supervised Panic Restart

## Round 1: Spec Diff Review

Artifact reviewed:

- `.intent/phases/010-mariner-supervised-panic-restart/spec-diff.md`

Read against:

- `.intent/SYSTEM.md`
- `.intent/phases/004-mariner-panic-capture/` (catch_unwind boundary
  this slice plugs into)
- `.intent/phases/006-009/` (lineage, address liveness, restartable
  records, and restart execution that this slice consumes)
- `tina/src/lib.rs:RestartPolicy`, `RestartBudget` (existing policy
  types in `tina`)
- `tina-runtime-current/src/lib.rs` (current panic path,
  `restart_direct_children`, `restart_child_record`)
- `ROADMAP.md` ("supervisor mechanism does not ship before Mariner";
  Open Question 6 on policy-vs-mechanism split)

### Positive conformance review

Judgment: passes

- **P1.** Right slice, right time. The four prior groundwork slices
  (006-009) all pointed at this. Auto-restart on handler panic is
  the Erlang/Tina-Odin "dead worker is not a dead system" property
  — without it, supervision is a half-measure.
- **P2.** Crate split is faithful to ROADMAP intent. Policy types
  stay in `tina`; supervisor config + future mechanism live in
  `tina-supervisor`; runtime-specific execution lives in
  `tina-runtime-current`. Concrete → abstract dependency direction
  preserved.
- **P3.** Reuses the slice 009 restart helper for selected child
  records. One restart execution path, two triggers (manual via
  `Effect::RestartChildren`, automatic via supervised panic).
  Avoids forking restart semantics.
- **P4.** Trace vocabulary cleanly extends the slice 009 tree
  shape: `SupervisorRestartTriggered` and `SupervisorRestartRejected`
  are new top-level events, while existing `RestartChildAttempted`/
  `Skipped`/`Completed` are reused for the per-child layer.
- **P5.** Negative-space discipline is broad and right: no
  `Effect::Stop` trigger, no manual `RestartChildren` consulting
  policy, no public child-record or replacement-address APIs, no
  time-windowed budgets yet, no factory-panic catching, no
  task-dispatcher example, no I/O effects.
- **P6.** Per-failure-response budget accounting (one supervised
  failure = one budget unit, regardless of replacement count)
  matches Erlang OTP semantics. Honest framing.
- **P7.** Recursive supervision is correctly bounded — only direct
  child records of the configured parent. No supervision-tree
  walking up to grandparents.
- **P8.** "If the supervisor parent is already stopped, runtime
  emits a visible rejection" handles the dead-supervisor case
  cleanly via the existing trace vocabulary.

### Negative conformance review

Judgment: passes with four things to pin

- **N1. `tina-supervisor` crate role is unstated for slice 010
  vs future slices.** The new crate ships only `SupervisorConfig`
  in this slice — a 3-field wrapper around `RestartPolicy` and
  `RestartBudget`. That's a small payload for a whole crate.
  Worth pinning: "this slice ships only `SupervisorConfig` in
  `tina-supervisor`; future slices may grow the crate with
  runtime-agnostic supervision mechanisms (e.g., shared trace
  vocabulary, policy logic). The crate exists now to reserve the
  namespace and lock in the policy-vs-mechanism split before
  `tina-runtime-monoio` or `tina-runtime-tokio-bridge` arrive."
  *Class: agent-resolvable.*

- **N2. Budget = "runtime lifetime" is a real subset of Erlang
  semantics that should be honest about its limitation.** Erlang
  uses "max N restarts in time window T" — the count resets at
  window boundaries. This slice has no window/reset. A supervised
  parent that exhausts its budget cannot recover for the
  runtime's lifetime. Worth pinning honestly: "budget exhaustion
  is permanent within the runtime's lifetime; time-windowed
  budgets and resets are deferred to a future slice. Long-running
  workloads may hit terminal supervisor exhaustion."
  *Class: agent-resolvable.*

- **N3. `supervise()` before any children is unstated.** The spec
  doesn't say what happens when `supervise()` is called for a
  parent that has no children yet. The natural answer ("the config
  takes effect when children fail") is correct, but spec should
  say so to prevent surprise.
  *Class: agent-resolvable.*

- **N4. Trace tree depth grows beyond slice 009's commitment.**
  Slice 009 pinned that the runtime trace is a deterministically
  ordered causal tree. Slice 010 extends the tree to arbitrary
  depth: `HandlerPanicked → SupervisorRestartTriggered →
  RestartChildAttempted* → IsolateStopped + Skipped/Completed +
  MessageAbandoned*`. The data model already supports this (one
  cause per event; tree shape comes from cause-sharing), but the
  spec should explicitly extend slice 009's invariant: "the trace
  tree may now be arbitrary depth as supervised restart subtrees
  nest under panic events. Consumers should walk causes
  recursively."
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, with two angles worth pinning

- **A1. Cross-shard / unknown-parent semantics for `supervise()`.**
  Plan acknowledges this in Open Question 1 (panic vs Result for
  unknown/stale parent). The slice 002 / slice 005 precedent for
  programmer errors at runtime APIs is panic. `supervise()`'s
  "configure something that doesn't exist" feels like the same
  shape — programmer error, not recoverable runtime state. Forcing
  callers to handle a Result for what is fundamentally a setup
  bug adds noise.
  *Class: human-escalation.* Affects what `supervise()` feels
  like to use and should be deliberate.

- **A2. Replacing a supervisor config: reset vs preserve budget.**
  Plan Open Question 2 asks this. Reset is intuitive ("new config,
  fresh budget"). Preserve has the merit that "I just made the
  policy stricter; don't let me skip past my own past failures."
  In practice, supervisors are usually configured once at startup
  and reconfiguration is a rare administrative event. Reset is
  the cleaner default.
  *Class: human-escalation.* Real semantic decision.

What I checked and found defended:

- Manual `Effect::RestartChildren` keeps slice 009 semantics
  unchanged (no policy/budget consultation). Two triggers, one
  shared per-child execution path.
- Normal `Effect::Stop` is not a failure (panic-only trigger).
  Aligns with Erlang `permanent`/`transient`/`temporary` policy
  separately — but that's a future slice's question.
- Root-isolate panics still just panic-capture (no parent →
  no supervisor → no restart). ✓
- The slice doesn't reconstruct supervision state from the
  trace — supervisors are runtime-owned state, like child
  records.
- Restart factory panics propagate as construction errors (slice
  009 invariant preserved).

---

## Round 2: Plan Review

Artifact reviewed:

- `.intent/phases/010-mariner-supervised-panic-restart/plan.md`

### Positive conformance review

Judgment: passes

- **P1.** Plan correctly maps spec items to implementation: new
  crate, runtime-side records, public `supervise()` API, panic
  path extension, slice 009 helper reuse, property test extension.
- **P2.** Trace vocabulary section pins exact event shapes,
  including the new `SupervisionRejectedReason` enum with
  `BudgetExceeded { attempted_restart, max_restarts }` and
  `SupervisorStopped` variants.
- **P3.** Cause shape section explicitly walks the new tree edges:
  `HandlerPanicked → IsolateStopped`,
  `HandlerPanicked → SupervisorRestartTriggered`,
  `SupervisorRestartTriggered → each selected RestartChildAttempted`.
- **P4.** Plan calls out that `RestartChildAttempted` continues to
  cause `IsolateStopped` (slice 009 branching) when the selected
  child was still running. This nesting is correctly inherited
  from slice 009.
- **P5.** Plan honestly lists 5 Open Questions For Review at
  lines 254-269. Self-aware about what needs human judgment.
- **P6.** Traps are sharp: don't reconstruct from trace, don't
  trigger on Stop, don't make manual RestartChildren consult
  policy, don't expose private snapshots, don't catch factory
  panics, don't make policy-excluded siblings look like skipped
  children, don't consume restart recipes, don't reuse ids, don't
  add timed budgets, don't add task-dispatcher/I/O.

### Negative conformance review

Judgment: passes with three small things to pin

- **N1. Plan's proposed `SupervisorConfig` getters take `self`
  by value.** Lines 71-73:
  ```rust
  pub const fn policy(self) -> RestartPolicy;
  pub const fn budget(self) -> RestartBudget;
  ```
  Taking `self` consumes the config. That's unusual for getters
  and forces callers to clone the config to inspect it twice.
  Recommend `&self` (or `Copy` impl with `self`) so callers can
  inspect without consuming.
  *Class: agent-resolvable.*

- **N2. Plan does not pin supervisor-record persistence after the
  supervisor parent stops.** When supervised parent A stops or
  panics, the runtime emits `SupervisorRestartRejected
  { reason: SupervisorStopped }` for any future child failures.
  But: does the supervisor record itself stay in the runtime's
  table forever, like child records and parent lineage? Probably
  yes (slice 007's monotonic-growth invariant), but worth being
  explicit: "supervisor records persist past the supervisor
  parent's stop; lookup returns the live config and the rejected
  rejection path fires when needed."
  *Class: agent-resolvable.*

- **N3. Plan does not pin how `supervise()` handles
  configuration during a step round.** What happens if a child
  isolate calls `supervise()` indirectly via... wait, it can't —
  `supervise()` is a runtime API, not a handler effect. So the
  only callers are external (test setup, future drivers).
  But: can the same external caller call `supervise()`
  mid-step (between two `step()` calls)? Implementation will
  naturally handle this because `supervise()` takes `&mut self`
  on `CurrentRuntime`, but plan should pin that "supervise() is
  expected to be called between step() invocations, not during
  one. There is no concurrent invocation surface."
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, with three plan-level decisions surfaced

- **A1. Plan explicitly asks for review on Q3 (budget accounting
  per-response vs per-child).** This is a real semantic
  commitment. Per-response (Erlang-aligned) is the right call,
  but slice 010 is also the place to make it durable. The
  rationale should land in the spec, not just be a default in
  the plan.
  *Class: human-escalation.* (See Open Decisions.)

- **A2. Plan explicitly asks for review on Q4 (policy-excluded
  children silent vs distinct event).** Silent is simpler and
  reduces trace noise. Distinct events would let replay tools
  trivially answer "which siblings did the policy exclude?" but
  cost trace size. Codex's pick is silent — defensible. Worth
  the spec being explicit about *why* and what replay tools need
  to do instead (walk policy + failed_child + sibling lineage).
  *Class: human-escalation.* (See Open Decisions.)

- **A3. Plan explicitly asks for review on Q5 (cause edge:
  HandlerPanicked vs IsolateStopped).** Codex picked
  HandlerPanicked, which is architecturally correct: the panic
  IS the cause of both the stop and the supervised response. The
  alternative (IsolateStopped → SupervisorRestartTriggered) would
  invent sequential ordering that doesn't represent reality —
  the runtime knows it needs to supervise before the stop
  completes. Pin this.
  *Class: human-escalation.* (See Open Decisions.)

What I checked and found defended:

- The crate dependency direction is correct: `tina-supervisor`
  depends on `tina`; `tina-runtime-current` depends on
  `tina-supervisor`. No reverse edges.
- Manual `Effect::RestartChildren` semantics are explicitly held
  constant.

---

## Round 3: Decision Pass

Human decisions:

- `supervise(...)` panics for unknown, stale, or cross-shard parent addresses.
  This is a setup-time programmer error, not a runtime delivery outcome.
- Reconfiguring a supervisor resets its budget tracker. A new config means a
  new supervisory relationship.
- Budget accounting is per supervised failure response, not per replacement
  child. This matches OTP-style restart intensity: the budget limits failure
  frequency, not the amount of work caused by one failure.
- Policy-excluded children are silent in the trace. Replay derives exclusion
  from policy, failed child ordinal, and child-record order.
- `SupervisorRestartTriggered` is caused by `HandlerPanicked`, so the failed
  child stop and supervised response are sibling consequences of the panic.

Agent-resolvable findings folded:

- `tina-supervisor` is scoped to config vocabulary in this slice; runtime
  mechanism stays in `tina-runtime-current`.
- Runtime-lifetime budgets are pinned as a strict subset of Erlang/OTP's timed
  restart intensity windows. Exhaustion is permanent until a later reset/window
  slice.
- `supervise(...)` before children exist is valid.
- Supervised restart extends the trace tree to arbitrary depth while preserving
  one cause per event.
- `SupervisorConfig` getters take `&self`.
- Supervisor records persist after the supervisor parent stops.
- `supervise(...)` is a between-`step()` configuration API; there is no
  in-handler or concurrent configuration surface.

Implementation status after this pass: ready under autonomous bucket mode.

---

## Original Open Decisions

These were the five human-escalation decisions identified before
implementation. Round 3 records the accepted choices.

1. **Unknown / stale parent in `supervise()`: panic or Result?**
   Plan Q1. The slice 002 / slice 005 precedent for programmer
   errors at runtime APIs is panic — configuring supervision for
   an isolate that doesn't exist is the same shape. Forcing
   callers to handle a Result for a setup bug adds noise.
   Recommend: **panic, matching the existing precedent.** Stale
   generation is the same: supervise() is setup-time, generations
   should be fresh.

2. **Replacing a supervisor config: reset or preserve budget?**
   Plan Q2. Reset is intuitive — new config means new
   relationship. Preserve has merit ("don't let me skip past my
   own past failures") but reconfiguration is a rare admin event,
   and surprising-budget-carryover is worse than surprising-fresh-
   start. Recommend: **reset on reconfigure.**

3. **Budget accounting: per-failure-response or per-replacement-
   child?** Plan Q3. Per-response (one supervised failure = one
   budget unit, regardless of how many replacements that policy
   produces) matches Erlang OTP. The budget exists to bound
   restart frequency, not work. Codex picked per-response.
   Recommend: **per-response, same as Erlang.** Pin the rationale
   in the spec so a future "this should count differently" pull
   request has to reopen intent.

4. **Policy-excluded children: silent or distinct
   `RestartChildSkipped { PolicyExcluded }` event?** Plan Q4.
   Codex picked silent. Pros of silent: smaller trace, no noise
   proportional to siblings. Pros of distinct: replay tools can
   trivially answer "which siblings did the policy not select?"
   without walking lineage. Recommend: **silent, with a spec
   note that replay tools derive selection from policy +
   failed_child_ordinal + sibling lineage.** This keeps the trace
   clean while making the derivation rule explicit.

5. **`SupervisorRestartTriggered` cause: `HandlerPanicked` or
   `IsolateStopped`?** Plan Q5. Codex picked HandlerPanicked.
   The panic IS the cause of both the stop and the supervised
   response — they are sibling consequences of the same failure.
   Forcing IsolateStopped → SupervisorRestartTriggered would
   create artificial sequential ordering. Recommend:
   **HandlerPanicked, as codex picked.** This is the
   architecturally honest choice and aligns with the slice 009
   "trace is a tree, branches represent simultaneous causal
   outcomes" framing.

Agent-resolvable findings codex can fold into the slice during
implementation:

- Pin in spec: "this slice ships only `SupervisorConfig` in
  `tina-supervisor`; future slices may grow the crate with
  runtime-agnostic supervision mechanisms." (Round 1 N1.)
- Pin in spec: "budget exhaustion is permanent within the
  runtime's lifetime; time-windowed budgets are deferred to a
  future slice." (Round 1 N2.)
- Pin in spec: "supervise() may be called before any children
  exist; the configuration takes effect when children fail."
  (Round 1 N3.)
- Pin in spec or plan: "the trace tree may now be arbitrary
  depth as supervised restart subtrees nest under panic events.
  Consumers walk causes recursively." (Round 1 N4.)
- Pin in plan: `SupervisorConfig::policy()` and `budget()` take
  `&self` (or `self` if `Copy`), not consuming `self`. (Round 2
  N1.)
- Pin in plan: "supervisor records persist past the supervisor
  parent's stop; lookup returns the live config and the
  rejection path fires when needed." (Round 2 N2.)
- Pin in plan: "supervise() is expected to be called between
  step() invocations, not during one." (Round 2 N3.)

If you take the five decisions and codex folds the seven
agent-resolvable items, slice 010 is ready to implement under
autonomous bucket mode.

---

## Round 4: Implementation Pass

Implemented in:

- `tina-supervisor/`
- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/runtime_properties.rs`

What landed:

- Added `tina-supervisor` with `SupervisorConfig`.
- Added `CurrentRuntime::supervise(...)` as a setup-time API. Unknown, stale,
  and cross-shard parent addresses panic.
- Added private supervisor records with runtime-lifetime budget state.
- Extended the handler-panic path so supervised direct child failures apply
  `RestartPolicy` and `RestartBudget` before reusing the existing child-record
  restart path.
- Added `SupervisorRestartTriggered` and `SupervisorRestartRejected` trace
  events plus `SupervisionRejectedReason`.
- Preserved manual `Effect::RestartChildren` semantics: manual restart does not
  consult supervisor policy or budget.
- Extended generated-history property tests with supervised child panics and
  supervisor-trigger outcome checks.

Proofs added:

- One-for-one restarts only the failed child.
- One-for-all restarts every direct restartable child.
- Rest-for-one restarts failed and younger children, while older siblings keep
  their identity.
- Selected non-restartable children skip visibly.
- Budget exhaustion and stopped-supervisor cases reject visibly and create no
  replacement children.
- Unsupervised child panic and normal child stop do not trigger supervision.
- `supervise(...)` before children works, and reconfiguration resets budget.
- `supervise(...)` panics for unknown, stale, and cross-shard parent addresses.
- The panic cause directly causes both `IsolateStopped` and
  `SupervisorRestartTriggered`, proving the chosen causal tree shape.

Verification:

- `make test` passed.
- `make verify` passed: fmt, check, workspace tests, Loom, docs, and clippy.
- `make miri` passed for the SPSC mailbox Miri suite.

Implementation note:

- The shared restart helper between manual and supervised paths keeps restart
  child semantics aligned across both triggers.
- Trace branching from slice 009 holds; this slice only deepens the tree while
  preserving single-cause-per-event.
- No `tina` trait crate changes were needed.

---

## Round 5: Implementation Review

Artifacts reviewed (all committed in `99ba928` and `31f97c0`):

- `tina-supervisor/` (new crate, 36 lines src/lib.rs + 19 lines
  tests/supervisor_config.rs)
- `tina-runtime-current/src/lib.rs` (+634 lines: 2 new
  `RuntimeEventKind` variants, `SupervisionRejectedReason` enum,
  `supervisors: Vec<SupervisorRecord>` field,
  `CurrentRuntime::supervise(...)`, `supervise_panic` helper,
  `checked_registered_address` validator, 10 new unit tests)
- `tina-runtime-current/tests/runtime_properties.rs` (+67 lines:
  supervised parent/child fixtures, `EnqueueChildPanic` operation,
  `assert_supervisor_triggers_have_visible_outcomes`, new property
  test)
- `.intent/SYSTEM.md` (refreshed to record the supervisor crate, the
  trace-tree-arbitrary-depth invariant from slice 009, and the
  supervised-restart-on-panic capability)
- `make verify`: green (26 runtime unit tests + 6 runtime_properties +
  2 supervisor_config + 8 spawn + 7 stop + 7 panic + 7 address-
  liveness + 7 send + 5 trace-core + 5 sputnik + 7 supervision_policy
  + 5 spsc + 4 loom + 4 drop + 2 alloc + 3 miri + clippy + docs)

### Headline

Slice 010 is correct. Already committed and the slice is closed.

All five human-escalation decisions implemented as approved. All
seven agent-resolvable findings folded. The trace tree branching
from slice 009 deepens correctly; single-cause-per-event preserved.
SYSTEM.md updated to record the new invariants. No human-
escalation findings remain in this implementation.

### Findings

- **F1.** All five Round 1/2 human-escalation decisions land in
  code as recommended:
  - **Unknown / stale / cross-shard parent** (Decision 1):
    `checked_registered_address` panics with descriptive messages.
    Verified by `supervise_panics_for_unknown_stale_or_cross_-
    shard_parent_addresses` test.
  - **Reconfigure resets budget** (Decision 2): `supervise()`
    line 626-634 finds existing supervisor for the same parent
    and overwrites both `config` and `budget_state` (calls
    `config.budget().tracker()` for fresh state). Verified by
    `supervise_before_children_and_reconfigure_reset_budget_-
    are_supported`.
  - **Per-failure-response budget** (Decision 3):
    `budget_state.record_restart()` is called once per
    `supervise_panic` invocation, before the policy walk. One
    failure consumes one budget unit regardless of how many
    children the policy selects. Verified by
    `supervised_restart_budget_exhaustion_is_visible_and_creates_-
    no_replacement` (uses one-for-all + budget=2 to prove a
    multi-replacement restart still consumes exactly one unit).
  - **Policy-excluded children silent** (Decision 4): the
    `selected` filter at line 928-940 keeps only ordinals where
    `policy.restarts(relation)` returns true; excluded children
    get no `RestartChildAttempted` or other event. The trace
    consumer derives exclusion from `policy + failed_ordinal +
    child_record_order`, exactly as the spec said.
  - **`HandlerPanicked` → `SupervisorRestartTriggered`**
    (Decision 5): the `cause` argument threaded into
    `supervise_panic` is the `HandlerPanicked` event ID; the
    triggered event uses it as `caused_by`. Sibling of
    `IsolateStopped`, not chained after it.

- **F2.** All seven agent-resolvable items folded. Verified
  spot-checks:
  - `tina-supervisor` crate: 36 lines, only `SupervisorConfig`.
    Future supervision mechanism reserved for the crate.
  - `SupervisorConfig::policy()` and `budget()`: take `&self`,
    matching Round 2 N1.
  - `supervise()` before children: tested in
    `supervise_before_children_and_reconfigure_reset_budget_-
    are_supported`.
  - SYSTEM.md: explicitly states "trace is a deterministically
    ordered causal tree" + "one event may be the direct cause of
    many later events." This is the slice 009 invariant carried
    forward and made durable.
  - SYSTEM.md: also adds "It can also apply configured supervisor
    policy and runtime-lifetime budget state when a direct child
    handler panics." Slice 010 is now part of the system
    baseline.

- **F3.** The `supervise_panic` execution sequence is clean and
  reads top-down:
  1. find failed child's record index (return early if no record
     — root isolate or unrecorded child)
  2. get parent and failed_ordinal from the record
  3. find supervisor record for parent (return early if none)
  4. check if supervisor is stopped → emit `SupervisorRestartRejected
     { SupervisorStopped }` and return
  5. attempt budget consumption → on Err: emit `SupervisorRestartRejected
     { BudgetExceeded }` and return
  6. write back updated budget state
  7. emit `SupervisorRestartTriggered`
  8. filter child_records by parent + policy → collect indices
  9. call slice 009's `restart_child_record` for each selected
     index
  No backwards branches, no shared mutable state across steps.

- **F4. Test coverage is comprehensive (10 new unit tests + 1
  property test).** Each policy variant gets its own test
  (`one_for_one`, `one_for_all`, `rest_for_one`); both rejection
  paths (`stopped_supervisor`, `budget_exhaustion`) get tests;
  negative cases (`unsupervised_child_panic`, `normal_child_stop`)
  are tested together; the configuration API gets its own test
  (`supervise_before_children_and_reconfigure_reset_budget`); the
  programmer-error panic path gets its own test
  (`supervise_panics_for_...`); the non-restartable-skip path is
  tested explicitly. Plus the property test catches generated-
  history shapes.

- **F5. Manual `Effect::RestartChildren` semantics are
  preserved.** No supervisor consultation in
  `restart_direct_children`. The two triggers (manual and
  supervised) share `restart_child_record` for per-child
  execution, but the trigger paths are separate. Verified by
  `unsupervised_child_panic_and_normal_child_stop_do_not_trigger_-
  supervision`.

### Negative findings

Judgment: nothing material.

The two small things I might have flagged as agent-resolvable
bookkeeping were already addressed:
- The `Box → Rc` recipe storage from slice 009 is unchanged.
- The runtime tree depth growth is documented in spec, plan, and
  SYSTEM.md.

### Adversarial review

Judgment: passes, with three angles I checked

- **A1. Same-round panic-then-supervise-then-restart trace
  ordering.** When child C panics in step N, the runtime emits
  `HandlerPanicked → IsolateStopped + (MessageAbandoned*) +
  SupervisorRestartTriggered → RestartChildAttempted* → ...`. The
  branching at HandlerPanicked produces two roots:
  IsolateStopped subtree and SupervisorRestartTriggered subtree.
  Trace event IDs are sequential, which means the test's
  whole-trace assertions can pin the exact emit order. Verified
  by `supervised_one_for_one_restarts_only_failed_child` which
  asserts a 14-event sequence whole-trace.
- **A2. Two children panic in the same step.** Each panic invokes
  `supervise_panic` separately. Each consumes one budget unit.
  This is a real edge case for budget accounting under
  concurrent failure — but slice 010 is single-threaded, so
  "concurrent" means "two children's handlers run sequentially
  in the same step round." The test set doesn't have an explicit
  "two panics, two budget units" test, but the property test
  exercises generated histories with multiple panics. Worth
  noting but not blocking.
- **A3. Self-supervised parent.** Could a parent supervise
  itself? `supervise(parent_address, config)` accepts any
  Address, so technically yes. But supervision applies to the
  parent's *direct children*, not the parent itself. So
  self-supervision is meaningless, not catastrophic. The
  unrecorded-child early-return in `supervise_panic` would skip
  any "I am my own child" scenario. ✓ No defect.

What I checked and found defended:

- Single-cause-per-event invariant preserved across the new
  events. Each `push_event` call in `supervise_panic` passes one
  `cause`.
- Branching is honest: `HandlerPanicked` causes
  `IsolateStopped` AND `SupervisorRestartTriggered` (or
  `SupervisorRestartRejected`); both are direct consequences of
  the same panic.
- The `failed_record_index` lookup correctly returns None for
  root isolates (they have no `child_record`), so root panics
  fall through to plain panic-capture without supervisor
  involvement.
- Budget update is atomic: `record_restart()` returns either Ok
  with the new state (then assigned back) or Err (then nothing
  is mutated). No partial-update window.
- Slice 009's `RestartChildAttempted → IsolateStopped + Skipped/
  Completed` branching is preserved inside the supervised
  subtree.
- `tina-supervisor` adds 1 entry to the workspace. Cargo.toml
  membership is correct. No reverse dependencies.
- `make verify` includes `make miri` and is green. Slice 010
  introduces no unsafe code.

### Final judgment

**Already committed; slice is closed correctly.**

Codex committed in two commits:
- `feat(runtime): restart supervised children after panic`
  (99ba928): the implementation.
- `docs(intent): close 010 supervised panic restart phase`
  (31f97c0): the slice's reviews_and_decisions.md, plan.md
  cleanup, commits.txt, and SYSTEM.md updates.

The SYSTEM.md update is the right shape — it pins both the
slice 009 trace-tree invariant AND the slice 010 supervised-
restart capability in one pass, so the system baseline reflects
what's actually shipped.

Slice 010 is the supervisor payoff for slices 006-009. The
runtime now genuinely auto-restarts children on handler panic
under a parent-owned policy with a budget — Tina-Odin's "dead
worker is not a dead system" is real for the single-shard
runtime. Mariner's done-when criteria for supervision are met.

### Autonomy assessment

No human-escalation findings. The slice closed cleanly under
autonomous bucket mode. Future slices can continue from here
without revisiting any 010-level decisions.
