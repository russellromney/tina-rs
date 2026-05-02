# 022 Review

Session:

- B (review)

## Plan Review 1

Artifact reviewed:

- `.intent/phases/022-kepler-core-primitive-completion/plan.md`

Reviewed against `.intent/SYSTEM.md`, the Galileo (020) implementation
that just landed, and the IDD framework rules in `idd/README.md`.

I also checked the plan's claim that the former Galileo closeout
surrogate-supported notes (per-isolate-pair FIFO across shards,
non-default-seed multi-shard replay, multi-shard composition with
017/019 fault surfaces) are already direct 020 evidence.
`24cfb89 Complete Galileo proof coverage` adds:

- `cross_shard_harvest_preserves_fifo_per_isolate_pair_with_multiple_sources_and_targets`
- `multishard_dispatcher_composes_with_seeded_timer_faults`
- `same_non_default_seed_faulted_multishard_dispatcher_replays_same_artifact`
- `different_seeds_can_diverge_in_faulted_multishard_dispatcher_replay`
- `multishard_tcp_workload_composes_with_seeded_tcp_completion_faults`
- `multishard_supervision_workload_composes_with_seeded_local_send_delay`

The baseline is honest. Kepler can stand on those without re-proving.

### What looks strong

- Phase identity is named once and named loudly: "finish the
  concurrency primitive before building bridges around it." That
  makes the phase a settlement phase, not a feature phase.
- "Why This Comes Before Apollo" earns its title. Apollo is bridge
  work; bridges have to compare against a stable contract; Kepler
  closes core uncertainty so Apollo has something stable to compare
  against. Real ordering rationale, not filler.
- The five gap list maps to real loose ends from prior phases. None
  of them is invented:
  - peer/shard liveness — Galileo explicitly deferred it; SYSTEM.md
    says full peer-quarantine semantics are later work
  - supervision boundary — Galileo kept parent/child shard-local and
    out of scope unless extended
  - buffering/allocation honesty at runtime level — SYSTEM.md says
    "no broad runtime allocation claim is supported yet"
  - checker pressure on harder semantics — 020 IR1 flagged exactly
    these surrogate-supported gaps; `24cfb89` closed them, so Kepler
    inherits a real baseline
  - boundary changes — gated to "make existing semantic truth
    explicit"
- Plan refuses adapter sugar, benchmark theater, polished docs,
  publish work, examples-as-proof. The right fences for a primitive-
  closing phase.
- Build steps are decision-first. "Write down the model... before
  broad implementation proceeds." That matches IDD's "state the
  change, state the non-change, build it" sequence.
- Plan does not treat 020's tightening commit as forgotten. It
  leans on it explicitly.

### What is weak or missing

