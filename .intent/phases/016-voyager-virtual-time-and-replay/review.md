# 016 Review

Session:

- A (artifacts)

## Plan Review 1

Artifacts prepared:

- `.intent/phases/016-voyager-virtual-time-and-replay/spec-diff.md`
- `.intent/phases/016-voyager-virtual-time-and-replay/plan.md`

Initial framing choices already pinned in the artifacts:

- 016 is the first Voyager foothold, not "full simulator everything."
- The first simulated capability is the shipped timer contract, not TCP.
- The first user-shaped workload is the existing retry/backoff path from 015.
- The simulator must use the live runtime event model as its meaning surface.
- Replay is part of the first slice, not deferred.
- The plan answers the Session A questions explicitly and names proof modes:
  - unit
  - integration
  - e2e
  - replay
  - blast-radius

What already looks strong:

- The slice says plainly what it is building: a first `tina-sim` crate for
  virtual time and replay on top of the shipped timer contract.
- The non-change space is clear: no TCP simulation yet, no failure injection
  yet, no multi-shard semantics yet, no simulator-only `tina` boundary.
- The proof shape is explicit instead of implied:
  - direct timer semantics proof
  - user-shaped simulator retry/backoff proof
  - replay proof
  - blast-radius proof against Mariner

What is still open for review:

- Is timer-only simulation the right first Voyager slice, or is it too narrow
  to justify a new crate?
- Is the retry/backoff workload enough user-shape for the first replay proof,
  or does the first simulator slice need one more workload?
- What is the minimum replay artifact shape we should insist on in 016?

## Implementation Review 1

Implemented the first Voyager foothold as a new `tina-sim` crate.

What landed:

- single-shard virtual-time simulator
- timer-only runtime-owned call surface (`Sleep { after }` / `TimerFired`)
- deterministic event recording using `tina-runtime-current`'s event
  vocabulary
- replay artifact carrying config, final virtual time, and event record
- direct timer-semantics proofs
- simulator-backed retry/backoff proof and replay reproduction test

Important scoping choices kept honest:

- no simulator-only `tina` boundary changes
- no TCP simulation yet
- no failure injection yet
- no checker DSL yet
- no multi-shard simulation yet

Proof shape that landed:

- public-path timer semantics tests in `tina-sim/tests/timer_semantics.rs`
- public-path retry/backoff and replay tests in
  `tina-sim/tests/retry_backoff.rs`
- blast-radius proof via existing `tina-runtime-current` retry test and full
  workspace verification

Exact verification:

- `cargo +nightly test -p tina-sim`
- `cargo +nightly test -p tina-runtime-current --test retry_backoff`
- `make verify`

One nuance kept explicit:

- `SimulatorConfig` already carries a replay seed, and replay artifacts
  preserve it, but this first timer-only Voyager slice does not yet consume
  the seed to perturb scheduling or faults. That is intentional scope
  discipline, not fake randomness: the event record is still replayable, and
  later Voyager slices can broaden the meaning of the seed without changing
  the artifact shape.

## Closeout Review 1

016 is closeable on its actual shipped terms.

What the phase honestly proves:

- `tina-sim` exists as a new simulator crate.
- It executes a single-shard virtual-time subset of the shipped runtime
  semantics without inventing a second visible model.
- It directly proves timer semantics:
  - no early wake
  - one-shot wake
  - different-deadline ordering
  - equal-deadline request-order tie-break
  - stopped-requester completion rejection
- It proves a user-shaped retry/backoff workload through the public simulator
  path.
- It proves replayability of the current slice by reproducing the same event
  record and final virtual time from the same workload/config.
- Blast-radius proof remains green.

What 016 deliberately does not claim:

- seeded perturbation
- failure injection
- checker infrastructure
- TCP simulation
- multi-shard simulation

Exact verification at closeout:

- `cargo +nightly test -p tina-sim`
- `cargo +nightly test -p tina-runtime-current --test retry_backoff`
- `make verify`
