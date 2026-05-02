# 022 Kepler Core Primitive Completion Plan

Session:

- A

## What We Are Building

Build the next core phase after Galileo:

> **finish the concurrency primitive before building bridges around it**

Kepler exists because Galileo made `tina-rs` honestly multi-shard, but a few
core edges are still not settled enough for bridges or publish claims.

This 022 slice is **Kepler-A: settlement by sealing**.

Expected direction:

- peer/shard liveness stays absent in the current explicit-step runner
- supervision stays shard-local
- stable shard ownership stays intact
- runtime buffering/allocation claims get narrowed or proven
- simulator/checker pressure targets those sealed rules
- no `tina` public boundary change is expected

If implementation discovers that any of those "seal it" decisions is wrong,
pause and split a Kepler-B extension slice instead of quietly growing 022.

This phase is intentionally **not** a docs/examples/polish phase.
It is also intentionally **not** the Tokio bridge phase.

Kepler should answer the remaining core questions that still sit too close to
the primitive itself:

- what peer/shard liveness does **not** mean yet in the explicit-step runner
- how far multi-shard supervision goes before real substrate liveness exists
- how the current shard-local child/restart rule is preserved
- what the real allocation/buffering story is in the runtime core, not just in
  the mailbox crate
- whether simulator/checker/replay support is strong enough to pressure these
  sealed rules rather than only happy-path demos

The story Kepler must tell is:

> Tina's current core primitive has fewer loose edges: no fake shard liveness,
> no hidden cross-shard child ownership, no vague runtime allocation story, and
> replay/checker tests that pressure those facts.

## What Will Not Change

- This phase does **not** prioritize guides, examples, onboarding prose, or
  marketing-quality documentation.
- This phase does **not** build the Tokio bridge.
- This phase does **not** chase benchmark theater or production performance
  claims beyond what is needed to make buffering/allocation semantics honest.
- This phase does **not** add broad convenience APIs merely because they feel
  nice.
- This phase does **not** add a second concurrency model beside the existing
  synchronous handler / explicit effect / bounded mailbox model.
- This phase does **not** change Galileo's stable-ownership rule: once an
  isolate is placed, its shard ownership stays stable for that incarnation.
- This phase does **not** add cross-shard spawn placement or cross-shard
  restart ownership by default.
- This phase does **not** add heartbeats, gossip, networking probes, or a
  real-substrate shard-down signal.
- This phase does **not** reopen 021's general syntax direction unless a core
  semantic gap truly forces it.
- This phase does **not** treat examples as proof. Any example work that lands
  here should exist only because a proof workload needs it.

## Size and Review Bar

The full Kepler problem is larger than one normal phase. Peer liveness,
supervision boundaries, allocation claims, checker pressure, and public boundary
changes could each become their own phase if extended.

022 deliberately takes the smaller settlement path:

- seal the liveness and supervision boundaries for the explicit-step model
- make allocation/buffering claims reviewable
- add checker/replay pressure around those sealed rules

If any one gap turns into a real feature extension, stop and split.

## Why This Comes Before Apollo

Apollo is outward-facing work. It asks how Tina fits into someone else's
runtime world.

Kepler is inward-facing work. It asks whether Tina's own primitive is fully
settled on the questions that most directly define its semantics and trust
story.

That order matters.

If we do Apollo first, the bridge has to explain weakened or preserved
guarantees against a core that still has a few important edges marked "later."
If we do Kepler first, Apollo gets to compare against a more stable primitive.

## Core Gaps Kepler Should Close

Kepler should close or deliberately seal the following gaps.

### 1. Peer / shard liveness boundary

Galileo explicitly stopped before full upstream-style peer quarantine /
shard-restarted propagation semantics.

022 pins the smaller story:

- the current explicit-step runner has no peer-unavailable signal
- address-local failures stay address-local
- an unknown, stopped, stale, or full remote target does not poison the whole
  destination shard
- there is no shard-down, peer-down, shard-restarted, or peer-restarted event
  vocabulary in 022
- runtime and simulator must agree on that absence

Proof should make the absence useful, not vague. A bad remote target should
fail visibly, and a later good send to the same shard should still work.

