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

`h2_frame_meta` reaches `pub(super)` internals through the `fuzzing` feature
on `tina-http` (`http2::fuzzing`), which is never enabled in production.

## Known finding, not yet covered here

An HPACK header-block target is intentionally absent. Fuzzing found that the
third-party `hpack` crate `.ok().unwrap()`s a truncated dynamic-table-size
update and panics inside `decode`. `decode_headers_block_with_storage` now
contains that panic with `catch_unwind` and maps it to a protocol error, so
the default (`panic = "unwind"`) build returns a clean error — but a service
built with `panic = "abort"` would still abort on that input. The complete
fix (replace or pre-validate ahead of `hpack`) is tracked separately; the
target returns once the decode path is panic-free under both strategies.
