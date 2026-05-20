# Phase 117: Local I/O, Codec, And IPC Parity

## Status

- First slice landed.
- Shipped: `tina_runtime::file_loops` helpers (`FileReadChunks`,
  `FileWriteAll`, `FileCopyBounded`), `tina-codec` battery crate
  (`LineFramer`, `LengthDelimitedFramer`,
  `FrameDecision::{NeedMore,Frame,Malformed,Full}`), Unix-domain rails on
  the public surface (`unix_bind`/`unix_accept`/`unix_connect`/
  `unix_read`/`unix_write`/`unix_close_listener`/`unix_close_stream`),
  full simulator support for Unix sockets, and
  `examples/specimen_local_io_codec_ipc` (file ingest, admin sidecar,
  framed mini-keyspace).
- Honest deferral: live OS-backed Unix-domain support is **not** in this
  slice. The live driver returns typed `CallError::Unsupported` on every
  platform for Unix rails. The simulator implements the full
  byte-stream pair semantics. Future work: implement the live Unix
  worker lane (likely along the existing `process_run` / `storage`
  worker-thread pattern).

## Layering

Phase 115 separated core from batteries (see
`docs/tina-user-guide/23-core-and-batteries.md`). This phase respects that
line:

- **Core** (`tina-runtime`, `tina-sim`): new public rails — file streaming
  source, Unix-domain socket rail — land in `tina-runtime` and gain
  scripted equivalents in `tina-sim`. These are runtime semantics, not
  battery features.
- **Codec battery** (new or in `tina-http`): codec helpers (line-delimited,
  length-delimited, sync-codec adapter) sit on top of public rail bytes.
  They never reach into runtime internals.
- **Local IPC battery / specimens**: local sidecar/admin IPC specimens
  consume the public rails and codec helpers as ordinary user code.

Codec helpers may live in a small new battery crate or inside `tina-http`
behind a feature flag; either way they obey the official battery rules in
`docs/tina-user-guide/23-core-and-batteries.md`.

## Purpose

Close common Tokio replacement gaps outside HTTP:

```text
files, framed bytes, and local sidecar/admin sockets
```

Tina owns I/O, capacity, cancellation, and replay. Codecs own bytes.

## Starting Facts

- File rails already exist: open/read-at/write-at/fsync/size/close, live and
  sim.
- TCP loop helpers already exist: `TcpWriteAll`, `TcpReadExact`,
  `TcpReadToEof`. They prove the helper shape: user-owned state machine, one
  runtime call per progress step.
- File rails are offset-shaped. File loop helpers must own explicit offset
  progress and return partial-progress reports on cancel/failure; they must not
  pretend a file stream is the same thing as a TCP stream.
- HTTP/1, chunked, WebSocket, HTTP/2, and gRPC all have private codec-ish
  parsers. This phase should not expose those exact internals; it should ship a
  generic sync codec pattern that matches their lessons.
- Unix-domain sockets are not present as runtime rails. Lifecycle docs mention
  Unix as a resource kind, but no `unix_bind` / `unix_connect` surface exists.
- Add `tina-codec` as the official codec battery crate. Do not put codecs in
  `tina` core.
- New runtime call kinds / trace tags for Unix rails must be appended to stable
  hash mappings. Do not renumber existing call/effect/protocol tags.

## Includes

- bounded file streaming read helper
- bounded file streaming write helper
- file streaming helpers are state machines over existing file rails, not new
  backend file primitives
- file helpers keep per-chunk trace truth; no hidden read-whole-file path
- line-delimited codec helper
- length-delimited codec helper
- sync codec adapter pattern with `NeedMore` / `Frame` / `Malformed` / `Full`
- codecs are pure data; runtime owns socket reads/writes and capacity
- public codec names should be task names, not storage names:
  - `LineFramer`
  - `LengthDelimitedFramer`
  - `FrameDecision::{NeedMore, Frame, Malformed, Full}`
- Unix-domain socket listener/client rails for local IPC
- Unix rails mirror TCP semantics for these first-form calls:
  - bind/listen/accept
  - connect
  - read/write/close
  - typed `Full`, `Closed`, `InvalidResource`, `Unsupported`
