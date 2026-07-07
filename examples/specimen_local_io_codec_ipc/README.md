# specimen-local-io-codec-ipc

Specimen for the local I/O, codec, and IPC parity specimen (Local I/O, Codec, and IPC Parity).

Flows, one binary:

- `file-ingest` — bounded file streaming via
  `tina_runtime::FileReadChunks`, plus a bounded `FileCopyBounded`
  pump that owns the read/write alternation while still surfacing one
  continuation per rail completion. The smoke reads a small payload
  and copies one; the bad-input proof exercises a cap shorter than the
  file and asserts `FileLoopEnd::CapReached` instead of a silent
  truncation.
- `admin-socket` — local admin sidecar over a simulator Unix-domain
  socket pair with line-delimited commands from
  `tina_codec::LineFramer` and the `UnixReadToEof` / `UnixWriteAll`
  loop shape. Smoke run sends three commands; the bad-input proof
  feeds an over-long line and asserts the framer surfaces `Full` and
  the connection is torn down.
- `framed-keyspace` — mini-keyspace protocol with length-prefixed
  frames using `tina_codec::LengthDelimitedFramer`. Smoke run does
  `set`/`get`; the bad-input proof feeds a frame whose declared
  length exceeds the configured cap and asserts the framer rejects
  before allocation.
- `live-unix` — drives the **live** runtime through one
  `unix_bind` / `unix_close_listener` cycle. On Unix the live
  OS-backed lane binds a real socket; off Unix the call returns typed
  `CallError::Unsupported`. Either is a pass for its platform.

The IPC flows run on the deterministic simulator so the framed
protocol logic is replayable; `live-unix` exercises the real
OS-backed rail.

## Run

```sh
cargo run -p specimen-local-io-codec-ipc -- file-ingest
cargo run -p specimen-local-io-codec-ipc -- admin-socket
cargo run -p specimen-local-io-codec-ipc -- framed-keyspace
cargo run -p specimen-local-io-codec-ipc -- live-unix
cargo run -p specimen-local-io-codec-ipc -- all
```

## Acceptance tests

`cargo test -p specimen-local-io-codec-ipc` runs every smoke flow and
every bad-input proof.
