# 017 Review

Session:

- A (artifacts)

## Plan Review 1

Artifacts prepared:

- `.intent/phases/017-voyager-seeded-faults-and-checkers/spec-diff.md`
- `.intent/phases/017-voyager-seeded-faults-and-checkers/plan.md`

Initial framing choices already pinned in the artifacts:

- 017 is the slice where the simulator seed becomes semantically real.
- The first fault model stays narrow and uses surfaces the simulator already
  owns.
- Checker support lands as a small honest surface, not a final DSL.
- Replay of faulted runs is part of the package, not deferred.
- Existing 016 behavior must remain green with faults disabled.

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

What is still open for review:

- Is local-send plus timer-wake perturbation the right first fault surface, or
  is one of them enough for this slice?
- Is retry/backoff alone a strong enough user-shaped workload for faulted
  replay, or should one more small workload land here?
- What is the smallest honest checker surface that still feels user-facing?
