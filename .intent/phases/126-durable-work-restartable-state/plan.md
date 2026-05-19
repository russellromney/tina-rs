# Phase 126: Durable Work And Restartable State

## Status

- Future implementation plan for the first post-122 core wave.
- Can run beside supervision/fairness if ownership stays in local persistence,
  durable queues/outboxes, and restart systems.

## Purpose

Make local Tina services useful after restart.

User story:

```text
my service records work before doing it, restarts, and resumes or reports the
truth without double-applying hidden work
```

## Includes

- `DurableOutbox` first form with bounded capacity and stable `WorkId`
- append-before-apply service helper
- restart recovery report
- corrupt-tail, truncated-tail, and uncertain-commit outcomes
- bounded replay of pending durable work
- typed duplicate/complete detection
- persistent keyspace or webhook-outbox system specimen

## Does Not Include

- no exactly-once claim
- no distributed transaction
- no database replacement
- no durable mailbox
- no cross-process locking unless a platform backend already proves it
- no generic durable queue abstraction without the outbox proof below

## Must Not Change

- Existing snapshot/journal APIs and recovery outcomes keep their current
  meaning.
- Existing persistence trace facts and durable simulator image behavior keep
  replay compatibility.
- Persistence remains local. This phase does not make a distributed guarantee.

## Implementation Shape

Use names from user workflow:

```text
DurableWork
DurableOutbox
Outbox
RecordedWork
CommittedWork
RecoveryReport
TailStatus
ApplyStatus
```

Rules:

- Record before apply. The helper must make apply-before-record impossible or
  loudly rejected.
- First form is at-least-once outbox semantics: after recovery, pending work may
  run again unless it was marked complete. The report names replayed work ids.
- A failed append returns the original work item.
- Mark-complete is idempotent by `WorkId`; duplicate apply/completion is a typed
  `AlreadyCompleted` / `DuplicateWork` outcome, not silent success.
- Replay is bounded by configured queue/log limits.
- Recovery separates: clean, truncated tail repaired, corrupt tail rejected,
  uncertain commit.
- Shutdown drains or reports pending durable work. No silent drop.

## User Proof Specimens

- webhook outbox: enqueue, send, mark sent, restart, resume unsent
- persistent keyspace: append mutation, snapshot, restart, recover state
- durability-misorder attempt: a compile-fail/user proof that mutation cannot
  apply before durable record success

## Required Proof

- full durable queue returns `Full`
- append failure returns original work
- process restart resumes pending work exactly as documented
- completed work is not double-applied after replay
- corrupt checksum stops or rejects recovery visibly
- truncated tail is repaired or warned, not called clean success
- uncertain commit is distinct from corrupt and clean
- simulator durable image replay matches live projection for supported
  operations; unsupported live facts are typed and replay rejects them by name
- shutdown report names pending, completed, failed, and abandoned durable work
- trybuild test proves apply-before-record cannot compile if type-state helper
  exists
- blast-radius proof: existing snapshot/journal corruption, truncation, and
  replay tests still pass; no existing recovery outcome is renamed silently

## Hostile Review Notes

- Do not say exactly-once.
- Do not mutate memory before durable record success.
- Do not hide duplicate replay behind "probably fine."
- Do not make the durable queue unbounded because disks are big.
