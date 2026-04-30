# 016 Voyager Virtual Time And Replay Plan

Session:

- A

## What We Are Building

Build the first `tina-sim` slice: a single-shard simulator with virtual time
and replay for the already-shipped runtime-owned timer contract, proved
through the 015 retry/backoff workload.

Concretely, this package adds:

- a new `tina-sim` crate
- virtual monotonic time
- deterministic execution/config
- replay artifact capture
- simulator execution for `Sleep { after }` / `TimerFired`
- a simulator-backed retry/backoff proof workload

## What Will Not Change

- `tina` remains substrate-neutral and does not gain simulator-only public API.
- The simulator must use the existing runtime event model as its meaning
  surface, not invent a second user-visible semantics.
- This slice stays single-shard.
- This slice does not simulate TCP yet.
- This slice does not add failure injection yet.
- This slice does not broaden into checker DSL design, PRNG-tree fanout
  policy, multi-shard semantics, or release-story work.
- Existing live runtime behavior and proofs must remain green.

## How We Will Build It

- Start from the shipped timer contract because it is already deterministic and
  user-shaped.
- Keep the simulator narrow:
  - one shard
  - timer-only call surface
  - ordinary message/effect stepping
- Prefer explicit simulator config types over ambient globals.
- Capture replay data in a simple artifact shape first; do not wait for a
  perfect final replay format.

## Build Steps

1. Add a new `tina-sim` crate to the workspace.
2. Define a minimal simulator config surface for this slice.
3. Implement virtual monotonic time and due-timer scheduling matching the
   shipped Mariner semantics:
   - due at or before sampled virtual instant
   - equal-deadline request-order tie-break
4. Implement enough execution to run the timer-backed retry/backoff workload
   against the existing effect/message model.
5. Emit a replay artifact containing at least config, final virtual time, and
   resulting event record.
6. Add direct simulator proofs:
   - repeated same-workload/config runs produce the same event record
   - replay reproduces that record exactly
   - timer semantics match the shipped Mariner meaning
7. Add a user-shaped simulator proof for retry/backoff.
8. Close out docs/evidence:
   - `.intent/SYSTEM.md`
   - `ROADMAP.md`
   - `CHANGELOG.md`
   - phase `commits.txt`
   - review receipt

## How We Will Prove It

Direct proof for the changed behavior:

- focused simulator timer tests for:
  - no early wake
  - one-shot wake
  - different-deadline ordering
  - equal-deadline request-order tie-break
- replay tests for:
  - same workload/config => same event record
  - saved replay artifact => same reproduced event record
- integration/e2e-style simulator test for retry/backoff:
  - first attempt fails
  - virtual-time delay occurs
  - retry happens later
  - success occurs
  - event record proves the delay path came through simulated `Sleep`

Proof modes expected in this package:

- unit proof:
  virtual-time bookkeeping and due-queue ordering
- integration proof:
  timer request -> simulated wake -> translated message
- e2e proof:
  retry/backoff workload through `tina-sim`
- replay proof:
  saved config/event-record shape reproduces the same event record
- blast-radius proof:
  existing Mariner runtime suites still pass

Surrogate proof that helps but does not close the claim:

- tests that only compare final state without comparing event record
- tests that only show deterministic wall-clock runtime behavior rather than
  simulator replay

## How We Will Prove We Did Not Break Earlier Intent

Blast-radius proof for already-shipped behavior:

- existing `tina-runtime-current` focused tests still pass
- existing workspace `make verify` still passes
- simulator work does not force `tina` boundary changes

## Pause Gates

Stop and ask for human input only if one of these happens:

- the simulator seems to require a different public event model than the live
  runtime already emits
- timer-only simulation proves too weak to justify a standalone `tina-sim`
  crate
- the cleanest replay artifact shape implies a broader public compatibility
  promise than we want to make yet
- implementation starts dragging in TCP simulation, failure injection, or
  multi-shard semantics prematurely

## Traps

- Do not invent simulator-only visible semantics.
- Do not rely on live runtime timing as proof of replayability.
- Do not broaden 016 into full DST/checker/fault infrastructure.
- Do not let the simulator drift from the shipped timer semantics.
- Do not update `SYSTEM.md` until replay and retry proofs are executable.

## Files Likely To Change

- workspace `Cargo.toml`
- new `tina-sim/` crate
- `.intent/SYSTEM.md`
- `ROADMAP.md`
- `CHANGELOG.md`

## Areas That Should Not Be Touched

- mailbox crates
- live runtime semantics except for tiny shared abstractions if absolutely
  needed
- multi-shard runtime code
- Tokio bridge code

## Report Back

- exact simulator config surface
- exact replay artifact shape
- whether any live runtime hook had to change
- what is directly proved vs surrogate-supported
- exact verification results
