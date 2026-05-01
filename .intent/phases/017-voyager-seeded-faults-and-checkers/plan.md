# 017 Voyager Faulted Replay And Checkers Plan

Session:

- A

## What We Are Building

Build the second Voyager slice: make `tina-sim`'s seed matter, add narrow
fault models, add checker support, and prove that the simulator can catch and
replay at least one meaningful failure instead of only reproducing happy-path
event logs.

Concretely, this package adds:

- seeded execution behavior in `tina-sim`
- first fault configuration over both:
  - local send
  - timer wake delivery
- structural and user-facing checker support
- replay artifacts rich enough to reproduce faulted runs
- at least two user-shaped workloads that prove the checker/fault path
- at least one intentionally caught and replayed failure

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
- Start with the smallest useful fault models over surfaces the simulator
  already owns:
  - local send
  - timer wake delivery
- Prefer explicit fault/config types over hidden global randomness.
- Add the smallest honest checker surface that can stop a run with a
  structured failure reason:
  - one user-supplied checker callback/trait
  - returns either continue or fail-with-reason
- Replay artifacts should capture exactly the information needed to reproduce
  a faulted run.
- Use one existing workload and one intentionally fault-sensitive workload so
  the simulator proves both preservation and bug-catching value.
- The second workload is pinned:
  - a tiny deliberate-bug harness
  - not a second realistic app
  - its job is to make checker failure and replay maximally crisp

## Build Steps

1. Extend `tina-sim` config so the seed drives a deterministic PRNG or
   equivalent perturbation source.
2. Add the first narrow fault configuration surface over both local-send and
   timer-wake behavior.
3. Implement seeded perturbation over both local-send and timer-wake paths.
4. Add structural checker hooks for framework invariants relevant to this
   slice.
5. Add one minimal user-facing checker API that can stop a run with a failing
   reason.
6. Extend replay artifacts with the fault/checker context needed for exact
   reproduction.
7. Add direct proofs:
   - same seed/config => same faulted event record
   - different seeds diverge under a fault-enabled workload
   - both perturbation paths are actually exercised by proof
   - replay reproduces the same checker failure
8. Add user-shaped workloads under simulation that prove the value of the new
   surface:
   - extend the existing retry/backoff workload under seeded faults
   - add one tiny deliberate-bug harness that a checker can fail reproducibly
9. Close out docs/evidence:
   - `.intent/SYSTEM.md`
   - `ROADMAP.md`
   - `CHANGELOG.md`
   - phase `commits.txt`
   - review receipt

## How We Will Prove It

Direct proof for the changed behavior:

- at least one integration or e2e flow must hit the public simulator path,
  exercise seeded perturbation, and prove the promised result through the
  replay artifact rather than through helper-only state
- focused seeded-fault tests for:
  - same seed/config => same event record
  - different seed => intentionally different replay-visible outcome
    - event record or checker failure for local-send perturbation
    - final virtual time for timer-wake perturbation
  - disabled faults preserve 016 behavior
- checker tests for:
  - structural checker observes expected framework invariants
  - user-facing checker halts run with a reproducible reason
  - replay reproduces the same checker failure exactly from artifact data alone
- e2e-style workload proof for:
  - one preserved success path under simulator execution
  - one fault-triggered divergence or failure
  - one checker-triggered reproducible failure
  - replay artifact reproducing those outcomes

Proof modes expected in this package:

- unit proof:
  seeded perturbation bookkeeping
- integration proof:
  faulted send/timer behavior on the simulator path
- e2e proof:
  user-shaped workload through `tina-sim`
- adversarial proof:
  checker-triggered failure plus exact replay of that failure
- replay proof:
  saved artifact reproduces checker failure exactly
- blast-radius proof:
  existing 016 simulator tests and Mariner runtime suites still pass

Surrogate proof that helps but does not close the claim:

- comparing only final state without comparing event record or checker result
- ad hoc logging instead of a replayable structured artifact
- helper-level seeded unit tests with no public-path faulted workload
- a synthetic failure that does not correspond to either a user-shaped workload
  or a real invariant/checker
- a proof that exercises only one perturbation path while claiming seeded fault
  coverage for both

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
- the only demonstrable failure requires a much broader workload or simulator
  surface than this phase should own

## Traps

- Do not fake seeded behavior by merely recording the seed.
- Do not broaden fault injection into full chaos infrastructure.
- Do not make replay depend on logs instead of structured artifact data.
- Do not let checkers silently invent a second semantic model.
- Do not break 016's timer semantics while adding fault paths.
- Do not declare DST value proven unless at least one meaningful failure is
  actually caught and replayed.

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
- exact first fault model(s)
- exact checker API and failure shape
- exact replay artifact additions
- which workloads were used to prove preserved success vs caught failure
- how both perturbation paths were directly proved
- what is directly proved vs surrogate-supported
- exact verification results