### 2. Multi-shard supervision boundary

Galileo kept parent/child execution shard-local.

022 seals that as the current rule:

- `MultiShardRuntime::supervise(parent, config)` routes root config to the
  shard that owns `parent`
- `MultiShardSimulator::supervise(parent, config)` does the same
- children spawned by a parent belong to the parent's shard
- restart policy applies to direct children on that shard
- cross-shard child placement remains forbidden / not expressible
- stable shard ownership remains unchanged

If implementation needs cross-shard child placement or remote restart
ownership, pause and split. That is Kepler-B scale work.

### 3. Cross-shard ownership and buffering honesty

Galileo proved bounded shard-pair transport and source-time vs destination-time
staging, but the broader ownership/copy boundary still wants a cleaner,
explicit statement.

022 must pin:

- whether cross-shard transport is clone-based, move-based, or mixed by design
- what parts of that are merely implementation detail versus semantic contract
- whether any hidden buffering or unexpected copying remains in runtime-core
  paths
- what exact runtime-level allocation claim we are comfortable making, if any

This is where we stop repeating mailbox-only allocation truths as if they
already described the runtime as a whole.

Measurement plan:

- static audit of the multi-shard send/harvest/replay paths
- focused allocation probes for the hot multi-shard path where practical
- explicit list of known allocations that remain allowed
- explicit statement of any claim we are **not** making

### 4. Multi-shard replay/checker pressure on the real semantics

Galileo already gives us deterministic multi-shard replay and a user-shaped
workload.

022 should make the simulator pressure the sealed rules, not only happy paths:

- address-local remote failure does not become shard liveness
- cross-shard supervision stays shard-local
- source-time queue entry and destination-time delivery stay separate
- no new shard-level event vocabulary appears unless the phase is split

If a semantic claim matters, Kepler should try to make it fail under replayable
pressure before we call it settled.

The former Galileo closeout proof notes are already direct 020 evidence:
per-isolate-pair FIFO across a shard pair, non-default seeded multi-shard
replay, and composition with existing seeded timer/local-send/TCP fault
surfaces. Kepler should not spend its first slice re-proving those. It should
use that stronger baseline to pressure the remaining core gaps.

### 5. Narrow boundary changes only if the primitive needs them

Kepler is allowed to change the `tina` abstraction boundary, but only where the
core primitive itself is still under-specified.

No `tina` public boundary change is expected in 022.

Good boundary changes in Kepler would be things like:

- make an existing semantic truth explicit
- remove an ambiguity around shard/peer/core ownership
- pin a real concurrency invariant
- example: a tiny shared type that names a proven shard-local ownership rule

Bad boundary changes in Kepler would be things like:

- convenience surface drift
- adapter-friendly sugar without a core semantic reason
- premature publish/bridge stabilization
- example: a helper that only makes future Tokio bridge code nicer

If a proposed boundary change does not make a new core test possible, reject it
or pause for review.

## Checker and Fault Scope

022 should add checker pressure, not a broad new fault language.

Expected path:

- add narrow event-stream checkers for the liveness boundary and supervision
  boundary
- use existing seeded simulator surfaces where they help compose with those
  boundaries
- do not add cross-shard delivery perturbation as a general DSL in 022

If the checker work needs a new broad cross-shard fault-injection language,
pause and split.

The deliberately injected bug must target a Kepler-sealed rule. Good targets:

- address-local remote failure accidentally poisons a whole shard
- remote supervision accidentally restarts or owns a child across shards
- source-time queue entry and destination-time delivery get collapsed again

Do not satisfy this proof by breaking a 020 invariant that is already covered.

## Pause Gates

Pause and split before implementation continues if:

- liveness work requires heartbeats, gossip, networking probes, or real
  substrate shard-down signals
- liveness work adds shard-down / peer-restarted public event vocabulary
- supervision work requires cross-shard child placement or remote restart
  ownership
- allocation audit finds a structural issue in effect erasure or replay storage
  that needs redesign rather than measurement
- checker work requires a new broad cross-shard fault language
- a `tina` public boundary change is needed
- one gap expands enough that the remaining gaps would close only in prose

