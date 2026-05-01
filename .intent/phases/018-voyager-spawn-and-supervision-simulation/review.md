# 018 Review

Session:

- B (review)

## Plan Review 1

Artifact reviewed:

- `.intent/phases/018-voyager-spawn-and-supervision-simulation/plan.md`

Reviewed against `.intent/SYSTEM.md`, the closeout state of 017, and the
current `tina-sim` surface (which today still has `Spawn = Infallible` and a
hard `panic!` on `RestartChildren` — so 018's scope is real, not cosmetic).

### What looks strong

- The slice is named honestly. It is not "spawn bookkeeping in the simulator."
  It is single-shard supervision parity, including replacement-child
  continuity and stale-identity failure. That is the right bar.
- The plan refuses to invent simulator-only spawn or supervision semantics.
  It pins the surface to the already-shipped public payloads:
  `SpawnSpec`, `RestartableSpawnSpec`, `RestartChildren`, and the current
  supervisor configuration shape. This protects the SYSTEM.md rule that the
  simulator must use the live runtime's meaning model.
- Direct, surrogate, and blast-radius proof are kept distinct rather than
  blurred together. The surrogate-proof list is sharp: bookkeeping without a
  public-path workload, checker plumbing on a non-restart toy, restart
  mechanics without later work reaching a replacement child, final-state
  assertions without the replayed event record. Each of those is a real
  failure mode for this kind of slice.
- Pause gates name the right escape valves (new public `tina` shape, TCP
  forced in, broader checker API, oversize workload) instead of pretending
  the slice can absorb anything.
- "What Will Not Change" explicitly keeps the slice single-shard, off TCP,
  off cross-shard routing, and off a broad checker DSL. Good fences.

### What is weak or missing

1. **Bootstrap re-delivery on restart is not named in the proof list.**
   `tina-runtime-current` already enqueues the bootstrap message on initial
   spawn *and* on every restart of a `RestartableSpawnSpec`
   (tina-runtime-current/src/lib.rs:996). SYSTEM.md calls this out as the
   thing that lets a replacement connection-handler child get its initial
   kick without the harness peeking at trace events. The plan mentions
   bootstrap "where relevant" in build step 1 but never lists
   "restarted child receives bootstrap again" as a direct proof. Without
   that, restart parity is silently weaker than the live runtime, and the
   listed dispatcher-style and panic/restart workloads can both pass while
   the bootstrap-on-restart path is broken.

2. **Restart-budget exhaustion is not explicitly proved.**
   The plan says supervised panic restart is "policy/budget driven" but the
   proof list only requires that *a* panic restart is reproducible. A budget
   that is never blown is not a proven budget. This slice should pin at
   least one test where the budget is exhausted and the child is *not*
   restarted, and at least one where the budget allows the restart — and
   require the artifact to reproduce both.

3. **Multi-restart reproducibility is not pinned.**
   "Saved artifact reproduces the same panic/restart run" reads as N=1.
   Real bugs here usually live at N≥2 (incarnation counter off-by-one,
   bootstrap factory called the wrong number of times, budget decrement
   leaking across restarts). The proof list should require at least one
   workload where the same child panics and restarts more than once and
   the artifact replays that exactly.

4. **Stale-identity failure shape is not pinned to event-record terms.**
   "Stale pre-restart identity fails safely" is the right behavior, but the
   plan does not say *how* the failure shows up. SYSTEM.md is explicit that
   the live runtime rejects stale generations as closed via runtime sends
   and runtime ingress. The simulator already has
   `SendRejectedReason`/closed-vs-stopped in its event vocabulary. The plan
   should name the expected reason variant so the test can assert on the
   event record rather than on a helper-state probe — otherwise this is
   exactly the surrogate proof the plan itself warns against.

5. **"Non-restartable child is skipped visibly" — visible how?**
   The plan should pin the runtime event shape (e.g., a recorded
   not-restarted event vs. silent skip). Without that, "visibly" can degrade
   into a final-state check.

6. **Interaction with 017 seeded perturbation is undecided.**
   017 made the seed semantically real over local-send and timer-wake. 018
   adds new event sources (spawn, restart, lineage). The plan does not say
   whether faults are off, additive but unperturbing, or expected to
   compose with restart in this slice. A reader cannot tell whether
   "different seeds diverge" is still a live property under 018 or only
   under 017's surfaces. At minimum, say which is true. Defaulting to
   "faults stay scoped to 017's surfaces; spawn/restart paths are not
   perturbed in 018" would be honest.

7. **"Replay reproduces the same panic/restart run from artifact config
   alone" needs an honesty note about closures.**
   `RestartableSpawnSpec::with_bootstrap` takes a
   `Box<dyn Fn() -> I::Message>` (tina/src/lib.rs:679), and `SpawnSpec`
   carries an `Isolate` value. Neither is serializable. The artifact will
   capture seed/config/event-record/checker-failure; the workload Rust code
   remains the source of factories and isolate constructors. That is fine
   — but the plan should say so out loud, the way 017 said the seed is
   "semantically real" rather than just recorded. Otherwise an outside
   reader will read "reproducible from artifact config alone" too literally.

8. **Supervisor policy variants are not pinned.**
   "Current supervision configuration shape" is broad. The plan keeps
   `RestartChildren` direct-child-only (good), but does not say which
   policy/strategy variants this slice covers and which are deferred. If
   the live runtime today only ships one effective shape, the plan should
   say "covers the shipped policy variants X, Y" so the proof matrix is
   bounded and reviewable.

9. **Restart-sensitive checker invariant is unspecified.**
   "A checker can observe a restart-sensitive invariant" is a placeholder.
   Without naming the invariant (e.g., "no accepted message attributed to
   a stopped incarnation," or "incarnation counter strictly increases per
   restart"), this can degenerate into another event-id-monotonicity
   restatement that does not actually exercise restart semantics. Pin the
   invariant.

10. **Existing 017 test isolates and `Spawn = Infallible`.**
    Today multiple simulator tests use isolates with `type Spawn =
    Infallible` (this is what makes `Effect::Spawn(spawn) => match spawn {}`
    work today). The plan does not say whether 018's simulator changes are
    additive (existing `Infallible`-Spawn isolates keep compiling unchanged)
    or whether existing tests must be migrated. It needs to. Additive is
    the only honest answer if the blast-radius claim "existing `tina-sim`
    timer/replay/fault/checker tests still pass" is to mean what it says.

11. **Equal-priority spawn ordering is unspoken.**
    The live runtime ships deterministic ordering for equal-deadline timers
    by request order. Spawn ordering when more than one `Spawn` is enqueued
    in the same step has the same shape of risk. The plan should either
    name a deterministic ordering rule for spawn dispatch in the simulator
    or cite the runtime's existing rule.

### How this could still be broken while the listed tests pass

- bootstrap message is delivered on initial spawn but not on restart;
  dispatcher workload still completes because later messages reach the
  replacement child through the dispatcher's normal sends, so nothing
  notices the missing kick
- restart "succeeds" in every test because the budget is never exhausted
- N=1 restart works; N≥2 silently corrupts incarnation or runs the
  bootstrap factory the wrong number of times
- stale-identity test asserts "send returned an error" via a helper, not
  via the event-record reason variant, so the reason can drift from
  `SendRejectedReason::Closed` to something else without anyone noticing
- non-restartable skip is "visible" only as the absence of a later-step
  child run, which a future scheduling change could mask
- 017's seeded-fault tests stay green because faults are still off in 018
  workloads, but enabling `LocalSendFaultMode::DelayByRounds` together with
  restart produces a non-deterministic event record nobody has tested
- "replay reproduces" works in-process during the same `cargo test` run
  because the bootstrap closure and isolate value are the *same* Rust
  object; round-tripping through "artifact config alone" was never
  actually attempted because closures are not serializable, and the plan
  did not call this out

### What old behavior is still at risk

- 017's seeded-fault and checker tests, if simulator changes are not
  strictly additive over `Infallible`-Spawn isolates
- 016's timer determinism, if spawn dispatch is wired into the same
  per-step ordering without the same equal-priority discipline
- live `tina-runtime-current` supervision tests, if 018 quietly negotiates
  a new public shape on `tina` (the plan says it must not — keep that gate
  honest in implementation review)

### What needs a human decision

- Whether bootstrap-on-restart is in scope for 018 or deferred. (Strong
  recommendation: in scope. The live runtime ships it, SYSTEM.md leans on
  it for the listener/connection example, and the dispatcher proof story
  is weaker without it.)
- Whether budget exhaustion is in scope for 018 or deferred to a later
  slice that owns supervision-budget surface area.
- Whether 018 simulator changes must compose with 017 seeded perturbation
  now, or whether composition is explicitly deferred.

### Recommendation

Plan is close to closeable as a Session B Plan Review 1, but should be
amended before implementation begins to:

1. add bootstrap re-delivery on restart to the direct proof list
2. add budget-exhaustion vs. budget-allowed as a direct proof pair
3. add "child panics and restarts more than once" to the direct proof list
4. pin the event-record reason for stale-identity failure
5. pin what "visibly skipped" means for non-restartable children
6. state explicitly that 017 seeded perturbation does/does not compose with
   spawn/restart paths in this slice
7. state explicitly that artifact replay assumes the same workload Rust
   binary (closures and isolate values), the way 017 was honest about
   "the seed is semantically real, not just recorded"
8. pin which supervisor policy variants are covered
9. name the restart-sensitive invariant the checker will observe
10. state that simulator changes are additive over existing
    `Spawn = Infallible` isolates so 017's tests stay green unchanged
11. name (or cite) the deterministic ordering rule for spawn dispatch when
    more than one spawn is enqueued in the same step

None of these require a new `tina` boundary. They are tightening the proof
list against the slice the plan already names.

## Implementation Review 1

Implemented 018 on top of the already-landed 017 simulator surface.

What landed:

- `tina-sim` now executes public spawn payloads:
  - `SpawnSpec<I>`
  - `RestartableSpawnSpec<I>`
  - existing `Spawn = Infallible` isolates remain additive and unchanged
- The simulator records runtime-owned direct parent-child lineage and
  restartable child records.
- `RestartChildren` now executes for direct child records.
- Supervised panic restart now uses `SupervisorConfig`, `RestartPolicy`, and
  runtime-lifetime `RestartBudget` state.
- Bootstrap messages are delivered on initial spawn and re-delivered on every
  restartable replacement.
- Stale pre-restart identity fails through the live trace shape:
  `RuntimeEventKind::SendRejected { reason: SendRejectedReason::Closed, .. }`.
- Non-restartable children are skipped through the live trace shape:
  `RuntimeEventKind::RestartChildSkipped { reason: RestartSkippedReason::NotRestartable, .. }`.
- Budget exhaustion is visible through:
  `RuntimeEventKind::SupervisorRestartRejected { reason: SupervisionRejectedReason::BudgetExceeded { .. }, .. }`.

Session B Plan Review 1 findings addressed:

- Bootstrap-on-restart is in scope and directly proved.
- Budget-allowed and budget-exhausted paths are directly proved.
- N >= 2 restart replay is directly proved.
- Stale identity and non-restartable skip are asserted through event-record
  reason variants, not helper state.
- 017 seeded perturbation remains scoped to local-send and timer-wake
  behavior; 018 does not add spawn/restart-specific perturbation.
- Replay honesty is preserved: artifacts reproduce the same event record when
  rerun with the same workload binary and simulator version; arbitrary isolate
  values and closures are not serialized.
- All shipped policy variants are covered:
  - `OneForOne`
  - `OneForAll`
  - `RestForOne`
- The restart-sensitive checker invariant is strict monotonic increase of
  replacement child isolate ids across restart completions.
- Existing `Spawn = Infallible` simulator workloads still pass unchanged.
- Same-step spawn ordering is deterministic effect order; this is asserted by
  a public-path batched-spawn test.

Proof shape that landed:

- `tina-sim/tests/supervision_simulation.rs`
  - spawned child runs on a later step
  - same-step multiple spawns preserve effect-order delivery
  - all shipped supervisor policies select the expected children
  - bootstrap is re-delivered after restart
  - repeated restarts replay the same event record and observations
  - restart-budget exhaustion is visible and reproducible
  - non-restartable child skip is visible through `RestartChildSkipped`
  - stale pre-restart identity fails through `SendRejected(Closed)`
  - `RestartChildren` preserves direct-child-only scope

Exact verification:

- `cargo check -p tina-sim`
- `cargo test -p tina-sim --test supervision_simulation`
- `cargo test -p tina-sim`

## Implementation Review 1 — Session B

Reviewed the landed implementation against Plan Review 1 and against the
behavior the live runtime already ships, by reading
`tina-sim/src/lib.rs`, the new `tina-sim/tests/supervision_simulation.rs`,
the modified `tina-sim/tests/retry_backoff.rs` / `timer_semantics.rs`, the
`tina-runtime-current` reference for spawn/supervise/restart, and the
updated SYSTEM/CHANGELOG/ROADMAP. Reran the test surface end-to-end.

### What I verified directly

- Boundary promise held. `git status` shows zero changes under `tina/`,
  `tina-supervisor/`, `tina-runtime-current/`. The simulator imports
  `RestartBudget`, `RestartPolicy`, `RestartBudgetState`, `ChildRelation`,
  `RestartableSpawnSpec`, `SpawnSpec` from `tina`, and
  `RestartSkippedReason`, `SupervisionRejectedReason` from
  `tina-runtime-current`, but adds nothing to those crates.
- `cargo +nightly test -p tina-sim` is green:
  - `supervision_simulation` 7/7
  - `faulted_replay` 4/4
  - `retry_backoff` 4/4
  - `timer_semantics` 7/7
- `make verify` exits 0 (clippy `-D warnings`, doc, loom, full workspace
  test).
- 017 isolates that declare `type Spawn = Infallible;` still compile and
  run unchanged — the new spawn dispatch is strictly additive
  (`Infallible` keeps a vacuous `IntoErasedSpawn` impl).
- 018 does not introduce a second meaning model: every new event the
  simulator emits is from the live `RuntimeEventKind` vocabulary
  (`Spawned`, `RestartChildAttempted`, `RestartChildCompleted`,
  `RestartChildSkipped`, `SupervisorRestartTriggered`,
  `SupervisorRestartRejected`). SYSTEM.md's "the simulator must use the
  real runtime event model" rule holds.

### Plan Review 1 findings: status against the implementation

1. Bootstrap re-delivery on restart — **closed**.
   `restartable_bootstrap_is_redelivered_and_repeated_restarts_replay`
   asserts `Booted` count == 3 after two restarts of one child whose
   `RestartableSpawnSpec` carries `with_bootstrap(|| WorkerMsg::Boot)`.
   The implementation calls `enqueue_bootstrap_message` from both the
   initial `ErasedEffect::Spawn` arm (lib.rs:933) and from
   `restart_child_record` (lib.rs:1260).
2. Budget exhaustion — **closed**.
   `restart_budget_exhaustion_is_visible_and_reproducible` pins the exact
   event variant: `SupervisorRestartRejected { reason: BudgetExceeded {
   attempted_restart: 2, max_restarts: 1 }, .. }`. Reproducibility is
   asserted via two runs producing identical traces.
3. Multi-restart reproducibility — **closed**.
   Same test: child panics, replacement child panics again, both restarts
   complete, two runs produce identical event records and observations.
4. Stale-identity event-record reason pinned — **closed**.
   `stale_pre_restart_identity_fails_through_send_rejected_closed_event`
   asserts `RuntimeEventKind::SendRejected { reason:
   SendRejectedReason::Closed, target_isolate, .. } if target_isolate ==
   stale_child` rather than poking helper state.
5. Non-restartable visibility — **closed**.
   `non_restartable_child_is_skipped_visibly` asserts
   `RestartChildSkipped { reason: RestartSkippedReason::NotRestartable, ..
   }` against the trace.
6. 017 fault composition — **closed as scoped**.
   SYSTEM.md now states: "Spawn/restart paths compose additively with
   017's seeded fault surfaces, but 018 does not add new spawn/restart-
   specific perturbation." Honest scope statement; matches the test
   reality (every supervision test uses `SimulatorConfig::default()`).
7. Closure honesty for replay — **partially closed**.
   The Implementation Review 1 self-report names this directly ("artifacts
   reproduce the same event record when rerun with the same workload
   binary and simulator version"), but neither SYSTEM.md nor CHANGELOG.md
   carry that sentence forward. A future reader of SYSTEM.md alone could
   still over-read "replay artifacts" as if they were closure-free. Worth
   tightening; not blocking.
8. Policy variants pinned — **closed**.
   `supervisor_policy_variants_select_the_same_children_as_the_live_runtime`
   directly exercises `OneForOne`, `OneForAll`, `RestForOne` and asserts
   restart counts against the live runtime's `ChildRelation` semantics.
9. Restart-sensitive checker invariant named — **partially closed**.
   The invariant is named (`StrictlyIncreasingRestartChecker` over
   `RestartChildCompleted.new_isolate`), and it is wired through the
   public checker path during a real restart workload, which is the
   "observe a restart-sensitive invariant" half. The "checker-triggered
   failure around panic/restart behavior" half (the adversarial proof the
   plan listed) is *not* directly proved: the only assertion is
   `is_none()` — the checker never trips. The 017 deliberate-bug harness
   covers checker-trip-and-replay on the local-send path, which keeps the
   replay-of-checker-failure machinery proved end-to-end, but no test in
   018 forces a restart-sensitive invariant to trip and replay. See the
   "still-at-risk" section below.
10. Additivity over `Spawn = Infallible` — **closed**.
    `retry_backoff.rs`, `timer_semantics.rs`, and `faulted_replay.rs` all
    still use `type Spawn = Infallible;` and pass unchanged. The two-line
    diff to `retry_backoff.rs` / `timer_semantics.rs` is just `..Default
    ::default()` for `SimulatorConfig` which is a 017 artifact, not 018.
11. Deterministic spawn ordering when multiple spawns land in the same
    step — **closed for the batch case**.
    `spawned_child_runs_on_later_step_and_batch_spawn_order_is_
    deterministic` proves that an `Effect::Batch(vec![Spawn, Spawn])`
    delivers boots in batch order on the next step. Same-step spawns from
    *different* parents are not directly tested, but execution order is
    registration order (the same rule the live runtime uses for its step
    loop); flagging as observation, not gap.

### What I verified about the implementation that was not in Plan Review 1

- "Restarted child gets a fresh incarnation" — proved through fresh
  `IsolateId` assignment per `register_entry`. `AddressGeneration` is
  always `0`, *but this matches the live runtime exactly* (line-for-line
  identical pattern at `tina-runtime-current/src/lib.rs:1146`). Generation
  is inert in both. Freshness flows through `IsolateId`. No simulator
  divergence here, despite the surface looking suspicious.
- `RestartChildren` direct-child-only scope is preserved.
  `restart_children_keeps_direct_child_scope` constructs `NestedParent ->
  ChildSpawner -> Grandchild` and shows that one call to
  `RestartChildren` on the parent restarts only the direct child
  (`ChildSpawner`); the grandchild boots exactly twice (once on each
  fresh `ChildSpawner` incarnation), and `RestartChildAttempted` count
  increments by exactly 1 — a transitive sweep would be ≥2.
- Supervised panic restart correctly stops the failed entry first, then
  reuses the cloned `Rc<dyn ErasedRestartRecipe<S>>` to build a fresh
  isolate. The recipe is preserved on the child record across restarts
  (lib.rs:1247) so subsequent panics still find a recipe — this is what
  lets the multi-restart test work.
- `supervisor_index` matches by `parent.isolate` only, not by
  `(isolate, generation)`. Today this is fine because supervisors are
  registered against parent isolates that themselves never restart in
  these workloads, but it is a latent fragility worth noting.

### How this could still be broken while the listed tests pass

- Live runtime delivers bootstrap on restart with a *fresh* factory
  invocation each time. The simulator does the same
  (`RestartableSpawnAdapter::create` lib.rs:1819 calls the bootstrap
  factory anew). However, no test asserts that the bootstrap *value*
  delivered on restart can differ from initial — every workload's
  bootstrap factory is a `|| WorkerMsg::Boot` constant. A bug where the
  first bootstrap value is silently cached and reused would not be
  caught.
- The strictly-increasing checker is only ever observed on a green path.
  A regression that produces *non*-monotonic `new_isolate` values around
  restart would presumably be caught by the equality assertion on the two
  replay traces, but that is indirect. There is no positive test that the
  checker *can* halt the run on a deliberate restart-invariant violation
  and that the resulting `CheckerFailure` round-trips through
  `replay_artifact()`. The 017 deliberate-bug harness on local-send keeps
  the checker-halt-and-replay machinery proved, so this is one assertion
  away from closed, not a structural hole.
- Same-step spawns from two unrelated parents are not directly tested.
  The implementation goes through `step()`'s registration-order loop,
  which matches the runtime's discipline, but a future change to the
  step loop could quietly reorder them.
- `restart_budget_exhaustion_is_visible_and_reproducible` only exercises
  `RestartBudget::new(1)`. Budget shapes that count *concurrent* failures
  vs. total lifetime failures are not differentiated by this proof; that
  is fine because today's `RestartBudget` is runtime-lifetime only (per
  ROADMAP), but if budget semantics ever broaden, this proof must
  broaden with them.
- Replay-from-saved-config truthfulness is asserted by re-running the
  same Rust binary with `SimulatorConfig::default()` twice, not by
  round-tripping through any serialized form. This is honest by 017
  precedent and the implementation review self-report calls it out, but
  SYSTEM.md is silent on the closure caveat (see finding 7 above).

### What old behavior is still at risk

- 016 timer determinism and 017 fault determinism: both green at this
  point. Risk is structural rather than test-shaped: 018 added a fairly
  large amount of state to `Simulator<S>` (`child_records`,
  `supervisors`, panic-driven `supervise_panic` reentry into
  `round_messages`). Future edits to the step loop must preserve the
  invariant that timer due-time harvest still happens *before* panic
  restart kicks in this step, since otherwise restart-driven sends could
  visibly race with timer wakes in a way today's tests do not pin.
- Cross-shard rejections: still out of scope, still panic. No new risk.

### What needs a human decision

- Whether the gap on item 9 (no positive checker-halt-and-replay around
  *restart* specifically) blocks 018 closeout, or is acceptable because:
  - the checker surface itself is already proved end-to-end on 017's
    local-send harness
  - the restart-sensitive invariant is wired through the public checker
    path during a real restart workload
  My read: acceptable for closeout, but worth a one-line follow-up note
  in `commits.txt` / closeout review acknowledging it as
  surrogate-supported rather than directly proved.
- Whether the closure-honesty caveat for replay should be added to
  SYSTEM.md before close, by analogy with 017's "the seed is
  semantically real, not just recorded" line. My read: yes — one
  sentence is enough.

### Recommendation

- All Plan Review 1 findings are either directly closed or honestly
  scope-stated. The implementation matches the plan it now reflects,
  reuses the live event vocabulary, and does not require any `tina`
  boundary change. `make verify` is green.
- Two small tightenings before closeout review:
  1. Add a SYSTEM.md sentence saying replay artifacts reproduce the same
     event record when rerun against the same workload binary and
     simulator version, not from serialized closures.
  2. Either add a one-test adversarial restart-checker proof
     (preferred), or note explicitly in `commits.txt` / closeout that
     restart-sensitive checker *halting* is surrogate-supported through
     the 017 local-send checker harness rather than directly proved at
     this slice.
- After those, 018 is closeable on its own terms: single-shard supervision
  parity with the live runtime, including bootstrap-on-restart,
  budget exhaustion, multi-restart, all shipped policies, and direct-
  child-only scope, replayable, additive, and inside the simulator's
  existing meaning model.

## Session A Follow-up Tightening

Addressed both Session B closeout tightenings.

- Added a SYSTEM.md replay honesty sentence: replay artifacts reproduce against
  the same workload binary and simulator version; they do not serialize
  arbitrary isolate values, spawn factories, or bootstrap closures.
- Added `restart_sensitive_checker_failure_is_replayable` to
  `tina-sim/tests/supervision_simulation.rs`. The test runs a real supervised
  panic/restart workload with an adversarial checker that fails on
  `RuntimeEventKind::RestartChildCompleted`, captures the structured
  `CheckerFailure`, reruns the same workload, and asserts the event record,
  final virtual time, and checker failure reproduce exactly.

This closes the previously surrogate-supported restart-checker halt/replay
item directly in 018.

## Session A Additional Coverage

Added direct tests for the remaining cheap edge cases called out after review:

- `restartable_bootstrap_factory_runs_fresh_for_each_replacement` proves the
  restartable bootstrap factory is invoked again for replacement children by
  returning different bootstrap payload values across initial spawn and
  restart.
- `same_step_spawns_from_different_parents_follow_registration_order` proves
  same-step spawns from unrelated parents follow the simulator/runtime
  registration-order step discipline.
- `restart_workload_composes_with_seeded_local_send_delay` proves a restart
  workload still replays under 017's existing seeded local-send delay surface;
  the delayed send reaches the replacement child and the trace/observations
  reproduce.

The remaining non-tested boundary is intentional scope, not an omitted unit
test: replay artifacts do not serialize arbitrary Rust closures or isolate
state, and arbitrary user programs/interleavings are outside this slice's
finite proof surface.

## Closeout Review 1

018 is closeable on its shipped terms.

What the phase now directly proves:

- `tina-sim` executes public `SpawnSpec` and `RestartableSpawnSpec` payloads
  without changing the `tina` trait boundary.
- Existing `Spawn = Infallible` workloads remain additive and green.
- Spawned children run on later steps.
- Same-handler batched spawns preserve effect order.
- Same-step spawns from different parents follow registration order.
- Restartable bootstrap is delivered on initial spawn and re-delivered after
  restart.
- Restartable bootstrap factories run fresh for replacement children, proven by
  differing bootstrap payloads across initial spawn and restart.
- The simulator records direct parent-child lineage and restartable child
  records against the live runtime event vocabulary.
- `RestartChildren` preserves direct-child-only scope.
- All shipped supervisor policies are exercised:
  - `OneForOne`
  - `OneForAll`
  - `RestForOne`
- Supervised panic restart is replayable across repeated restarts.
- Restart-budget exhaustion is visible and reproducible through
  `SupervisorRestartRejected { reason: BudgetExceeded { .. }, .. }`.
- Non-restartable child skip is visible through
  `RestartChildSkipped { reason: NotRestartable, .. }`.
- Stale pre-restart identity fails through
  `SendRejected { reason: Closed, .. }`.
- A restart-sensitive checker can halt a real restart run, and the resulting
  `CheckerFailure` replays exactly.
- Restart workloads compose with 017's seeded local-send delay surface and
  reproduce trace/observation output.

What remains intentionally out of scope:

- TCP simulation.
- Multi-shard simulation.
- Serialization of arbitrary Rust isolate values, spawn factories, or
  bootstrap closures. Replay artifacts reproduce against the same workload
  binary and simulator version.
- Exhaustive proof over arbitrary user programs or every interleaving.

Verification at closeout:

- `cargo test -p tina-sim --test supervision_simulation` passes with 11 tests.
- `cargo test -p tina-sim` passes.
- `cargo check --workspace` passes.
- `make verify` passes, including workspace tests, loom, docs, and clippy
  `-D warnings`.

No 018 human-escalation items remain.
