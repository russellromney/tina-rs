# Phase 117: Local I/O, Codec, And IPC Parity

## Status

- Future IDD outline for Wave A.
- Can run in parallel with phases 116 and 118 if ownership stays mostly in
  runtime rails, codec helpers, and local IPC specimens.

## Purpose

Close common Tokio replacement gaps outside HTTP:

```text
files, framed bytes, and local sidecar/admin sockets
```

Tina owns I/O, capacity, cancellation, and replay. Codecs own bytes.

## Includes

- bounded file streaming read helper
- bounded file streaming write helper
- line-delimited codec helper
- length-delimited codec helper
- sync codec adapter pattern with `NeedMore` / `Frame` / `Malformed` /
  `Full`
- Unix-domain socket listener/client rails for local IPC
- simulator support for file streaming and Unix sockets, or typed unsupported
  truth where a backend cannot support it
- system specimens:
  - media/file ingest
  - local admin sidecar
  - framed mini keyspace or echo protocol

## Does Not Include

- no async codec trait
- no hidden Tokio
- no unbounded file buffering
- no production database wire protocol
- no mmap/zero-copy promise

## Proof Shape

- large file does not buffer whole file
- slow reader/writer pressure is visible
- malformed frame is typed
- Unix socket close/cancel/drain truth is visible
- live and sim tests cover the same protocol shape where possible
- compile-fail tests keep codec adapter state typed, not stringly

