# tina fuzz targets

Coverage-guided fuzzing for the hand-rolled byte-level decoders. This crate
is excluded from the workspace (its own `[workspace]` table) so the unstable
`cargo-fuzz` toolchain never touches normal builds.

## Run

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run <target> -- -max_total_time=60
```

## Cadence

CI does not fuzz per-PR (too slow to gate on). Instead:

- **Per PR**: `cargo check -p tina-http --features fuzzing --locked` guards the
  shim signatures, and the seed corpus + documented panic shapes are folded
  into deterministic unit tests that run in the normal suite — the HPACK panic
  inputs through `decode_headers_block`, the chunked-decoder cap invariant, the
  h2 payload views, and the rpc frame decode (`tina-http` lib `http2` tests +
  the `fuzz_corpus_regression` integration test). So every PR exercises the
  panic-containment property on the real production functions.
- **Weekly + manual** (`verify.yml` `fuzz` job): installs `cargo-fuzz`, builds
  every target (catching target bit-rot the shim check cannot see), and runs
  each for `-max_total_time=60`. Trigger on demand via *workflow_dispatch*.

## Targets

| Target | Exercises |
|---|---|
| `chunked_decoder` | Incremental chunked-body decode; asserts the decoded-length cap holds across a split feed. |
| `http1_request_head` | `parse_request_head` under default limits. |
| `http1_response_head` | `parse_response_head` under default limits. |
| `rpc_frame` | `decode` (full frame, default limits) and `decode_body` (trusts the length prefix). |
| `h2_frame_meta` | HTTP/2 frame-header decode + max-frame-size rejection. |
| `h2_payload` | HTTP/2 DATA/HEADERS padding + priority stripping (`data_payload_view` / `headers_payload_view`). |
| `hpack_headers` | The HPACK soundness walker vs `hpack::Decoder::decode` — see below. |
| `ws_frame` | Hand-rolled WebSocket frame parser (client + server framing: 7/16/64-bit lengths, mask XOR, buffer draining). |
| `grpc_frame` | Hand-rolled gRPC length-prefix reassembler (`next_grpc_frame_boundary` in `grpc.rs`) — header check, `max_message_bytes` cap, boundary/drain math. Never touches `prost` decode. |

`h2_frame_meta`, `h2_payload`, `hpack_headers`, `ws_frame`, and `grpc_frame`
reach `pub(super)`/`pub(crate)` internals through the `fuzzing` feature on
`tina-http` (`http2::fuzzing`, `websocket::fuzzing`, `grpc::fuzzing`), never
enabled in production.

## The `grpc_frame` target

`GrpcRequestStream::next_buffered_message` used to be untestable in isolation:
its constructor requires a runtime-coupled `Http2RequestStream`, and its
decode step delegates to `prost` (fuzzed upstream, not our concern). The pure
length-prefix state machine — the 5-byte header check, the `max_message_bytes`
cap on the declared length, and the `drain`-bounded end offset — is now
extracted into `next_grpc_frame_boundary`, a free function taking `&mut
Vec<u8>` and a cap with no connection coupling. `next_buffered_message` calls
it and only owns the `T::decode` / `GrpcStatus` mapping on top.

The target (`grpc::fuzzing::fuzz_grpc_frame_reassembly`) feeds arbitrary bytes
through the boundary function in a loop — draining each `Ready` boundary and
re-running it, so one input can exercise several concatenated frames — and
asserts only that no boundary offset ever exceeds the buffer length. The seed
corpus (`fuzz/corpus/grpc_frame/`) covers a truncated header, a declared
length over the cap, a declared length near `u32::MAX` with a short body, two
concatenated messages, and a zero-length message; the same shapes are also
deterministic unit tests in `grpc.rs` (`grpc::tests::frame_boundary_*`).

## The `hpack_headers` target

The third-party `hpack` crate `.ok().unwrap()`s a failed integer decode in its
dynamic-table-size-update path, so a truncated or over-long size-update integer
panics *inside* `decode` — attacker-reachable and pre-auth. `catch_unwind` only
contains that on `panic = "unwind"` builds; under `panic = "abort"` it aborts
the process. So `tina-http` gates the decoder behind `hpack_block_is_sound`, a
structural walker that rejects exactly the inputs that would make `hpack`'s
`decode_integer` return `Err` (the only panic trigger), under every panic
strategy.

This target drives the **real** production decode entry
(`http2::headers::decode_headers_block`) — the shipped gate, the fast-literal
path, and the `catch_unwind`, not a private copy of the gate. Under the
fuzzer's `panic = "abort"` it crashes precisely when the gate lets a panic
input reach `decode` (e.g. if someone deletes `hpack_block_is_sound`), so a
clean run is evidence the shipped entry contains every input — the security
property, made load-bearing under `abort`. Driving production also gives the
fast-literal path (`decode_fast_literal_headers`, which runs first on every
inbound block) its only fuzz coverage.

It is one-sided: it cannot see a walker that over-*rejects* a valid block.
Completeness (no false rejects) is covered by
`walker_gates_every_short_block_against_the_real_decoder` and
`walker_gates_deep_size_update_continuations_against_the_real_decoder` in
`tina-http` — exhaustive differential unit tests over all 1-/2-byte blocks and
all 3-byte size-update blocks, run under `panic = "unwind"`, checking both
directions. The `panic = "unwind"` regression that the gate is load-bearing
(deleting it makes a test fail rather than silently pass) lives in
`hpack_gate_rejects_before_decode_not_via_catch_unwind`.

The seed corpus for this target holds three genuine panic inputs (truncated,
unterminated, and over-long size updates) plus one well-formed block. The
other targets ship no seeds, except `grpc_frame` (see above).
