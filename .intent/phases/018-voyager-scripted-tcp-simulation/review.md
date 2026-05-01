# 018 Review

Session:

- A (artifacts)

## Plan Review 1

Artifacts prepared:

- `.intent/phases/018-voyager-scripted-tcp-simulation/spec-diff.md`
- `.intent/phases/018-voyager-scripted-tcp-simulation/plan.md`

Initial framing choices pinned in the artifacts:

- 018 is a Voyager simulator slice, not Gemini release-story work.
- The slice simulates the already-shipped TCP call family through scripted
  network inputs, not real sockets.
- The simulator continues to use `tina-runtime-current`'s call and event
  vocabulary.
- The proof workload is TCP echo-shaped because that is the existing Mariner
  server proof surface.
- Replay must include both event record and peer-visible output, not logs.
- 017's seeded fault/checker work is treated as a prerequisite or composition
  point, not something 018 should silently duplicate.

What already looks strong:

- The package follows naturally after the first timer/replay simulator slice
  and the planned seeded fault/checker slice.
- The non-change space is explicit:
  - no `tina` boundary changes
  - no real sockets
  - no virtual TCP stack
  - no multi-shard semantics
  - no simulator-only event model
- The proof shape distinguishes direct proof from surrogate proof:
  - focused TCP call semantics
  - user-shaped TCP echo workload
  - replay of event record and peer output
  - blast-radius against prior simulator/runtime suites

What is still open for review:

- Should 018 require deterministic partial-write simulation, or is full-write
  capture enough for the first scripted TCP slice?
- What is the smallest replay artifact addition that honestly captures
  scripted network input and observed peer output?
- If 017 has not landed before implementation starts, should 018 pause, or may
  it land as a TCP-only simulator slice with faults/checkers still deferred?
