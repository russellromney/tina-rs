# Plan Review 1

- [fixed] Cross-shard ownership was allowed to devolve into "typed
  unsupported" for the whole phase. That would not advance the core capability.
  The plan now requires local live/sim multi-shard child ownership and reserves
  typed unsupported for remoting/clustering edges.
- [fixed] The plan did not state old behavior that must remain stable. Added
  same-shard spawn, `spawn_observed`, restart-budget, panic-restart, stale
  generation, and runtime-owned lineage non-change rules.
- [fixed] Proof did not name blast radius. Added public-path regression proof
  for existing supervision and observed-spawn behavior.

Remaining risk: cross-shard child ownership is runtime-deep. Implementation
review must check that stop/restart/report truth works in both live and sim,
not only in simulator.