## Build Steps

1. Update `.intent/SYSTEM.md` with the sealed 022 rules:
   - no shard/peer liveness signal in the explicit-step runner
   - address-local remote failure does not poison a shard
   - supervision and child ownership stay shard-local
   - stable shard ownership remains the default rule
2. Prove the liveness boundary in `tina-runtime` and `tina-sim`:
   - bad remote target fails visibly
   - later good traffic to the same shard still succeeds
   - no shard-level liveness event is emitted
3. Prove the multi-shard supervision boundary:
   - root `supervise` routes to the owning shard
   - spawned/restarted children stay on the parent's shard
   - cross-shard child ownership remains forbidden or not expressible
4. Audit runtime-core allocation/buffering behavior on the multi-shard path:
   - static code-path audit
   - focused allocation probes where practical
   - explicit list of allowed allocations and non-claims
5. Add narrow checker/replay pressure around the sealed rules.
6. Add one adversarial bug proof targeted at a Kepler-sealed rule.
7. Update `CHANGELOG.md` and any closeout artifact with concrete test names and
   the final allocation/buffering claim.

## Proof Plan

Kepler should prove the primitive, not just describe it.

### Runtime proofs

- address-local remote failures are directly observable and deterministic
- address-local remote failures do not create shard-level liveness facts
- later valid traffic to the same destination shard still succeeds
- root supervision routes to the owning shard
- spawned and restarted children stay on the parent's shard
- cross-shard child placement remains forbidden / not expressible
- source-time queue entry versus destination-time delivery remains intact

### Simulator proofs

- the simulator agrees with the runtime on the sealed liveness/supervision
  model
- replay records reproduce against the same workload binary and simulator
  version; they do not serialize arbitrary isolate values, spawn factories,
  bootstrap closures, or TCP scripts
- a narrow checker catches at least one deliberately injected bug in a
  Kepler-sealed rule
- replay reproduces that checker failure

### Additional integration / e2e proof strengthening

- at least one multi-shard workload pressures the new liveness or supervision
  rule under replayable fault/search conditions
- any delivery-rule tightening is covered by a user-shaped workload, not only
  by helper-state probes

### Allocation / buffering proofs

- no new hidden unbounded queues appear in the multi-shard path
- runtime-level allocation claims are backed by static audit plus focused
  allocation probes where practical
- anything not proven is named as a non-claim

### Proof modes

- unit tests for crate-private runtime/simulator state that should not become
  public API
- black-box integration tests for public runtime/simulator behavior
- replay tests for simulator records
- checker/adversarial tests for the sealed liveness or supervision rule
- allocation probes for hot paths where practical
- blast-radius tests showing 020 invariants still hold

## What Kepler Explicitly Defers

Kepler should leave these for later phases:

- Tokio bridge design and implementation
- adoption guides, walkthroughs, and polished examples
- publish/semver/public-positioning work
- benchmark suite and product-style hardening story
- optional MPSC fallback unless the core semantics cannot move forward without
  it
- real peer quarantine, shard-restarted broadcast semantics, and real
  substrate liveness signals
- cross-shard child placement and remote restart ownership
- broad cross-shard delivery perturbation language

## Done Means

Kepler is done when all of the following are true:

- peer/shard liveness semantics are written down, implemented, and directly
  proved as an explicit absence in runtime and simulator
- address-local remote failures do not poison a shard, and tests prove later
  good traffic to that shard still succeeds
- the multi-shard supervision boundary is no longer ambiguous: root
  supervision routes by parent shard, children stay on the parent shard, and
  cross-shard child ownership is not part of 022
- the cross-shard ownership/buffering/allocation story is honest at the runtime
  level, not just at the mailbox level
- replay/checker pressure covers at least one Kepler-sealed rule rather than
  only the happy path
- any closeout artifact lists the concrete tests that prove each rule
- `CHANGELOG.md` records the completed work without rewriting old names from
  earlier phases
- `make verify` passes
- later phases can talk about adapters, docs, and adoption against a core that
  no longer has these semantic gaps hanging open
