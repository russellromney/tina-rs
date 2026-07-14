# specimen-local-io-codec-ipc

Canonical local I/O, codec, and IPC specimens. Every actor owns its terminal
report, releases its files, streams, and listeners, then publishes the typed
result with `stop_with`. Simulator and live hosts register `observe_result`
before starting the actor; no result crosses an `Arc<Mutex<_>>` side channel.

The binary exposes four flows:

- `file-ingest` uses `FileReadChunks` and `FileCopyBounded`. The ingest reports
  EOF and cap exhaustion separately. The copy waits for both file-close
  continuations before returning its destination contents and loop report.
  The simulator seeder is also observed, so setup failures cannot disappear.
- `admin-socket` uses `LineFramer` for decode and bounded
  `UnixFramedWriter::lines` batches for normal commands and responses. It
  flushes responses preceding `shutdown`, closes the one-shot listener and
  stream, and returns complete decoded response frames.
- `framed-keyspace` uses `LengthDelimitedFramer` and bounded
  `UnixFramedWriter::length_delimited` batches. The client reads until every
  expected acknowledgement frame is decoded rather than treating one socket
  read as one protocol response.
- `live-unix` uses fallible `LocalSystem` startup with
  `DefaultThreadedMailboxFactory`, then waits directly on the probe's typed
  stop result. Unix platforms must bind and close successfully; other
  platforms must return `CallError::Unsupported`.

The two bad-input proofs intentionally bypass `UnixFramedWriter` with
`UnixWriteAll` so they can inject malformed wire bytes. Normal application
traffic never constructs a service envelope or manually encodes a frame.

All public run and smoke functions are fallible. Invalid zero chunk/body
configuration, bounded frame refusal, host admission, observation, rail,
protocol, and cleanup outcomes remain distinct typed errors.

## Run

```sh
cargo run -p specimen-local-io-codec-ipc -- file-ingest
cargo run -p specimen-local-io-codec-ipc -- admin-socket
cargo run -p specimen-local-io-codec-ipc -- framed-keyspace
cargo run -p specimen-local-io-codec-ipc -- live-unix
cargo run -p specimen-local-io-codec-ipc -- all
```

## Acceptance tests

```sh
cargo test -p specimen-local-io-codec-ipc --all-targets
```

The suite covers EOF, honest cap exhaustion, zero caps, empty payloads,
partial writes, bounded frame refusal, malformed raw injection, response
completion, exact two-file cleanup, repeated live bind/close, and terminal
result observation.
