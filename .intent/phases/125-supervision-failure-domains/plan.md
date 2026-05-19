# Phase 125: Supervision And Failure Domains

## Status

- Future implementation plan for the first post-122 core wave.
- Runs after Phase 122. Can run beside durable-state and fairness work if
  ownership stays in child/failure-domain runtime code and supervision systems.

## Purpose

Make child ownership and failure boring.

User story:

```text
my service can spawn workers or sessions, observe start/failure/result, stop
them on owner stop, and refresh replacement addresses without scraping traces
```

## Includes

- cross-shard child ownership for the existing local live/sim multi-shard
  runtime
- parent-stop child cleanup across shards
- shard restart propagation for owned children in live and sim; if one path
  cannot support it, return the same typed unsupported truth everywhere
- replacement address refresh after restart
- failed-peer / failed-shard ingress truth
- non-panic failure policy for user-reported child failure
- supervisor terminal report naming children, restarts, failures, stale
  generations, and abandoned work
- docs/specimen updates for supervised worker/session services

## Does Not Include

- no network remoting or clustering
- no OS crash isolation
- no `panic = abort` claim
- no hidden global child registry
- no restart that reuses a stale address generation
- no automatic retry policy hidden inside supervision
- no "cross-shard unsupported" as the whole deliverable. Typed unsupported is
  only for remoting/cluster edges outside the local multi-shard runtime.

## Must Not Change

- Existing same-shard spawn, `spawn_observed`, child ref, restart budget, and
  panic-restart behavior keep their current public outcomes.
- Stale address generation rejection stays loud.
- Parent/child lineage remains runtime-owned; no app-side registry becomes the
  blessed path.

## Implementation Shape

Use user-facing names:

```text
ChildStarted
ChildStopped
ChildFailed
ChildRestarted
ChildAddressChanged
SupervisorReport
FailureDomainReport
RestartWindow
```

Rules:

- Parent ownership is explicit. A child has one owner unless the API says
  otherwise.
- Restart creates a new generation. Stale addresses reject visibly.
- Replacement address refresh is a typed message/report, not trace spelunking.
- Parent stop first stops admission, then stops or drains owned children, then
  emits a report.
- Cross-shard ownership in the existing local multi-shard runtime is part of
  this phase: start, stop, restart report, and stale replacement address truth.
  Network remoting/clustering remains typed unsupported.
- Non-panic child failure is a normal typed outcome. Panic failure remains
  visible separately.

## User Proof Specimens

- supervised worker pool: parent spawns children, one fails, replacement starts,
  parent learns new address
- supervised session service: parent stop closes children and reports all
  terminal child outcomes
- cross-shard child service: parent owns a child on another shard, receives
  start/fail/restart/address-change truth, then stops it cleanly

## Required Proof

- live single-shard child restart yields new generation and stale address
  rejection
- live multi-shard parent-stop child cleanup
- simulator replay of child start/fail/restart/stop sequence
- failure-before-start reports no hidden child
- failure-after-start reports child id/generation and owner id
- owner stop while child has in-flight call settles caller visibly
- restart budget exhaustion stops restarting and reports final state
- compile-fail or macro test for illegal hidden/shared child ownership if the
  API adds an ownership type
- blast-radius proof: existing same-shard supervision tests, existing
  `spawn_observed` tests, and existing restart-budget tests still pass through
  the public path

## Hostile Review Notes

- Do not build clustering.
- Do not hide stale address refresh in docs-only advice.
- Do not claim cross-shard ownership unless a live test proves it.
- Do not collapse panic, typed failure, budget exhaustion, and owner stop into
  one vague `Closed`.
