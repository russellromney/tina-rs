# 022 Kepler Core Primitive Completion Plan

Session:

- A

## What We Are Building

Build the next core phase after Galileo:

> **finish the concurrency primitive before building bridges around it**

Kepler exists because Galileo made `tina-rs` honestly multi-shard, but there
are still a few core semantic and hardening gaps between "real and reviewable"
and "settled enough to anchor adapters, publish stronger claims, or freeze
more surface area."

This phase is intentionally **not** a docs/examples/polish phase.
It is also intentionally **not** the Tokio bridge phase.

Kepler should answer the remaining core questions that still sit too close to
the primitive itself:

- what peer/shard liveness means across shards
- how far multi-shard supervision semantics really go in the first serious core
- whether cross-shard child placement remains forbidden or grows an explicit
  reviewed model
- what the real allocation/buffering story is in the runtime core, not just in
  the mailbox crate
- whether simulator/checker/replay support is strong enough to pressure these
  semantics as core invariants rather than as happy-path demos

The story Kepler must tell is:

> Tina's core concurrency primitive is now semantically settled enough that
> later bridge, docs, and adoption phases can treat it as a thing with a real
> center of gravity instead of a still-moving target.

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
- This phase does **not** reopen 021's general syntax direction unless a core
  semantic gap truly forces it.
- This phase does **not** treat examples as proof. Any example work that lands
  here should exist only because a proof workload needs it.

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

### 1. Peer / shard liveness semantics

Galileo explicitly stopped before full upstream-style peer quarantine /
shard-restarted propagation semantics.

Kepler should decide and prove one honest first-class story for:

- what a shard learns when a peer shard becomes unavailable
- whether remote-target failure stays address-local only or can escalate to
  shard-level liveness facts
- whether a shard-restarted / peer-restarted event vocabulary becomes part of
  the observable runtime model
- how runtime and simulator agree on that story

This is a core concurrency question, not adapter garnish.

### 2. Multi-shard supervision boundary

Galileo kept parent/child execution shard-local and treated cross-shard spawn
or restart semantics as out of scope.

Kepler should make that boundary explicit in one of two ways:

- either keep child/supervision semantics shard-local and prove that this is a
  deliberate long-lived boundary
- or extend the core model with reviewed cross-shard child placement and the
  minimum supervision semantics needed to keep that honest

The goal is not "more features." The goal is that the primitive stops feeling
uncertain at one of its most structural edges.

### 3. Cross-shard ownership and buffering honesty

Galileo proved bounded shard-pair transport and source-time vs destination-time
staging, but the broader ownership/copy boundary still wants a cleaner,
explicit statement.

Kepler should pin:

- whether cross-shard transport is clone-based, move-based, or mixed by design
- what parts of that are merely implementation detail versus semantic contract
- whether any hidden buffering or unexpected copying remains in runtime-core
  paths
- what exact runtime-level allocation claim we are comfortable making, if any

This is where we stop repeating mailbox-only allocation truths as if they
already described the runtime as a whole.

### 4. Multi-shard replay/checker pressure on the real semantics

Galileo already gives us deterministic multi-shard replay and a user-shaped
workload.

Kepler should make the simulator pressure the harder core edges, not only the
happy path:

- peer-liveness transitions
- cross-shard supervision boundary behavior
- transport admission versus destination-harvest staging
- any new shard-level event vocabulary added here

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

Good boundary changes in Kepler:

- make an existing semantic truth explicit
- remove an ambiguity around shard/peer/core ownership
- pin a real concurrency invariant

Bad boundary changes in Kepler:

- convenience surface drift
- adapter-friendly sugar without a core semantic reason
- premature publish/bridge stabilization

## Build Steps

1. Write down the peer/shard liveness model Kepler intends to ship before
   broad implementation proceeds.
2. Decide the multi-shard supervision boundary:
   - shard-local only and explicitly sealed, or
   - minimal reviewed cross-shard extension
3. Implement the chosen peer/quarantine/liveness semantics in both
   `tina-runtime` and `tina-sim`.
4. Implement or seal the chosen multi-shard supervision boundary in both live
   runtime and simulator terms.
5. Audit runtime-core allocation/buffering behavior on the multi-shard path and
   narrow or strengthen claims accordingly.
6. Extend multi-shard replay/checker pressure so the new semantics are tested
   under reproducible failure/search conditions rather than only exact
   hand-authored traces.
7. Only after the above, decide whether a small `tina` boundary update is
   required to make the primitive honest and explicit.

## Proof Plan

Kepler should prove the primitive, not just describe it.

### Runtime proofs

- peer/shard liveness behavior is directly observable and deterministic
- any shard-level quarantine or peer-restarted propagation is causally visible
- if cross-shard child placement remains forbidden, misuse fails in one pinned,
  deliberate way
- if cross-shard child placement is added, placement/restart semantics are
  directly proved
- source-time versus destination-time staging remains intact under the new
  liveness model

### Simulator proofs

- the simulator agrees with the runtime on the new liveness/supervision model
- replay artifacts are sufficient to reproduce failures around those semantics
- seeded or scripted perturbation can expose at least one deliberately-injected
  multi-shard semantic bug that hand-authored happy-path tests would miss
- at least one non-default-seed multi-shard replay path is directly proved

### Additional integration / e2e proof strengthening

- at least one multi-shard workload pressures the new liveness or supervision
  rule under replayable fault/search conditions
- any new shard-level event or delivery rule is covered by a user-shaped
  workload, not only by helper-state probes

### Allocation / buffering proofs

- no new hidden unbounded queues appear in the multi-shard path
- runtime-level allocation claims are either backed by focused evidence or
  explicitly narrowed

## What Kepler Explicitly Defers

Kepler should leave these for later phases:

- Tokio bridge design and implementation
- adoption guides, walkthroughs, and polished examples
- publish/semver/public-positioning work
- benchmark suite and product-style hardening story
- optional MPSC fallback unless the core semantics cannot move forward without
  it

## Done Means

Kepler is done when all of the following are true:

- peer/shard liveness semantics are written down, implemented, and directly
  proved in runtime and simulator
- the multi-shard supervision boundary is no longer ambiguous
- the cross-shard ownership/buffering/allocation story is honest at the runtime
  level, not just at the mailbox level
- replay/checker pressure covers the hard new semantics rather than only the
  happy path
- later phases can talk about adapters, docs, and adoption against a core that
  no longer has these semantic gaps hanging open
