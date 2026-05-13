# Phase 080: HTTP Body Chunked Symmetric

## Status

- Done: shared decoder, client chunked response decode, server chunked request
  streaming, tests, docs, and merge.
- Deferred: trailers, broad transfer-coding support, and HTTP/2.

## Goal

Add symmetric chunked-transfer-encoding support to `tina-http`:
- Client decodes chunked responses into bounded buffered bodies.
- Server accepts chunked requests through the existing request-stream pull model.
- One shared incremental decoder serves both sides.

## Plan

1. **Add one shared incremental chunked decoder** (`tina-http/src/chunked_decoder.rs`).
   - `ChunkedDecoder` with `feed()` for incremental use and `feed_all()` for accumulation.
   - Reject trailers. Parse and ignore chunk extensions deliberately.
   - `ChunkedError` enum: `BadChunkSize`, `MissingCrlf`, `BodyTooLarge`, `TrailersNotSupported`.
   - Unit tests cover: single chunk, multiple chunks, empty body, extensions, bad hex, missing CRLF, body too large, partial feeds, split CRLF, split trailers, max size line rejection.

2. **Make the HTTP client decode chunked responses** (`tina-http/src/client.rs`).
   - `ActiveCall` grows `chunked_decoder: Option<ChunkedDecoder>` and `body_buf: Vec<u8>`.
   - `handle_bytes_read` initializes decoder when `head.chunked` and accumulates decoded chunks.
   - `body_complete` returns `false` for chunked (completion is decoder-driven).
   - Malformed wire surfaces as `HttpClientError::Parse(ResponseParseError::MalformedChunkedBody)`.

3. **Make the HTTP server accept chunked requests** (`tina-http/src/connection.rs`).
   - `RequestStream` gains `chunked: bool` flag.
   - Parser accepts `Transfer-Encoding: chunked` when `inbound_stream_chunk_size.is_some()`.
   - Rejects `Content-Length` + `Transfer-Encoding` in both directions.
   - Connection dispatches chunked requests as `HttpRequestBody::Stream` with `content_length = 0` and `chunked = true`.
   - `chunked_decoder`, `chunked_raw_buffer`, and `inbound_chunked` fields manage state.
   - `handle_body_chunk_read` feeds raw bytes through decoder and chains reads when needed.
   - Malformed wire records `body_io_error` and replies `RequestChunkReply::Error`.

4. **Add one HTTPS parity proof** (`tina-http/tests/body_parity_tls.rs`).
   - `chunked_request_over_https_matches_http_shape`: raw rustls client sends chunked POST over TLS; server decodes via streaming pull and echoes total bytes.

5. **Keep body metrics honest**.
   - Decoded app bytes are charged to `metrics.request_body_*`, not raw wire bytes.
   - Bounded resident bytes: subsequent reads honor `chunk_size`.
   - All charges drain on `Eof` or error.
   - `chunked_request_charges_decoded_bytes_and_drains` integration test verifies.

6. **Update `specimen_http_body_streaming` and docs**.
   - Correct outdated README statement that client does not decode chunked.
   - Update `tina-http/src/lib.rs` module docs to reflect chunked request acceptance.

## Hard Rules

- No HTTP/2, gRPC, trailers, compression, redirects, cookies, or framework surface.
- No hidden unbounded Vec.
- No "unknown length means 0".
- No clean EOF for malformed chunked wire.
- If chunk extensions are accepted, parse and ignore them deliberately; otherwise reject and test.
- Reject Content-Length plus Transfer-Encoding unless you explicitly change the plan and prove the rule.
- Chunked requests require `HttpLimits::inbound_stream_chunk_size = Some(n)`; if disabled, reject loudly.

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina-http --tests`
- `cargo clippy -p tina-http --tests -- -D warnings`
- If docs changed: `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps`

## Status

- [x] Shared incremental chunked decoder
- [x] Client decodes chunked responses
- [x] Server accepts chunked requests via streaming pull
- [x] HTTPS parity proof
- [x] Body metrics honest under chunked paths
- [x] Specimen and docs updated
- [x] `cargo fmt` clean
- [x] `cargo test -p tina-http --tests` clean
- [x] `cargo clippy -p tina-http --tests -- -D warnings` clean
- [x] `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps` clean
- [x] Hostile review complete
