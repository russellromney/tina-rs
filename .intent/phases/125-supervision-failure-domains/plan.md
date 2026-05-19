# Phase 125: Runtime Supervision And Fairness

## Status

- Future implementation plan for the first post-122 core wave.
- Combines the old supervision/failure-domain and runtime-fairness plans.
- Runs after Phase 122. Can run beside durable-state work if ownership stays in
  runtime supervision, failure domains, fairness reports, and systems.

## Purpose

Make owned work keep progressing and fail loudly.

User story:

```text
my service can spawn workers or sessions, observe start/failure/result, stop
children on owner stop, refresh replacement addresses, and see starvation before
it becomes mystery latency
```

## Includes

- local multi-shard child ownership for live and sim runtimes
- parent-stop child cleanup across shards
- shard restart propagation for owned children
- replacement address refresh after restart
- failed-peer / failed-shard ingress truth
- non-panic failure policy for user-reported child failure
- supervisor terminal report naming children, restarts, failures, stale
  generations, abandoned work, and final child state
- scheduler/session fairness counters where Tina can observe them
- hot actor versus quiet actor progress proof
- timer progress under hot message load
- remote inbound/local command fairness reports
- starvation-ish lag counters and warnings
- constrained CPU/memory load profiles that end with reports

## Does Not Include

- no network remoting or clustering
- no OS crash isolation
- no `panic = abort` claim
- no hidden global child registry
- no restart that reuses a stale address generation
- no automatic retry policy hidden inside supervision
- no strict real-time guarantee
- no global priority scheduler
- no benchmark marketing
- no hidden buffering to improve fairness numbers
- no OS scheduling promise

## Must Not Change

- Existing same-shard spawn, `spawn_observed`, child ref, restart budget, and
  panic-restart behavior keep their current public outcomes.
- Stale address generation rejection stays loud.
- Parent/child lineage remains runtime-owned; no app-side registry becomes the
  blessed path.
- Existing scheduler order and trace determinism stay stable unless a specific
  unfairness bug requires a semantic change and the trace/hash impact is pinned.
- Existing capacity/pressure reports keep their fields and meanings.
- Existing protocol/session behavior must not gain hidden retry or buffering.

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
FairnessReport
ReadyTurnLag
TimerLateBy
RemoteDrainYield
StarvationWarning
```

Rules:

- Parent ownership is explicit. A child has one owner unless the API says
  otherwise.
- Restart creates a new generation. Stale addresses reject visibly.
- Replacement address refresh is a typed message/report, not trace spelunking.
- Parent stop first stops admission, then stops or drains owned children, then
  emits a report.
- Local multi-shard child ownership is part of this phase: start, stop, restart
  report, and stale replacement address truth. Network remoting/clustering
  remains typed unsupported.
- Non-panic child failure is a normal typed outcome. Panic failure remains
  visible separately.
- "Lag" must be Tina-observable: turns waited, runtime time late, progress
  counts, or bounded drain yields.
- If fairness cannot be guaranteed, report the bad condition instead of hiding
  it.
- Do not retry, buffer, or reprioritize invisibly.
- Reports compose with existing pressure/capacity summaries.
- Stable trace/fact tags append only; never renumber.
- If runtime scheduling semantics change, update saved replay expectations
  intentionally and prove unaffected traces do not churn.

## User Proof Specimens

- supervised worker pool: parent spawns children, one fails, replacement starts,
  parent learns new address
- supervised session service: parent stop closes children and reports all
  terminal child outcomes
- cross-shard child service: parent owns a child on another shard, receives
  start/fail/restart/address-change truth, then stops it cleanly
- hot-key service with quiet-key progress under child churn
- timer-driven flusher under heavy ingress
- multi-shard remote flood plus local shutdown command

## Required Proof

- live single-shard child restart yields new generation and stale address
  rejection
- live multi-shard parent-stop child cleanup
- simulator replay of child start/fail/restart/stop sequence
- failure-before-start reports no hidden child
- failure-after-start reports child id/generation and owner id
- owner stop while child has in-flight call settles caller visibly
- restart budget exhaustion stops restarting and reports final state
- hot self-sending isolate does not starve unrelated isolate beyond documented
  profile, or emits `StarvationWarning`
- recurring timer records progress/missed ticks under hot load
- remote inbound flood does not starve local runtime command
- constrained CPU/memory smoke plateaus or fails with typed pressure
- final reports prove no leaked leases/permits/body charges/pending calls
- long soak profile is ignored/opt-in with documented command; CI profile is
  small and deterministic
- blast-radius proof: existing same-shard supervision, `spawn_observed`,
  restart-budget, dispatcher, multi-shard fairness, timer, and protocol tests
  still pass through the public path
- saved replay hashes only change for scenarios whose event shape intentionally
  changed

## Hostile Review Notes

- Do not build clustering.
- Do not hide stale address refresh in docs-only advice.
- Do not claim cross-shard ownership unless a live test proves it.
- Do not collapse panic, typed failure, budget exhaustion, owner stop, and
  starvation into one vague `Closed`.
- Do not call one happy-path test fairness.
- Do not pretend wall-clock scheduling is deterministic.
- If a fairness test flakes twice, treat it as a bug.
- Do not add queues whose only job is hiding unfairness.
