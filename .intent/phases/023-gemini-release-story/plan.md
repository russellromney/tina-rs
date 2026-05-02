# 023 Gemini Release Story Plan

Session:

- A

## What We Are Building

Build the first release-contract phase after Kepler:

> **turn the settled primitive into an honest public contract**

Gemini exists because Sputnik through Kepler have now built and tested the core
model:

- isolate/effect vocabulary
- bounded mailbox behavior
- explicit-step runtime
- runtime-owned time and TCP
- supervision and restart
- deterministic simulator and replay
- multi-shard explicit-step semantics
- sealed shard-liveness and supervision boundaries

The next risk is not another hidden runtime primitive. The next risk is that a
reader cannot tell what is supported, what is experimental, what is proven, and
what is deliberately not promised.

Gemini should make the repo understandable and reviewable as a package without
pretending it is production-ready.

Expected direction:

- write the supported-invariant contract
- make README/ROADMAP match the actual current system
- make proof gates explicit and repeatable
- decide publish posture, likely "not yet" unless every release gate is met
- keep all core semantics unchanged
- do not start Apollo / Tokio bridge work yet

If Gemini discovers that a claimed invariant is not actually proved, stop and
either narrow the claim or add a focused proof. Do not cover gaps with prose.

## What Will Not Change

- This phase does **not** add new runtime semantics.
- This phase does **not** add the Tokio bridge.
- This phase does **not** add a monoio/thread-per-core backend.
- This phase does **not** add new convenience APIs just to make examples look
  nicer.
- This phase does **not** publish crates unless the release gates are actually
  satisfied and the human explicitly chooses publish.
- This phase does **not** treat README prose as proof.
- This phase does **not** rewrite old changelog names. Historical entries stay
  historically true.

## Why This Comes Before Apollo

Apollo will compare Tina's guarantees against Tokio integration constraints.
That comparison is only useful if Tina's own current guarantees are named
first.

Without Gemini, Apollo can accidentally blur three different things:

- guarantees Tina already proves
- guarantees the bridge preserves
- guarantees the bridge weakens or cannot provide

Gemini gives Apollo a clean baseline.

## Core Questions Gemini Should Close

### 1. Supported invariant contract

Write down the current supported model in one place for users and reviewers.

It should cover:

- isolate handler discipline
- effect boundary
- bounded mailbox behavior
- typed address / generation behavior
- supervision scope
- runtime-owned call behavior
- simulator replay honesty
- multi-shard explicit-step behavior
- current liveness non-claims
- current allocation non-claims

The contract must distinguish:

- supported invariant
- proven implementation behavior
- experimental API surface
- deferred production/runtime backend work

### 2. Proof gate

Make the proof gate easy to run and easy to understand.

`make verify` already exists and should remain the main gate. Gemini should
document what it means:

- formatting
- workspace check
- workspace tests
- Loom SPSC tests
- docs build
- clippy `-D warnings`

If there are proof commands that matter but are not in `make verify`, Gemini
must either add them or explicitly name them as non-default/manual gates.

### 3. User-facing path

The README should teach the actual current path, not the ghost of older phases.

It should explain:

- what Tina is
- what code looks like
- which crates exist
- what works today
- what does not work today
- where to look for proof

This is not marketing polish. It is user orientation. If a reader cannot tell
how to start writing one isolate and how to test it, Apollo will inherit a
muddy contract.

### 4. Public positioning and publish decision

Gemini should decide the public posture:

- publish `0.1.0`
- prepare for publish but do not publish
- explicitly remain private/experimental for now

The expected direction is conservative: **prepare the contract, do not publish
unless the release checklist passes and the human chooses publish.**

The Tina-Odin relationship must stay clear:

- independently maintained Rust project
- inspired by Tina-Odin
- no implied official endorsement
- public wording should be respectful and precise

### 5. API and crate boundary audit

Review current public surfaces before any release posture is claimed.

This is an audit, not a rename party.

Good Gemini changes:

- remove stale docs that teach old names
- add missing docs for public types users already need
- add compile tests for public examples
- narrow claims that are too broad

Bad Gemini changes:

- rename things because they feel slightly imperfect
- add helper APIs without a proof/user need
- stabilize bridge-facing APIs before Apollo
- hide experimental status

If a public API change seems necessary, pause for review. Gemini should mostly
document and prove the surface that already exists.

## Pause Gates

Pause before implementation continues if:

- any claimed invariant lacks a direct test/proof or must be weakened
- publish looks likely but public positioning is unresolved
- a docs update requires semantic or API changes
- examples need new helper APIs to look acceptable
- bridge work starts sneaking into this phase
- a release checklist item implies a new runtime backend

## Build Steps

1. Update `ROADMAP.md` so Kepler is completed and Gemini is the active next
   package.
2. Refresh README status/next-step wording after Kepler.
3. Write a supported-invariants document for the current model.
4. Audit existing public docs and examples for stale names or stale claims.
5. Add compile-tested snippets or integration tests for any public example code
   that is not already exercised.
6. Write a release checklist:
   - proof gate
   - docs gate
   - public-positioning gate
   - semver/publish decision
   - known non-claims
7. Decide and record publish posture.
8. Update `CHANGELOG.md` only for actual Gemini changes.
9. Run `make verify`.

## Proof Plan

Gemini should prove that the public story matches the code.

Proof modes:

- docs build for public API docs
- compile tests for README/user snippets where practical
- existing integration/e2e tests for runtime and simulator behavior
- `make verify` as the release gate
- audit checklist tying supported claims to named tests or documents

The proof artifact should map every major supported invariant to one of:

- named tests
- documented non-claim
- deferred future phase

## What Gemini Explicitly Defers

- Tokio bridge implementation
- monoio/thread-per-core runtime backend
- real peer quarantine / shard-restart broadcast semantics
- cross-shard child placement
- broad runtime allocation-free claims
- benchmark suite and production-server claims
- public publish if release gates or positioning are not ready

## Done Means

Gemini is done when:

- README and ROADMAP describe the post-Kepler repo honestly
- supported invariants and non-claims are written down in one clear place
- public examples/snippets are either tested or clearly linked to tested code
- release/publish posture is explicitly recorded
- no new runtime semantics were smuggled into the docs phase
- Apollo has a stable contract document to compare bridge guarantees against
- `make verify` passes
