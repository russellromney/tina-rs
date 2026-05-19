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

# Plan Review 2

- [fixed] Runtime fairness was too small as a separate phase and too close to
  Phase 121. It is now folded into the supervision/failure-domain phase because
  both touch runtime ownership, failed-shard truth, progress, and terminal
  reports.
- [fixed] Fairness proof now requires Tina-visible facts: ready-turn lag,
  timer lateness, remote-drain yield, progress counts, and starvation warnings.
  Throughput charts are explicitly not enough.
- [fixed] Trace determinism and replay blast radius are now part of the same
  runtime phase instead of a separate later paper cut.

Remaining risk: one large runtime phase can sprawl. Keep scope to ownership,
failure, progress, and reports. Do not add broad scheduler policy or priority
queues.
