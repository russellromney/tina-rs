# Phase 148 Review

## Plan Review 1

Findings:

- [P2] The plan could become benchmark theater if it only chases p50. It now
  makes wall-clock numbers evidence, not the main gate. Stage counts,
  allocation counts, leak truth, final-current zero, and typed pressure are the
  harder checks.
- [P2] "Reduce HTTP turn count" could hide suspension truth. The plan now only
  allows protocol-local folding when no user policy boundary is crossed, and it
  forbids hidden callbacks, hidden retries, hidden pipelining, and body-pressure
  lies.
- [P2] Linux evidence could be hand-waved. The plan now requires Linux/x86 rows
  or a concrete environment blocker, and keeps perf-check platform-scoped.
- [P2] Whole-service proof could duplicate existing systems. The plan names
  `mini_saas_api` as the primary service and sharpens it instead of creating
  another half-service.
- [P3] Strict latency gates would be flaky and stupid. The plan says loose
  "obviously worse" ceilings only; p50 is recorded for evidence.
- [P3] Allocation cleanup could accidentally force a public `HeaderMap`
  migration. The plan keeps that out unless explicitly justified before code.

Decision:

- Plan is implementation-ready. It is intentionally not a production
  performance claim.

## Plan Review 2

Findings:

- [P2] Hotpath stage counts were visible but not durable. Current
  `perf_record.sh` records `perf-compare` and `perf-process` rows, not
  `hotpath` rows, so a stage-count regression could disappear after logs roll
  away. The plan now requires recording hotpath rows into Phase 148 history and
  checking stage/process-allocation fields with loose thresholds.
- [P2] The old done condition allowed "no improvement, but blocker named" to
  merge. That is a planning/audit outcome, not an implementation phase. The
  plan now requires at least one measured HTTP turn or allocation improvement;
  if none is found, stop and hand back the blocker instead of merging.
- [P2] `mini_saas_api` pool coverage could still be inferred from generic 503s.
  That is weak user-shaped proof. The plan now requires direct notify/outbound
  pool activity fields such as attempted notify ops and acquired/released/
  retired leases.
- [P2] Linux evidence was too easy to hand-wave. The plan now requires a manual
  non-required Ubuntu workflow that uploads/prints JSONL rows, with Linux rows
  recorded in history or attached as workflow artifact. If the session cannot
  run the workflow, the missing external proof must be named.
- [P3] The plan named `make proof-long-soak` before requiring the target. It now
  explicitly requires adding the Makefile target and checking it with
  `make -n proof-long-soak`.
- [P3] Dry-run parser proof needed a stable sample input. The plan now requires
  a phase-local `perf_sample.txt` carrying compare/process/hotpath sample rows.

Decision:

- Plan is stronger and still gruglike enough: build evidence, reduce measured
  cost, prove soak/load truth, do not claim production performance.
