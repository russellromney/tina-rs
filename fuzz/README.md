# tina fuzz targets

Coverage-guided fuzzing for the hand-rolled byte-level decoders. This crate
is excluded from the workspace (its own `[workspace]` table) so the unstable
`cargo-fuzz` toolchain never touches normal builds.

## Run

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run <target> -- -max_total_time=60
```

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

`h2_frame_meta`, `h2_payload`, `hpack_headers`, and `ws_frame` reach
`pub(super)`/`pub(crate)` internals through the `fuzzing` feature on `tina-http`
(`http2::fuzzing`, `websocket::fuzzing`), never enabled in production.

## Not fuzzed, covered by inspection

The gRPC length-prefixed frame reassembler (`GrpcRequestStream::next_buffered_message`
in `grpc.rs`) is hand-rolled but has no target: its constructor requires a
runtime-coupled `Http2RequestStream`, and its only decode work delegates to
`prost` (fuzzed upstream). The tina-specific framing is fully length-guarded —
`buffer.len() < GRPC_FRAME_HEADER_LEN` and `< end` early-returns, a
`max_message_bytes` cap, and a `drain(..end)` bounded by the checked `end` — so
it is panic-safe by construction. Extracting the pure framing into a
standalone function so it can be fuzzed (and unit-tested) directly is a tracked
follow-up.

## The `hpack_headers` target

The third-party `hpack` crate `.ok().unwrap()`s a failed integer decode in its
dynamic-table-size-update path, so a truncated or over-long size-update integer
panics *inside* `decode` — attacker-reachable and pre-auth. `catch_unwind` only
contains that on `panic = "unwind"` builds; under `panic = "abort"` it aborts
the process. So `tina-http` gates the decoder behind `hpack_block_is_sound`, a
structural walker that rejects exactly the inputs that would make `hpack`'s
`decode_integer` return `Err` (the only panic trigger), under every panic
strategy.

This target mirrors the production guard: it decodes only when the walker
accepts. Under the fuzzer's `panic = "abort"` it crashes precisely when the
walker is *unsound* (admits a block that panics `decode`), so a clean run is
evidence of soundness — the security property. It is one-sided: it cannot see
a walker that over-*rejects* a valid block. Completeness (no false rejects) is
covered by `walker_gates_every_short_block_against_the_real_decoder` in
`tina-http`, an exhaustive differential unit test over all 1- and 2-byte blocks
that runs under `panic = "unwind"` and checks both directions.

The seed corpus for this target holds three genuine panic inputs (truncated,
unterminated, and over-long size updates) plus one well-formed block; the other
five targets ship no seeds.