- typed non-Unix unsupported truth for Unix rails
- simulator support for file streaming and Unix socket pairs; non-Unix live
  backends return typed unsupported for Unix rails
- Windows/non-Unix must compile. Unix-specific live tests are cfg-gated, but
  unsupported rails still report typed unsupported instead of disappearing.
- system specimens:
  - media/file ingest
  - local admin sidecar
  - framed mini keyspace protocol

## Does Not Include

- no async codec trait
- no hidden Tokio
- no unbounded file buffering
- no production database wire protocol
- no mmap/zero-copy promise
- no moving HTTP/WebSocket parsers into public API
- no driver-level `file_read_to_end` that hides per-chunk progress

## Blast Radius

Medium blast radius.

- Allowed: `tina-runtime` file loop helpers, Unix socket rails, `tina-sim`
  scripted Unix/file behavior, new codec battery crate, local IPC/file
  specimens.
- Not allowed: HTTP/WebSocket parser rewrites, database protocol work, hidden
  async codec traits, broad driver refactor beyond new Unix rail plumbing.
- Unix sockets must be additive. Existing TCP/TLS/file tests should not need
  behavior changes.

## Proof Shape

- large file does not buffer whole file
- empty file, exact chunk boundary, max-total-overrun, and zero-chunk config are
  pinned
- file read helper reads in multiple chunks and reports high-water/cap truth
- file write helper handles partial writes and fsync/close truth
- file helper cancel after partial progress returns a partial-progress report or
  typed cancel; it must not claim full success
- cancellation/shutdown of an in-flight file stream reports what completed and
  what did not; no fake all-or-nothing story
- slow reader/writer pressure is visible
- malformed frame is typed
- line codec rejects line too large without growing unbounded
- length codec rejects frame too large before allocation
- Unix socket close/cancel/drain truth is visible
- Unix socket wrong-resource operations return typed errors, not TCP-shaped
  accidental success
- Unix peer close while read/write is pending settles visibly
- Unix socket live echo/admin specimen works on Unix
- live Unix and sim tests cover the same framed protocol shape; non-Unix live
  tests assert typed unsupported
- non-Unix tests assert typed unsupported capability, not cfg-silent omission
- compile-fail tests keep codec adapter state typed, not stringly
- doctests show codec state living on an isolate and one runtime read per
  progress step
- every new specimen has a smoke command and one bad-input proof

## Implementation Shape

- Add `tina_runtime::file_loops` beside `tcp_loops`:
  - `FileReadChunks`
  - `FileWriteAll`
  - `FileCopyBounded`
  - explicit max chunk and max total
  - no zero chunk
  - no hidden allocation beyond configured buffer
  - explicit offset progress in every continuation
  - terminal report includes bytes read/written, final offset, and whether the
    helper ended by done, EOF, cancel, cap, or error
- Add `tina-codec` as an official small battery crate:
  - `LineFramer`
  - `LengthDelimitedFramer`
  - `FrameDecision::{NeedMore, Frame, Malformed, Full}`
  - parser state is owned by the isolate/specimen, not a background task
- Add Unix rail calls in `tina-runtime` and `tina-sim`:
  - live Unix platforms use OS Unix sockets
  - live non-Unix platforms complete with typed unsupported
  - simulator models Unix sockets as local byte-stream pairs with Unix-socket
    resource names
  - use distinct `UnixListenerId` and `UnixStreamId`; do not reuse TCP ids
- Add specimens:
  - `specimen_file_ingest`
  - `specimen_local_admin_socket`
  - `specimen_framed_keyspace`

## Hostile Review Notes

- Do not make codecs async. That would smuggle a second runtime into Tina.
- Do not hide read/write loops inside the driver without per-step trace truth.
- Do not let Unix socket support be cfg-silent. A non-Unix user must see a
  typed unsupported capability.
- Do not expose HTTP/WebSocket private parser types as the generic codec API
  just because they are nearby.
