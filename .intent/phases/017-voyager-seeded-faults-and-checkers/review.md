# 017 Review

Session:

- A (artifacts)

## Plan Review 1

Artifacts prepared:

- `.intent/phases/017-voyager-seeded-faults-and-checkers/plan.md`

Initial framing choices already pinned in the artifacts:

- 017 is the slice where the simulator seed becomes semantically real.
- The first fault model stays narrow and uses both simulator-owned surfaces:
  local send and timer wake delivery.
- Checker support lands as a small honest surface, not a final DSL:
  one user-facing checker callback/trait that can continue or fail-with-reason.
- Replay of faulted runs is part of the package, not deferred.
- Existing 016 behavior must remain green with faults disabled.
- The package is now intentionally bigger than seed plumbing: it must catch
  and replay at least one meaningful failure.
- The second proof workload is pinned to a tiny deliberate-bug harness rather
  than a second realistic app.

What already looks strong:

- The package is a coherent follow-on to 016 rather than a grab-bag.
- The scope is explicit about what does *not* land yet:
  - no TCP simulation
  - no multi-shard
  - no giant checker language
- The proof shape is explicit:
  - same seed/config determinism
  - different-seed divergence
  - checker-triggered reproducible failure
  - blast-radius against 016 and Mariner
- The plan names direct proof, surrogate proof, and blast-radius proof
  distinctly instead of blurring them together.
- The widened package now asks Voyager to prove value, not just infrastructure:
  preserved success, faulted divergence, and at least one replayable failure.

What is still open for review:

- Are local-send plus timer-wake together still the right minimum breadth, or
  is one likely to add too much complexity for 017?
- Is the deliberate-bug harness specific enough to stay crisp without turning
  into a toy no user would recognize?
- Is the minimal checker callback/trait enough, or does failure reporting need
  one more structured layer?

How this could still be broken while the listed tests pass:

- the seed affects one perturbation path but not the other, while the proof
  only exercises the implemented path
- the timer-wake path changes only virtual-time outcome while the artifacts
  keep talking as if every seeded divergence must appear in the event record
- the checker halts the run but the replay artifact omits enough context that
  failure reproduction only works in-memory, not from the saved artifact
- fault-disabled blast-radius stays green while a fault-enabled public-path
  workload still relies mostly on helper-state assertions instead of event
  record meaning
- the package proves divergence under faults but never proves that a meaningful
  invariant failure can be caught and replayed
- the deliberate-bug harness proves checker plumbing only, but not a failure
  shape that feels like a real simulator/debugging win

## Implementation Review 1

Implemented the first seeded-fault/checker Voyager slice directly in
`tina-sim`.

What landed:

- `SimulatorConfig.seed` now drives two narrow seeded perturbation paths:
  - local-send delivery
  - timer-wake delivery
- `FaultConfig` / `FaultMode` define the first explicit fault surface without
  broadening into general chaos machinery.
- `Checker`, `CheckerDecision`, and `CheckerFailure` add the first
  user-facing checker surface.
- `ReplayArtifact` now preserves checker failure information alongside config,
  final virtual time, and event record.
- `run_until_quiescent_checked()` allows one checker to observe the public
  runtime event stream and halt the run with a structured reason.

What the direct proof surface now covers:

- timer-wake seeded perturbation on the existing retry/backoff workload in
  `tina-sim/tests/retry_backoff.rs`
- local-send seeded perturbation on a tiny deliberate-bug harness in
  `tina-sim/tests/faulted_replay.rs`
- same-seed replay of a checker-triggered failure from artifact config alone
- a small structural checker over event-id monotonicity
- fault-disabled preserved success for the deliberate-bug harness

Real implementation bugs the new proof flushed out:

- local-send "delay" originally did not delay an extra delivery round at all,
  because handler-emitted sends already miss the current step snapshot
- `run_until_quiescent()` originally stopped even when future-visible delayed
  local sends were still pending in inboxes

Those are now fixed in `tina-sim/src/lib.rs`, which is a good sign that the
phase is proving real simulator behavior rather than just adding config knobs.

Exact verification after the fixes:

- `cargo +nightly test -p tina-sim`
- `cargo +nightly test -p tina-runtime-current --test retry_backoff`
- `make verify`

## Closeout Review 1

017 is closeable on its actual shipped terms.

What the phase now proves directly:

- the simulator seed is semantically real on both seeded perturbation paths
  owned by this slice:
  - local-send delivery
  - timer-wake delivery
- same seed/config reproduces the same replay-visible outcome
- different seeds can diverge intentionally:
  - event record plus checker failure on the local-send path
  - final virtual time on the timer-wake path
- a checker can halt a run with a structured failure reason
- replay artifacts preserve enough information to reproduce that checker
  failure from artifact config alone
- fault-disabled workloads preserve the 016 success path
- blast-radius proof remains green across the workspace

What 017 deliberately does not claim:

- TCP simulation
- spawn or supervision simulation
- a broad checker DSL
- broad chaos/fault infrastructure
- multi-shard simulation
- a final PRNG policy for all later Voyager work

Important honesty note:

- timer-wake perturbation in this slice changes replay-visible virtual-time
  outcome, but not necessarily the event record shape
- local-send perturbation is where the phase proves event-record and
  checker-outcome divergence directly

Exact verification at closeout:

- `cargo +nightly test -p tina-sim`
- `cargo +nightly test -p tina-runtime-current --test retry_backoff`
- `make verify`
