# Phase 126: Durable Local State And IPC

## Status

- Future implementation plan for the first post-122 core wave.
- Combines the old durable-state and storage/file/IPC completion plans.
- Can run beside runtime supervision/fairness if ownership stays in local
  persistence, file rails, local IPC, codecs, and restart systems.

## Purpose

Make local Tina services survive restart and own boring local OS work.

User story:

```text
my service records work before doing it, restarts, resumes or reports the truth,
streams files, and speaks framed local protocols without falling back to Tokio
```

## Includes

- `DurableOutbox` first form with bounded capacity and stable `WorkId`
- append-before-apply service helper
- restart recovery report
- corrupt-tail, truncated-tail, and uncertain-commit outcomes
- bounded replay of pending durable work
- typed duplicate/complete detection
- file read/write streaming ownership polish
- directory fsync / rename-commit with backend capability truth
- Unix socket listener/client lifecycle and pressure parity with TCP
- line and length-delimited codecs in real local IPC service
- explicit unsupported facts where live/sim/platform support differs
- persistent keyspace, webhook-outbox, static-file, and local-sidecar specimens

## Does Not Include

- no exactly-once claim
- no distributed transaction
- no database replacement
- no durable mailbox
- no cross-process locking unless a platform backend already proves it
- no generic durable queue abstraction without the outbox proof below
- no distributed filesystem
- no cross-platform fake fsync guarantee
- no unbounded file buffering
- no async runtime interop bridge

## Must Not Change

- Existing snapshot/journal APIs and recovery outcomes keep their current
  meaning.
- Existing persistence trace facts and durable simulator image behavior keep
  replay compatibility.
- Existing file/path/persistence rail outcomes keep their current names and
  meanings.
- Existing simulator unsupported facts remain honest; no platform support is
  faked to satisfy a specimen.
- Existing TCP lifecycle vocabulary remains the model for Unix sockets.
- Persistence remains local. This phase does not make a distributed guarantee.

## Implementation Shape

Use names from user workflow and local OS work:

```text
DurableWork
DurableOutbox
RecordedWork
CommittedWork
RecoveryReport
TailStatus
ApplyStatus
FileStream
FileWriteCommit
RenameCommit
DirectorySync
UnixListener
UnixStream
FramedStream
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
- File streaming caps resident bytes.
- Rename commit reports platform support and failure truth.
- Directory sync is supported only where the backend proves it; otherwise typed
  unsupported with capability report evidence.
- Codecs are sync state machines; Tina owns I/O and pressure.
- Unix sockets use the same lifecycle/capacity/report words as TCP where
  possible.

## User Proof Specimens

- webhook outbox: enqueue, send, mark sent, restart, resume unsent
- persistent keyspace: append mutation, snapshot, restart, recover state
- durability-misorder attempt: compile-fail/user proof that mutation cannot
  apply before durable record success
- static file responder: streams a large file without full buffering
- local admin sidecar over Unix socket with line or length codec
- append/rename commit specimen with platform capability report

## Required Proof

- full durable outbox returns `Full`
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
- large file stream stays under configured chunk/body cap
- write failure returns typed partial/failed truth
- rename commit succeeds on supported backend capability and returns typed
  unsupported elsewhere
- Unix socket request/reply local sidecar works live
- malformed framed input rejects without corrupting next valid frame
- shutdown closes file/IPC rails and reports final current counts
- blast-radius proof: existing snapshot/journal corruption, truncation, replay,
  file/path/persistence, and Phase-117 first-form file/codec/IPC tests still
  pass; no existing recovery outcome is renamed silently

## Hostile Review Notes

- Do not say exactly-once.
- Do not mutate memory before durable record success.
- Do not hide duplicate replay behind "probably fine."
- Do not make the durable outbox unbounded because disks are big.
- Do not load whole files to prove streaming.
- Do not claim fsync semantics the platform cannot prove.
- Do not make codecs own hidden buffers outside Tina capacity.
