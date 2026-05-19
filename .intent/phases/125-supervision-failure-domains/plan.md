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

- cross-shard child ownership, or a typed explicit rejection if not supported
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
- Cross-shard ownership must be either implemented with typed remote truth or
  rejected at registration/spawn time. No half-support.
- Non-panic child failure is a normal typed outcome. Panic failure remains
  visible separately.

## User Proof Specimens

- supervised worker pool: parent spawns children, one fails, replacement starts,
  parent learns new address
- supervised session service: parent stop closes children and reports all
  terminal child outcomes
- cross-shard child attempt: either works with typed propagation or rejects with
  the documented unsupported outcome

## Required Proof

- live single-shard child restart yields new generation and stale address
  rejection
- live multi-shard parent-stop child cleanup or typed unsupported rejection
- simulator replay of child start/fail/restart/stop sequence
- failure-before-start reports no hidden child
- failure-after-start reports child id/generation and owner id
- owner stop while child has in-flight call settles caller visibly
- restart budget exhaustion stops restarting and reports final state
- compile-fail or macro test for illegal hidden/shared child ownership if the
  API adds an ownership type

## Hostile Review Notes

- Do not build clustering.
- Do not hide stale address refresh in docs-only advice.
- Do not claim cross-shard ownership unless a live test proves it.
- Do not collapse panic, typed failure, budget exhaustion, and owner stop into
  one vague `Closed`.
