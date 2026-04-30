# 017 Voyager Seeded Faults And Checkers

Session:

- A

## In Plain English

016 gave `tina-rs` its first honest simulator foothold:

- single shard
- virtual monotonic time
- shipped timer semantics
- replayable event records
- a retry/backoff workload under simulation

That is a real start, but it is not yet the full DST payoff.

The biggest remaining gap is that Voyager still cannot *search* interesting
behavior. The simulator is deterministic, but the seed does not influence
anything yet, there are no injected faults, and there is no checker surface
that can halt a run with a reproducible failing artifact.

017 should close that gap in one coherent package:

- make the seed real
- add a narrow first fault model
- add structural and user-facing checkers
- prove that failures become reproducible artifacts instead of vague test
  flakes

This is the first slice where Voyager starts looking like Tina's promised DST
story rather than "virtual time plus replay."

## What Changes

- `tina-sim` grows a real seeded execution surface.
- The simulator gets a small fault configuration model driven by that seed.
- The first fault model stays narrow and honest:
  - local-send drop or delay
  - timer-wake drop or delay
  - no TCP faults yet
  - no crash-supervision simulation yet
- `tina-sim` grows checker support:
  - structural checker hooks for framework invariants
  - one small user-facing checker API that can stop a run with a failing
    reason
- Replay artifacts grow enough information to reproduce faulted runs, not just
  happy-path ones.
- The proof surface grows from "replay the same good run" to:
  - same seed/config => same faulted event record
  - different seeds can diverge intentionally
  - a checker failure is reproducible from the saved artifact

## What Does Not Change

- `tina` stays substrate-neutral.
- 017 stays single-shard.
- 017 does not simulate TCP yet.
- 017 does not add a final checker DSL.
- 017 does not add multi-shard routing or scheduling semantics.
- 017 does not broaden into release-story or bridge work.
- 017 does not weaken the timer semantics already shipped in 016.

## Acceptance

- `SimulatorConfig.seed` influences simulator behavior in a deterministic,
  reviewable way.
- The simulator supports at least one honest seeded fault model over the
  already-shipped timer/send surface.
- Structural checker support exists and is exercised.
- A user-facing checker can halt a run with a reproducible failure reason.
- Replay artifacts capture enough information to reproduce a faulted run
  exactly.
- Direct proof covers:
  - same seed/config => same event record
  - at least one different-seed divergence
  - checker-triggered failure replay
- A user-shaped workload demonstrates the fault/checker path:
  - either the existing retry/backoff workload extended under faults
  - or one additional small workload if retry/backoff alone is too narrow
- Blast-radius proof shows 016 timer/replay behavior still passes unchanged
  when faults are disabled.

## Notes

- This package should prefer narrow, composable fault semantics over broad
  "chaos mode."
- The seed should become semantically meaningful here, not just remain an
  artifact placeholder.
- A small honest checker surface is better than a half-designed final DSL.
