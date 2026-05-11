# specimen_http_body_streaming

A 256 KiB response body served chunk-by-chunk to a slow reader.
The shared `slow_reader_client` reads in 1 KiB slices with a 2 ms
pause between each read, which makes the kernel's send buffer back
up against the server. Both sides have to deal with that pressure;
they deal with it differently.

- Tokio: `axum::body::Body::from(big_vec)`. The whole 256 KiB is
  resident in `Vec<u8>` before the response starts.
- Tina: `HttpResponse::stream_known_length(...)` over an
  `IterBodySource` that wraps a closure-iterator. Each chunk is
  pulled only after the previous one drains; nothing else sits in
  the connection.

The Tina service also routes `/big-chunked` to
`HttpResponse::stream_chunked(...)` for an unknown-length body
framed by `Transfer-Encoding: chunked`. Same source contract,
different framing — the API forces the choice.

## Run

```sh
cargo run --manifest-path examples/specimen_http_body_streaming/Cargo.toml -- both
cargo run --manifest-path examples/specimen_http_body_streaming/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_http_body_streaming/Cargo.toml -- tina
```

Both sides:

```
side=tokio bytes_received=262144 status_ok=true wall_clock_ms=~600
           exit_clean=true tokio_response_alloc_floor=262144
           tina_response_high_water=n/a
           tina_chunked_wire_bytes=n/a tina_chunked_decoded_bytes=n/a
side=tina  bytes_received=262144 status_ok=true wall_clock_ms=~600
           exit_clean=true tokio_response_alloc_floor=n/a
           tina_response_high_water=4096
           tina_chunked_wire_bytes=262661 tina_chunked_decoded_bytes=262144
```

The Tina line reports two chunked numbers: `tina_chunked_wire_bytes`
is the raw `Transfer-Encoding: chunked` body (data + framing
overhead) and `tina_chunked_decoded_bytes` is the decoded payload.
The decoded length matches `RESPONSE_BODY_BYTES`; the wire is
larger by the per-chunk size header overhead. The smoke test
asserts both.

Wall-clock is similar (the slow reader paces both). The body
footprint numbers tell the real story:

- Tokio reports `tokio_response_alloc_floor=262144`. This is a
  *lower bound*: we know the `Vec<u8>` we hand to `Body::from(...)`
  is 256 KiB and lives until the response finishes streaming.
  Hyper queues more bytes for its writer task — we do not see
  those.
- Tina reports `tina_response_high_water=4096`. This is the
  *exact peak* observed via `BodyMetrics`: at no point did the
  connection isolate hold more than one chunk's worth of body.
  64× smaller than the body itself.

Both numbers are honest. Tokio's is "at least this much" because
the body model has no per-chunk hook to measure through. Tina's
is "exactly this much" because every chunk goes through a
charge/release pair.

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)
- [`src/lib.rs`](src/lib.rs) — the shared slow-reader client.

## Tokio shape

`axum::Router` with `get(big)`. `big()` builds a `Vec<u8>` of the
whole body and wraps it in `Body::from(...)`. From there
`hyper`/`tokio` own the wire pacing — the body Vec is held alive
inside the response future until the last byte goes on the wire.
There is no observable per-chunk pull point and no body-pressure
counter.

## Tina shape

A closure-iterator yields one `CHUNK_BYTES` chunk per call.
[`IterBodySource::new(iter)`](../../tina-http/src/streaming.rs)
wraps the iterator into an `Isolate` that answers
`ResponseChunkMsg::Next` with the next yielded value, or `Eof`
when the iterator drains. No custom `Isolate` impl needed for the
common case.

The service handler returns one of:

```rust
HttpResponse::stream_known_length(StatusCode::OK, n, source)  // Content-Length
HttpResponse::stream_chunked(StatusCode::OK, source)          // Transfer-Encoding: chunked
```

The choice is loud. There is no "guess a length" path; if you
don't know the length, you say so.

The connection isolate pulls chunks via `call(source, Next, t)`
and writes each chunk only after the previous chunk has fully
drained. `BodyMetrics` charges body bytes when the chunk is
queued and releases them when the runtime accepts the bytes — so
`response_body_high_water` is the peak resident at any moment.

## Discussion

What got shorter:

- The chunk source went from a hand-rolled `BigBody` `Isolate` impl
  with `tina::isolate_types!` and `ResponseChunkMsg`/`ResponseChunkReply`
  arms down to a plain closure-iterator wrapped in
  `IterBodySource::new`.
- The framing choice moved from "build a `ResponseStream` literal"
  into a typed constructor (`stream_known_length` vs
  `stream_chunked`). Caller sees both options at the call site.

What stayed explicit:

- Every chunk is a runtime call: `Next` -> `Chunk(bytes)` -> wire
  write -> `Wrote(count)` -> next `Next`. Each step shows up in
  the trace. No hidden buffering task.
- The connection isolate registration is still by hand. The
  service isolate is still by hand. Tina does not pretend HTTP
  body streaming is "just `async fn`".
- Pressure is still visible. `BodyMetrics` is shared between
  listener and connection by `with_metrics(metrics.clone())`;
  `snapshot().response_body_high_water` reads at any time.
- Failure shape is still typed: source `Timeout` increments
  `body_timeout_count`; source `Closed`/`Full` and wire write
  failures increment `body_io_error_count`; source under-produce
  for known-length is also an IO error. Tests assert each.

How known vs chunked works:

- Known length: emit `Content-Length: N`, write raw bytes, close
  after exactly N bytes drained. Source under-produce or peer
  close shows up as `body_io_error_count`. Source over-produce
  is truncated to N — the wire stays honest.
- Chunked: emit `Transfer-Encoding: chunked`, frame each `Chunk`
  reply as `<size in hex>\r\n<bytes>\r\n`, write `0\r\n\r\n` on
  source `Eof`. The connection writes the terminator on its own;
  the source just signals end-of-stream.

What this suggests:

- The "hand-roll an Isolate just to yield bytes" gap is gone for
  iterator-style sources. A source that really does need
  `Isolate` (file reads, accumulating state) still implements it
  by hand, and `IterBodySource` doesn't get in the way.
- The framing API is now loud enough that "guess a Content-Length"
  is a type error. Tokio's `Body::from(...)` collapses everything
  through the same constructor; Tina makes the framing a visible
  call-site choice.

## What this does *not* prove

- Tokio's shape can stream too — `Body::from_stream(...)`,
  `axum::response::sse::Sse`, etc. Pick the right axum API and the
  in-flight footprint shrinks. The point of this specimen is the
  *default* shape of each library, not the upper bound.
- Wall-clock numbers are noisy on a single machine. The
  `tina_response_high_water` line is the property worth pinning,
  not the milliseconds.
- Chunked responses are decoded by both the HTTP and HTTPS
  client paths. The specimen exercises the server-emitting path;
  integration tests in `tina-http/tests/client_chunked_response.rs`
  and `tina-http/tests/body_parity_tls.rs` cover the client decode
  side. Chunked *request* bodies are accepted via the same
  streaming pull model when `inbound_stream_chunk_size` is set.
