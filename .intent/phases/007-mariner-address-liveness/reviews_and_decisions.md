# Reviews and Decisions: 007 Mariner Address Liveness

## Round 1: Spec Diff Review

Artifact reviewed:

- `.intent/phases/007-mariner-address-liveness/spec-diff.md`

Read against:

- `.intent/SYSTEM.md`
- `.intent/phases/006-mariner-parent-child-lineage/` (the prior
  supervision groundwork slice)
- `tina-runtime-current/src/lib.rs` (current `register`,
  `spawn_isolate`, `try_send`, and dispatch behavior)
- `tina/src/lib.rs:Address<M>`, `IsolateId`, `ShardId` shape
- `ROADMAP.md` ("Strategic prerequisites: Decide the address
  liveness story before supervision")

### Positive conformance review

Judgment: passes

- **P1.** Right slice in the right place. ROADMAP explicitly names
  address liveness as a strategic prerequisite to supervision.
  Slice 006 landed parent-child lineage; this slice closes the
  other supervision groundwork. Both must exist before
  `RestartChildren` execution can be honest.
- **P2.** Address-as-incarnation is faithful to Tina-Odin's
  stale-handle property. The semantic choice — "Address<M> names
  one physical isolate incarnation, not a logical role" — is the
  central commitment that lets supervision restart children
  without breaking the type-system contract that an address
  targets a specific recipient.
- **P3.** No new trace events. The slice uses existing
  `SendRejected { reason: Closed }` for stale-known and the
  existing programmer-error panic for unknown. Disciplined —
  doesn't widen the trace vocabulary just because there's a new
  semantic distinction to draw.
- **P4.** Explicit forward gate: "If a future runtime ever wants
  to reuse isolate ids, it must first add a generation or
  equivalent liveness token." Names the constraint that future
  slices must respect rather than letting it be discovered when
  someone tries to add compaction.
- **P5.** Negative-space discipline broad and right: no generation
  field on `Address<M>`, no public-shape change to `Address<M>` /
  `SendMessage<M>` / `IsolateId` / `ShardId`, no logical service
  names, no aliases, no registries, no refresh APIs, no
  RestartChildren execution, no supervisor mechanism.
- **P6.** Verification criteria are testable and concrete:
  stop + ingress = `Closed`; dispatched send to stopped =
  `SendRejected{Closed}`; later spawn gets fresh id; old address
  after later spawn still targets old closed incarnation;
  synthetic id panics; deterministic reruns.

### Negative conformance review

Judgment: passes with three things to pin

- **N1. u64 wraparound is not addressed.** Spec says ids are
  "monotonic within one runtime lifetime" but does not pin what
  happens at the type boundary. Operationally this is irrelevant
  (~580,000 years at 1M spawns/sec sustained), but the spec should
  say so explicitly: "u64 wraparound is out of scope for this
  slice; the practical bound is the runtime lifetime, not the
  type." Otherwise a future reviewer wonders if there is a
  defined panic on overflow.
  *Class: agent-resolvable.*

- **N2. "Distinguishing stale-known from unknown" relies on the
  *absence* of an event in the unknown case.** Spec says the
  trace/proof vocabulary distinguishes the two using existing
  outcomes. True — but the distinction is asymmetric: stale-known
  produces `SendRejected{Closed}`; unknown panics and produces
  *no trace event* because the panic propagates out of `step()`.
  Worth pinning explicitly: "unknown-isolate panics propagate;
  the runtime does not record `SendRejected` or any other trace
  event for them." Without that, a future reader might think
  both outcomes should produce trace events.
  *Class: agent-resolvable.*

- **N3. The forward commitment to restart relies on restart
  slices respecting fresh-id allocation.** Spec says "the
  replacement gets a fresh address" but slice 007 does not
  implement restart. Worth one sentence pinning: "the no-reuse
  invariant in slice 007 is the foundation for restart's
  fresh-id requirement; future restart slices must mint fresh
  ids via the existing `register` / spawn paths and must not
  reuse a stopped isolate's id." Otherwise a future
  RestartChildren slice might quietly add an "in-place
  replacement" path that breaks this invariant.
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, with two angles worth being honest about

- **A1. Address-as-incarnation forecloses implicit logical
  naming.** Application code that wants "send to the worker for
  tenant 7, whichever incarnation is currently alive" cannot get
  it from the runtime. It has to build a registry isolate that
  maps logical names to current incarnation addresses. This
  matches Tina-Odin (and Erlang) — both rely on app-level
  registries — but it is a real product feel. Spec should say so
  honestly: "logical naming is a user-space concern; tina-rs
  does not provide it. Applications that need logical names
  build a registry isolate themselves."
  *Class: human-escalation.*

- **A2. No-reuse + lineage retention = monotonic runtime memory
  growth.** Slice 006's lineage entries persist after stop;
  slice 007 commits to never reusing isolate ids. Together, the
  registry grows monotonically with spawn count over the
  runtime's lifetime. There is no compaction. For long-running
  runtimes with high spawn churn, this is a real footprint
  concern. Slice 007 is the right place to commit to "either
  accept monotonic growth, or commit to a future generation-token
  slice that enables compaction." Spec should make this explicit
  rather than leaving it implicit alongside slice 006's similar
  deferral.
  *Class: human-escalation.*

What I checked and found defended:

- The implementation already satisfies most of slice 007's
  intent without code change: `next_isolate_id` increments
  monotonically, stopped isolates' mailboxes return `Closed`,
  unknown isolate ids panic. The slice mostly codifies existing
  behavior as deliberate intent.
- Slice 006's `restart_children_remains_observed_only_after_-
  lineage_lands` test stays valid; this slice does not execute
  RestartChildren.
- Single-cause trace shape preserved.
- `tina` crate untouched — no new public API surface.

### Autonomy assessment

The slice's autonomy note correctly classifies itself as
escalation-class (semantic commitment about address meaning).
The two human-escalation findings (A1, A2) reinforce that
classification: both are product positioning, not implementation
mechanics.

The three agent-resolvable findings (N1, N2, N3) can be folded
into the slice by Session A without re-escalation: each is a
one-sentence clarification that does not change intent, only
makes existing intent harder to misread.

---

## Open Decisions

Two human-escalation decisions before the slice is implementable
as written. Both are product positioning, not implementation
choices.

1. **Logical naming is user-space, not runtime-provided.** Slice
   007 commits the runtime to "Address<M> = one incarnation."
   Apps that want logical names ("send to tenant 7's current
   worker") build their own registry isolate. This matches
   Tina-Odin and Erlang. Decide: is this the position you want
   tina-rs to ship with? It affects what tina-rs *feels like* to
   use. Once Address<M> is documented as incarnation-only, adding
   logical naming later requires reopening this commitment.

2. **Monotonic registry growth for the runtime's lifetime.** The
   no-id-reuse commitment from slice 007, combined with slice
   006's lineage retention, means the registry grows monotonically
   with spawn count. There is no compaction. For long-running
   runtimes with high spawn churn, this is a real memory
   footprint. Decide: accept monotonic growth and document it as
   a known limitation, or commit to a future generation-token
   slice that enables compaction. Today's implicit choice is the
   former; slice 007 should make it explicit either way.

The three agent-resolvable findings (u64 wraparound out of scope,
unknown-isolate panics produce no trace event, future restart
slices must mint fresh ids) are not blockers and can be folded
into the slice's spec by Session A inside the active phase
without re-escalation.

---

## Decision Round 1: Human Follow-Up and Spec Update

Human direction:

- Do not punt immediately obvious foundations.
- If the liveness-token fix is the right foundation, do it in this slice
  instead of documenting a permanent monotonic-growth limitation.

Decisions:

- **A1 accepted.** Logical naming remains user-space. `Address<M>` names one
  isolate incarnation. Applications that need "the current worker for this
  logical thing" should build that as a registry isolate or equivalent
  application-level pattern.
- **A2 revised.** The old spec accepted monotonic no-reuse as the whole
  liveness story. That was weaker than necessary. The spec now adds an address
  generation token in slice 007, so future compaction or id reuse has a correct
  stale-address foundation. This slice still does not implement compaction or id
  reuse.
- **N1 accepted.** The spec now says isolate-id and generation wraparound are
  out of scope.
- **N2 accepted.** The spec now says unknown-isolate panics propagate and do not
  record trace events.
- **N3 accepted in revised form.** Future restart slices must mint a fresh
  address identity. Reusing an isolate id is allowed only with a fresh
  generation and closed stale old-generation addresses.

Artifact changes:

- Updated `spec-diff.md` to make generation part of `Address<M>` identity.
- Updated `spec-diff.md` to require generation-aware runtime lookup.
- Updated `spec-diff.md` to require send trace target identity to include
  generation.
- Updated negative space to keep compaction/id reuse out of this slice while
  preserving future compatibility.

---

## Round 2: Plan Review

Artifact reviewed:

- `.intent/phases/007-mariner-address-liveness/plan.md`

Read against:

- `.intent/phases/007-mariner-address-liveness/spec-diff.md`
- `tina/src/lib.rs`
- `tina-runtime-current/src/lib.rs`

### Positive conformance review

Judgment: passes

- **P1.** The plan maps the revised generation-token spec directly to code:
  `AddressGeneration`, generation-aware runtime entries, generation-aware
  lookup, erased-send propagation, and send trace payload updates.
- **P2.** The plan preserves the slice boundary. It explicitly avoids
  compaction, id reuse, `RestartChildren`, restart records, supervisor
  machinery, logical names, and cross-shard semantics.
- **P3.** The test plan covers both stale-known paths: stopped current
  generation and known isolate id with wrong generation. That is the core proof
  this slice needs.

### Negative conformance review

Judgment: passes with one agent-resolvable fix

- **N1. `Address::new` initial-generation behavior belongs in the spec.** The
  plan keeps `Address::new(shard, isolate)` as an initial-generation
  constructor and adds an explicit constructor for non-initial generations. That
  is the right compatibility shape, but it is public API meaning, not merely
  implementation reasoning. Move that sentence into `spec-diff.md` before
  implementation.
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes

- **A1. Event subject identity remains shard + isolate id only.** The plan calls
  this out as an ambiguity and contains it by not implementing id reuse or
  compaction in this slice. That is acceptable. A future slice that actually
  reuses isolate ids must revisit event subject identity before enabling reuse.
  *Class: agent-resolvable if it becomes a future slice note; no current
  blocker.*

### Decisions

- **N1 accepted.** Update `spec-diff.md` to say `Address::new` constructs an
  initial-generation address and the explicit generation constructor exists for
  runtime-issued/non-initial addresses and tests.
- **A1 accepted as a future constraint.** No implementation change in this
  slice. Id reuse/compaction remains out of scope.

No human-escalation findings remain in this plan review.

---

## Round 3: Implementation Review

Artifact reviewed:

- `tina/src/lib.rs`
- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/address_liveness.rs`
- updated runtime trace assertions in existing tests
- verification output from `make verify`

Read against:

- `.intent/phases/007-mariner-address-liveness/spec-diff.md`
- `.intent/phases/007-mariner-address-liveness/plan.md`

### Positive conformance review

Judgment: passes

- **P1.** `Address<M>` now includes an `AddressGeneration`, exposes
  `generation()`, keeps `Address::new` as the initial-generation constructor,
  and adds `Address::new_with_generation` for explicit generation-aware
  construction.
- **P2.** `CurrentRuntime` stores generation per registered entry and returns
  runtime-issued addresses with the entry generation.
- **P3.** Runtime ingress and local send dispatch both match isolate id plus
  generation. Known-id wrong-generation targets are treated as stale-known and
  fail as `Closed`.
- **P4.** Send trace variants now carry `target_generation`, preserving enough
  target identity for generation-aware address liveness.
- **P5.** Focused address-liveness tests prove initial generation, stopped
  address closure, wrong-generation ingress rejection, wrong-generation
  dispatch rejection, trace generation payloads, unknown-ingress panic behavior,
  and deterministic reruns.

### Negative conformance review

Judgment: passes with one artifact wording fix

- **N1. Unknown dispatch trace wording was too strong.** The spec/plan briefly
  said unknown-isolate panics record no trace event. That is true for runtime
  ingress, but not for dispatched sends: existing behavior records the handler
  chain and `SendDispatchAttempted` before the programmer-error panic
  propagates. The important invariant is that unknown targets do not become
  `SendRejected` or any other recoverable runtime outcome.
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes

- **A1. Generation exists before id reuse exists.** This is intentional. The
  slice installs and proves the public liveness foundation without implementing
  compaction or id reuse. Current generation is always initial in normal
  registration, while wrong-generation tests prove lookup behavior before a
  future restart/id-reuse slice depends on it.
- **A2. Context-created addresses remain initial-generation addresses.** This is
  acceptable for this slice because id reuse is not implemented and runtime
  issued addresses remain the correct source of current incarnation identity. A
  future slice that reuses ids must revisit context address helpers before
  relying on non-initial current generations.

### Decisions

- **N1 accepted.** Updated `spec-diff.md` and `plan.md` to distinguish runtime
  ingress unknowns, which record no trace, from dispatched unknown sends, which
  may retain pre-panic handler/attempt trace but never record `SendRejected`.

No human-escalation findings remain. The implementation is accepted for this
slice.

### Evidence

- `make verify` passed.
