# Tina/Tinio 0.1.0 Rust Remediation Report

**Date:** 2026-07-09

**Scope:** Implementation follow-up to
[`tina-0.1.0-rust-code-review.md`](tina-0.1.0-rust-code-review.md). This report records
the code changes on `codex/tina-0.1-ergonomics-review`, the user-visible and
deterministic-simulation coverage added for them, and the work deliberately left
for a later change.

## Implemented

### Codec input is independent of transport chunking

- `Framer::feed` now reports how many bytes it consumed and stops at a single
  current frame rather than treating an arbitrary read chunk as one frame.
- `decode_chunk` alternates feeding and draining so coalesced frames are decoded
  without an unbounded decoded-frame queue.
- Length-delimited and line framers no longer repeatedly drain the front of a
  `Vec`; complete frame storage is moved out instead.
- Exhaustive partition tests cover maximum-size frames followed by another frame,
  exact-capacity CRLF input, terminal suffixes, and custom-codec examples.

### Buffered HTTP/2 responses obey flow control

- Response HEADERS are emitted immediately and buffered bodies are advanced by a
  cursor using the available connection and stream credit.
- DATA resumes on WINDOW_UPDATE and checked conversions replace narrowing casts.
- A live raw-peer test transfers a 256 KiB response through the default window;
  deterministic simulator tests exercise the same response state machine.

### Deadline arithmetic is checked consistently

- Public duration paths use checked/saturating deadline construction across
  runtime drivers, shutdown, DNS, TLS, process, storage, HTTP keepalive, Tokio,
  reqwest, AWS, the proof harness, and simulator timers.
- Tokio waits use absolute instants so retry work does not extend the caller's
  deadline.
- Live and deterministic tests cover `Duration::MAX` and replay behavior.

### Tokio RPC construction and calls are spawn-safe

- The bridge marker now models a type relationship without pretending to own the
  shard type, so call futures remain `Send` without an unnecessary `S: Sync` bound.
- Capacity validation is fallible, including checked addition.
- Registration errors retain their concrete source, and a real bridge call is
  exercised through `tokio::spawn`.

### Write-all loops retain one owned allocation

- TCP, Unix, positional file writes, and bounded file copy use owned buffers plus
  cursors rather than cloning and front-draining after short writes.
- Backends return the owned buffer on both success and failure. Impossible byte
  counts produce `CallError::InvariantViolation` instead of being clamped.
- Tests cover pointer reuse, changed-length buffers, invalid cursors, impossible
  completions, one-byte TCP/Unix writes, and deterministic file ingest/copy.

### Public failures are visible and structured

- `RequestBuilder::try_header` returns a typed error distinguishing invalid names
  and values; CRLF rejection is tested at the public request-builder boundary.
- SQLite, SQLx, reqwest, AWS, and Tokio bridge install errors retain source chains.
- Persistent `u64` lengths and worker-pool capacities use checked narrowing.
- Consuming builders and related configuration types gained selective
  `#[must_use]` annotations.

### Safety and identity boundaries are clearer

- Pool identity now uses a private, process-unique `PoolAuthority` capability
  instead of `unsafe` constructors for a non-memory-safety policy.
- Runtime and SPSC crates deny unsafe operations in unsafe functions and require
  safety documentation for their covered unsafe blocks.
- The SPSC mailbox retains its Loom coverage, and the uninhabited-reference
  implementation was replaced with an explicit unreachable invariant.

## Verification

The final source state passed:

```text
cargo test --workspace --locked --no-fail-fast -j 2
```

The workspace gate ran with `CARGO_TARGET_DIR=/tmp/tina-rs-full-gate`,
`CARGO_INCREMENTAL=0`, and `RUSTFLAGS='-C debuginfo=0'`. It completed with exit
code 0 after exercising all workspace tests and doctests. Focused checks also
covered:

- HTTP live and deterministic HTTP/2 tests.
- Codec unit, trybuild, exhaustive partition, and example integration tests.
- Runtime TCP, Unix, file-loop, deadline, persistence, and simulator tests.
- Tokio RPC spawn and bridge tests.
- Normal and Loom SPSC tests.

The final commit also runs workspace all-target/all-feature Clippy with warnings
denied, formatting, diff whitespace, and the repository race/rail guards.

## Remaining launch work

### Simulator file-size admission

Simulated positional file writes still convert arbitrary `u64` offsets to
`usize` and can resize a file without a total storage cap. Add a checked
`max_file_bytes` policy and deterministic tests for offsets and ranges beyond the
configured limit. This is the most concrete unresolved correctness risk.

### Request-effect logical capability

`request_effect_from_consumed_effect` still uses `unsafe` to mark a logical
must-answer escape hatch. Removing it correctly requires a permit threaded
through deferred request wrappers, pending/wait tickets, shared work, and compile
tests. A partial rename would preserve the policy problem without enforcing the
authority, so this remains a separate, explicit API change.

### Vendored substrate audit

The new lint coverage does not constitute a complete cross-platform audit of the
customized Betelgeuse substrate. Audit and document the Linux and Darwin raw I/O
paths, completion pointer casts, and slab access, then enable equivalent lints on
all compiled platform modules.

### Coverage follow-ups

- Add an HTTP/2 buffered response larger than the window with trailers and a
  two-stream fairness test.
- Consider giving `decode_chunk` callbacks control flow so a terminal command can
  stop decoding the remainder of a coalesced input chunk.
- Native kernels do not deterministically force one-byte Unix/file writes; those
  cases are currently deterministic simulator E2E tests.
- The actual Tokio shim-registration failure path is source-chain tested with a
  synthetic source because registration failure is difficult to induce publicly.
