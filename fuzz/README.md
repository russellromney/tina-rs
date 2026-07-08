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
| `hpack_headers` | The HPACK soundness walker vs `hpack::Decoder::decode` — see below. |

`h2_frame_meta` and `hpack_headers` reach `pub(super)` internals through the
`fuzzing` feature on `tina-http` (`http2::fuzzing`), never enabled in
production.

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
