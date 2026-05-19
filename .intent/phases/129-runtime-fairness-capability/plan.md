# Phase 129: Runtime Fairness As Capability

## Status

- Future implementation plan for the first post-122 core wave.
- Builds on Phase 121. Phase 121 proves fairness/load behavior; this phase adds
  runtime/service capability surfaces where those proofs expose real starvation
  or lag.

## Purpose

Make starvation visible and reduce avoidable starvation.

User story:

```text
one hot actor, session, timer, or remote edge should not quietly starve the
rest of my service
```

## Includes

- scheduler/session fairness counters
- hot actor versus quiet actor progress proof
- timer progress under hot message load
- protocol session fairness reports
- remote inbound/local command fairness reports
- starvation-ish lag counters where Tina can observe them
- constrained CPU/memory load profiles that end with reports

## Does Not Include

- no strict real-time guarantee
- no global priority scheduler
- no benchmark marketing
- no hidden buffering to improve fairness numbers
- no OS scheduling promise

## Must Not Change

- Existing scheduler order and trace determinism stay stable unless a specific
  unfairness bug requires a semantic change and the trace/hash impact is pinned.
- Existing capacity/pressure reports keep their fields and meanings.
- Existing protocol/session behavior must not gain hidden retry or buffering.

## Implementation Shape

Use honest observable names:

```text
FairnessReport
ReadyTurnLag
TimerLateBy
SessionProgress
RemoteDrainYield
StarvationWarning
```

Rules:

- "Lag" must be Tina-observable: turns waited, runtime time late, progress
  counts, or bounded drain yields.
- If fairness cannot be guaranteed, report the bad condition instead of hiding
  it.
- Do not retry, buffer, or reprioritize invisibly.
- Reports must compose with existing pressure/capacity summaries.
- Stable trace/fact tags append only; never renumber.
- If runtime scheduling semantics change, update saved replay expectations
  intentionally and prove unaffected traces do not churn.

## User Proof Specimens

- hot-key service with quiet key progress
- 1000-session chat/load profile with slow peer
- timer-driven flusher under heavy ingress
- multi-shard remote flood plus local shutdown command

## Required Proof

- hot self-sending isolate does not starve unrelated isolate beyond documented
  profile, or emits `StarvationWarning`
- recurring timer records progress/missed ticks under hot load
- one blocked protocol session does not stop unrelated admitted session
- remote inbound flood does not starve local runtime command
- constrained CPU/memory smoke plateaus or fails with typed pressure
- final reports prove no leaked leases/permits/body charges/pending calls
- long soak profile is ignored/opt-in with documented command; CI profile is
  small and deterministic
- blast-radius proof: existing dispatcher, multi-shard fairness, timer, and
  protocol tests still pass; saved replay hashes only change for scenarios whose
  event shape intentionally changed

## Hostile Review Notes

- Do not call one happy-path test fairness.
- Do not pretend wall-clock scheduling is deterministic.
- If a fairness test flakes twice, treat it as a bug.
- Do not add queues whose only job is hiding unfairness.
