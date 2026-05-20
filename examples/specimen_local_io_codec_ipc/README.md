# specimen-local-io-codec-ipc

Specimen for Phase 117 (Local I/O, Codec, and IPC Parity).

Three flows, one binary:

- `file-ingest` — bounded file streaming via
  `tina_runtime::FileReadChunks`. Smoke run reads a small payload; the
  bad-input proof exercises a cap shorter than the file and asserts
  `FileLoopEnd::CapReached` instead of a silent truncation.
- `admin-socket` — local admin sidecar over a simulator Unix-domain
  socket pair with line-delimited commands from
  `tina_codec::LineFramer`. Smoke run sends three commands; the
  bad-input proof feeds an over-long line and asserts the framer
  surfaces `Full` and the connection is torn down.
- `framed-keyspace` — mini-keyspace protocol with length-prefixed
  frames using `tina_codec::LengthDelimitedFramer`. Smoke run does
  `set`/`get`; the bad-input proof feeds a frame whose declared
  length exceeds the configured cap and asserts the framer rejects
  before allocation.

A fourth subcommand `live-unix` runs the simulator-side smoke and
points at the matching live-driver "typed `Unsupported`" deferral.
Live Unix-domain rails ship typed `Unsupported` from the live driver
in this slice; the simulator implements the full byte-stream
semantics.

## Run

```sh
cargo run -p specimen-local-io-codec-ipc -- file-ingest
cargo run -p specimen-local-io-codec-ipc -- admin-socket
cargo run -p specimen-local-io-codec-ipc -- framed-keyspace
cargo run -p specimen-local-io-codec-ipc -- all
```

## Acceptance tests

`cargo test -p specimen-local-io-codec-ipc` runs every smoke flow and
every bad-input proof.
