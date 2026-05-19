# Phase 130: Storage, File, And IPC Completion

## Status

- Future implementation plan for the second post-122 core wave.
- Runs after Phase 117. This phase owns the production-shaped local OS pieces
  that remain core after first-form file/codec/IPC support.

## Purpose

Finish boring local OS work that real services need.

User story:

```text
my Tina service can stream files, speak framed local protocols, and use local
IPC without falling back to Tokio
```

## Includes

- file read/write streaming ownership polish
- directory fsync / rename-commit with backend capability truth
- Unix socket listener/client lifecycle and pressure parity with TCP
- line and length-delimited codecs in real local IPC service
- explicit unsupported facts where live/sim/platform support differs
- local sidecar/admin service specimen

## Does Not Include

- no database
- no distributed filesystem
- no cross-platform fake fsync guarantee
- no unbounded file buffering
- no async runtime interop bridge

## Implementation Shape

Use OS-user names:

```text
FileStream
FileWriteCommit
RenameCommit
DirectorySync
UnixListener
UnixStream
FramedStream
```

Rules:

- File streaming must cap resident bytes.
- Rename commit reports platform support and failure truth.
- Directory sync is supported only where the backend proves it; otherwise typed
  unsupported.
- Codecs are sync state machines; Tina owns I/O and pressure.
- Unix sockets use the same lifecycle/capacity/report words as TCP where
  possible.

## User Proof Specimens

- static file responder: streams a large file without full buffering
- local admin sidecar over Unix socket with line or length codec
- append/rename commit specimen with platform capability report

## Required Proof

- large file stream stays under configured chunk/body cap
- write failure returns typed partial/failed truth
- rename commit succeeds on supported backend capability and returns typed
  unsupported elsewhere
- Unix socket request/reply local sidecar works live
- malformed framed input rejects without corrupting next valid frame
- shutdown closes file/IPC rails and reports final current counts
- simulator support is either implemented or declared unsupported with replay
  facts

## Hostile Review Notes

- Do not load whole files to prove streaming.
- Do not claim fsync semantics the platform cannot prove.
- Do not make codecs own hidden buffers outside Tina capacity.
- Do not duplicate Phase 117 first-form work. This phase is the production
  completion pass: lifecycle, pressure, commit truth, and e2e local sidecar
  proof.
