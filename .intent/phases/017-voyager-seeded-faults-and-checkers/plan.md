# 017 Voyager Seeded Faults And Checkers Plan

Session:

- A

## What We Are Building

Build the second Voyager slice: make `tina-sim`'s seed matter, add a narrow
fault model, and add checker support so simulated failures become reproducible
artifacts instead of just alternate event logs.

Concretely, this package adds:

- seeded execution behavior in `tina-sim`
- a first fault configuration surface
- structural and user-facing checker support
- replay artifacts rich enough to reproduce faulted runs
- at least one user-shaped workload that proves the checker/fault path

## What Will Not Change

- `tina` remains substrate-neutral and does not gain simulator-only public
  API.
- This slice stays single-shard.
- This slice does not simulate TCP yet.
- This slice does not add final multi-domain PRNG design for all future
  Voyager work; it only needs a clean first seeded execution shape.
- This slice does not add a full checker DSL.
- This slice does not add multi-shard semantics, release-story work, or
  Tokio-bridge work.
- Existing 016 timer/replay behavior must remain green when faults are
  disabled.

## How We Will Build It

- Make `SimulatorConfig.seed` semantically real.
- Keep seeded behavior deterministic and easy to audit.
- Start with the smallest useful fault model over surfaces the simulator
  already owns:
  - local send
  - timer wake delivery
- Prefer explicit fault/config types over hidden global randomness.
- Add checker support that can stop a run with a structured failure reason.
- Replay artifacts should capture exactly the information needed to reproduce
  a faulted run.

## Build Steps

1. Extend `tina-sim` config so the seed drives a deterministic PRNG or
   equivalent perturbation source.
2. Add the first narrow fault configuration surface.
3. Implement seeded perturbation over local-send and/or timer-wake behavior.
4. Add structural checker hooks for framework invariants relevant to this
   slice.
5. Add one user-facing checker API that can stop a run with a failing reason.
6. Extend replay artifacts with the fault/checker context needed for exact
   reproduction.
7. Add direct proofs:
   - same seed/config => same faulted event record
   - different seeds diverge under a fault-enabled workload
   - replay reproduces the same checker failure
8. Add a user-shaped workload under simulation that proves the value of the
   new surface.
9. Close out docs/evidence:
   - `.intent/SYSTEM.md`
   - `ROADMAP.md`
   - `CHANGELOG.md`
   - phase `commits.txt`
   - review receipt

## How We Will Prove It

Direct proof for the changed behavior:

- focused seeded-fault tests for:
  - same seed/config => same event record
  - different seed => intentionally different event record
  - disabled faults preserve 016 behavior
- checker tests for:
  - structural checker observes expected framework invariants
  - user-facing checker halts run with a reproducible reason
  - replay reproduces the same checker failure exactly
- e2e-style workload proof for:
  - fault-free success
  - fault-triggered divergence or failure
  - replay artifact reproducing that outcome

Proof modes expected in this package:

- unit proof:
  seeded perturbation bookkeeping
- integration proof:
  faulted send/timer behavior on the simulator path
- e2e proof:
  user-shaped workload through `tina-sim`
- replay proof:
  saved artifact reproduces checker failure exactly
- blast-radius proof:
  existing 016 simulator tests and Mariner runtime suites still pass

Surrogate proof that helps but does not close the claim:

- comparing only final state without comparing event record or checker result
- ad hoc logging instead of a replayable structured artifact

## How We Will Prove We Did Not Break Earlier Intent

Blast-radius proof for already-shipped behavior:

- existing `tina-sim` timer/replay tests still pass when faults are disabled
- existing `tina-runtime-current` tests still pass
- `make verify` still passes
- no `tina` boundary changes are required

## Pause Gates

Stop and ask for human input only if one of these happens:

- making the seed real appears to require simulator-only `tina` semantics
- the first useful fault model needs TCP simulation after all
- the checker shape wants a much larger DSL/API than this package should own
- reproducible fault replay requires a substantially broader artifact
  compatibility promise than we want yet

## Traps

- Do not fake seeded behavior by merely recording the seed.
- Do not broaden fault injection into full chaos infrastructure.
- Do not make replay depend on logs instead of structured artifact data.
- Do not let checkers silently invent a second semantic model.
- Do not break 016's timer semantics while adding fault paths.

## Files Likely To Change

- `tina-sim/src/lib.rs`
- `tina-sim/tests/`
- `.intent/SYSTEM.md`
- `ROADMAP.md`
- `CHANGELOG.md`

## Areas That Should Not Be Touched

- mailbox crates
- live runtime semantics
- Betelgeuse backend
- multi-shard runtime code
- Tokio bridge code

## Report Back

- exact seeded execution surface
- exact first fault model
- exact checker API and failure shape
- exact replay artifact additions
- what is directly proved vs surrogate-supported
- exact verification results