1. **Five distinct semantic decisions in one phase.**
   Each of the five gaps is the size 016-019 paid for as one
   slice. Kepler asks for liveness vocabulary plus supervision-
   boundary decision plus runtime allocation audit plus checker
   pressure plus boundary change. That is materially larger than
   any prior phase. The plan should either:
   - acknowledge the size honestly ("intentionally the largest
     semantic phase to date; review bar scales accordingly"), or
   - split into Kepler-A and Kepler-B with explicit ordering, or
   - pick one or two gaps as load-bearing for this slice and
     defer the rest with named follow-up phases.
   My read: split. Liveness alone is a big distributed-systems
   question. Supervision-boundary alone is a big design question.
   Bundling them risks 022 closing on the easier two and quietly
   carrying the rest forward.

2. **Decision-first paths per gap, but the expected direction is
   not pinned.**
   Each gap can land as "decide and seal" (small slice) or
   "decide and extend" (big slice). The plan does not say which
   way each gap is expected to go. If both liveness and
   supervision-boundary seal, Kepler is a writing-down phase and
   the implementation surface is small. If both extend, the
   surface is a new public vocabulary. Pin expected direction
   per gap. Pause-gate the alternative: "if extending the
   supervision boundary requires more than a small additive
   effect or new shard-level event, pause and split."

3. **"Liveness" in a single-process explicit-step coordinator.**
   `tina-rs` Galileo is synchronous, single-threaded, in-process.
   A peer shard cannot become "unavailable" in the distributed
   sense yet. The only liveness signal available today is "an
   isolate panicked or stopped." Kepler's liveness gap is really
   one of these:
   - reserve a vocabulary now (shard-down / peer-restarted
     events) so a future substrate can populate it without
     retrofitting, or
   - decide there is no liveness signal in 022 because the
     coordinator is synchronous, and seal that as a deliberate
     non-feature
   Either is honest. The plan reads as if it expects the first
   without saying so. Pin which. Otherwise readers will conflate
   `tina-rs` with distributed-systems liveness, and the proof
   plan's "deterministic and directly observable" line is hard
   to interpret.

4. **No pause gates.**
   020 had four explicit pause gates ("if honest semantics
   require a public boundary change, pause"; "if substrate
   engineering creeps in, pause"). 022 has none. Add pause
   gates that match the slice size:
   - if liveness semantics require heartbeats, gossip, or
     networking primitives, pause
   - if supervision-boundary extension requires more than a
     small additive constructor / effect variant, pause
   - if allocation audit reveals a structural issue requiring
     redesign of effect erasure, pause
   - if checker pressure needs a new fault-injection language
     broader than 017's `Checker`, pause
   - if any one gap takes more than its share of the slice and
     the others get pushed to "later," pause and split
   Pause gates are how a large slice protects itself from
   silent expansion.

5. **Allocation audit needs a measurement plan.**
   "Audit runtime-core allocation/buffering behavior" is a
   verb without a method. Pick a tier:
   - static read of the per-step code paths and a written
     claim about which paths allocate
   - test probes that count allocations on hot paths
   - both
   020 left the runtime-level allocation story narrow on
   purpose. Kepler should pick a tier and pin it so the
   resulting claim is reviewable. Without this, "audit" closes
   on prose rather than on evidence.

6. **Adversarial bug should be pinned to a Kepler-introduced
   semantic.**
   "At least one deliberately-injected multi-shard semantic bug
   that hand-authored happy-path tests would miss." Good
   adversarial framing. But which semantic should the bug
   target? Pin: the bug should expose a regression in the new
   liveness or supervision rule, not a 020-era invariant
   already covered by the tightening commit. Otherwise the
   implementation can pick whichever bug is cheapest and still
   call this requirement met.

7. **Checker pressure tooling — what new simulator surface
   widening lands?**
   017 added the `Checker` trait. 020 did not extend
   checkers. Cross-shard delivery perturbation was deferred
   from 020 explicitly. Kepler's plan asks for "seeded or
   scripted perturbation" against new semantics. Decide:
   - does Kepler add a cross-shard delivery perturbation
     surface?
   - or new checkers tailored to liveness/supervision
     events?
   - or both?
   Pin which surface widening lands. Otherwise the simulator
   pressure proof can default to existing 017/019 surfaces,
   which is exactly what gap 4 says it must not do.

8. **Stable shard ownership under cross-shard child placement.**
   020 pinned "once an isolate is placed, its shard ownership
   remains stable for its lifetime." If Kepler extends the
   supervision boundary (option 2b), what happens to that
   rule? Pin: stable ownership stays even with cross-shard
   placement, or stable ownership relaxes for spawned
   children, or extension explicitly does not relax it. This
   is a load-bearing invariant from 020 and 022 should not
   silently widen it.

9. **Replay artifact honesty carry-forward.**
   016/017/018/019/020 each carried the line: replay
   reproduces against the same workload binary and simulator
   version, not from serialized payloads. New 022 artifacts
   should carry the same honesty so a reader does not over-
   interpret "reproducible from artifact alone."

10. **Boundary change guidance needs at least one example.**
    "Good boundary changes" / "Bad boundary changes" lists are
    useful but abstract. A reviewer cannot tell from them
    whether a specific rename qualifies. Add one concrete
    example for each list, or a sharper heuristic ("if a
    proposed change makes no test pass that does not already
    pass, reject it as adapter sugar").

11. **No proof-modes section.**
    016-020 each named the proof modes they expected (unit /
    integration / e2e / adversarial / replay / blast-radius).
    022 lists proofs but not the modes they fall under.
    Adding the section keeps Kepler reviewable in the same
    vocabulary as prior phases.

12. **Done Means lacks closeout-artifact specifics.**
    020's Done Means named concrete tests and SYSTEM.md
    sections. 022's Done Means says "directly proved" but
    not "via test names X, Y, Z" or "via SYSTEM.md sections
    A, B." A future closeout reviewer should be able to
    check the artifact list directly.

13. **Phase framing: completion vs settlement.**
    Title says "Core Primitive Completion." Body is mostly
    settlement. The two are close but not identical:
    completion implies adding things; settlement implies
    sealing things. Plan should pick the framing it actually
    means. My read: settlement is dominant. The peer-liveness
    gap might add small vocabulary, but the supervision-
    boundary gap and the allocation audit are mostly about
    sealing existing implicit truths. Worth saying that
    completion-via-sealing is the dominant mode.

14. **What Will Not Change should preserve 020's stable-
    ownership rule unless extension is taken.**
    Add explicitly: 020's "once placed, shard ownership is
    stable" remains the default unless gap 2 takes the
    extension path, in which case the new semantics must
    pin the rule's replacement.

### How this could still be broken while the listed tests pass

- Kepler closes on whichever two gaps are easiest (probably
  the allocation audit and the checker pressure) and
  carries liveness + supervision-boundary forward as
  "addressed in writing" without code change. Tests pass;
  the primitive is not actually settled. Mitigation: pause
  gate per finding 4; explicit per-gap closure status in
  Done Means.
- "Liveness semantics are deterministic and directly
  observable" passes because the test asserts the absence
  of any liveness event in a happy run. Mitigation:
  finding 6 — pin the deliberately-injected bug at the new
  semantic, not at a 020-era invariant.
- "No new hidden unbounded queues" passes because the
  audit was a prose claim, not a test. A future commit
  introduces an unbounded buffer and the claim survives.
  Mitigation: finding 5 — pick a measurement tier.
- Cross-shard child placement extends the supervision
  boundary; tests prove "spawn on remote shard works"; but
  no test pins what happens to the placed child's
  generation, parent linkage on the source shard, or
  restart policy under shard-pair queue overflow.
  Mitigation: finding 2 + finding 8.
- Kepler ships a new shard-level event vocabulary;
  existing 020 traces gain new events; 017's structural
  monotonicity checker stays meaningful; but no test pins
  the cause linkage shape between a shard-down event and
  the affected isolate's pre-existing trace. Replay
  reproduces the new events because they are new, not
  because they are correct. Mitigation: finding 6 again.

### What old behavior is still at risk

- 020 stable-ownership rule under any cross-shard child
  placement extension (finding 8).
- 017 `Checker` trait and 019 `TcpCompletionFaultMode`
  composition with whatever new perturbation surface
  Kepler adds (finding 7). Composition should remain
  additive, not entangled.
- 018 supervision policy variants (`OneForOne`,
  `OneForAll`, `RestForOne`) under any cross-shard
  extension. Their meaning is shard-local today; if
  Kepler extends across shards, the meaning of "all" or
  "rest" needs explicit redefinition.
- The blast-radius bar: every prior 016-020 test stays
  green without rewrite. State this explicitly so
  implementation review has a sharp gate.

### What needs a human decision

- Whether to split Kepler. My recommendation: split.
  Kepler-A: allocation audit + checker pressure on existing
  020 invariants. Kepler-B: liveness vocabulary +
  supervision boundary + boundary changes. The two halves
  are independently reviewable and let the harder
  semantic question (B) absorb its own review bar.
- Whether liveness in 022 is "reserve vocabulary now" or
  "explicitly no signal yet." (finding 3)
- Whether supervision boundary extends or seals. (finding 2)
- Allocation audit measurement tier. (finding 5)
- New checker/perturbation surface scope. (finding 7)

### Recommendation

Plan is structurally on-shape: clear identity, real loose
ends, decision-first build steps, refusal of adapter and
benchmark theater. The phase ordering rationale (Kepler
before Apollo) earns its place.

Not yet ready to hand off to implementation. Two
structural decisions are pending: whether to split, and
how each gap is expected to land.

Amend the plan before implementation begins to:

1. Choose phase shape: split into Kepler-A and Kepler-B,
   or accept "largest semantic phase to date" with the
   matching review bar.
2. Pin each gap's expected direction (seal vs extend),
   with pause-gate language for the alternative.
3. Pin what "liveness" means in a single-process
   synchronous coordinator: reserved vocabulary or
   explicit non-feature.
4. Add pause gates against silent slice expansion and
   substrate creep.
5. Pick an allocation-audit measurement tier (static,
   probe, both).
6. Pin the deliberately-injected adversarial bug to a
   Kepler-introduced semantic.
7. Pin the new simulator perturbation/checker surface
   scope.
8. State explicitly that 020's stable-ownership rule
   remains unless extension takes it; if extension is
   taken, redefine it.
9. Carry forward replay-artifact honesty.
10. Add at least one concrete example to the boundary-
    change guidance lists.
11. Add a proof-modes section using 016-020's vocabulary.
12. Sharpen Done Means with named closeout artifacts
    (test names, SYSTEM.md sections).
13. State explicitly whether the phase frame is
    "completion" or "settlement."
14. Update What Will Not Change to preserve 020 invariants
    that are not deliberately being changed.

Most of these are one or two sentences. Findings 1, 2, 3,
and 7 are the load-bearing ones; the rest are tightening.
After those, Kepler is reviewable as a real settlement
phase rather than as a five-front semantic discussion.
