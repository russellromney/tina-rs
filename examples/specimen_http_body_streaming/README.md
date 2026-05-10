# specimen_http_body_streaming

A 256 KiB response body served chunk-by-chunk to a slow reader. The
shared `slow_reader_client` reads in 1 KiB slices with a 2 ms pause
between each read, which makes the kernel's send buffer back up
against the server. Both sides have to deal with that pressure;
they deal with it very differently.

- Tokio: `axum::body::Body::from(big_vec)`. The whole 256 KiB lives
  in `Vec<u8>` before the response starts.
- Tina: `HttpResponse::with_stream(...)` + a `BigBody` chunk-source
  isolate. Each pull is one `CHUNK_BYTES` chunk; nothing else is
  resident in the connection at the time of writing.

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
side=tina  bytes_received=262144 status_ok=true wall_clock_ms=~600
           exit_clean=true tokio_response_alloc_floor=n/a
           tina_response_high_water=4096
```

Wall-clock is similar (the slow reader paces both). The body
footprint numbers tell the real story:

- Tokio reports `tokio_response_alloc_floor=262144`. This is a
  *lower bound*: we know the `Vec<u8>` we hand to `Body::from(...)`
  is 256 KiB and lives until the response finishes streaming.
  Hyper queues more bytes for its writer task — we don't see
  those.
- Tina reports `tina_response_high_water=4096`. This is the
  *exact peak* observed via `BodyMetrics`: at no point did the
  connection isolate hold more than one chunk's worth of body.
  64× smaller than the body itself.

Both numbers are honest. Tokio's is "at least this much"
because that body model has no per-chunk hook to measure
through. Tina's is "exactly this much" because every chunk goes
through a charge/release pair.

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
counter. The whole response is the unit of work.

## Tina shape

A `BigBody` isolate produces one `CHUNK_BYTES` chunk per
`ResponseChunkMsg::Next` call, then `Eof`. The connection isolate
pulls chunks via `call(source, Next, t).reply(StreamChunk)`,
writes each chunk via `tcp_write`, and only pulls the next chunk
*after* the previous chunk has fully drained. The high-water at
peak is therefore one chunk's worth — not the whole body.

`BodyMetrics::new()` is shared with the listener. The connection
charges bytes when a chunk is queued for `tcp_write` and releases
them when the runtime accepts the bytes. After shutdown,
`metrics.snapshot()` reports the high-water observed across the
whole run.

## Discussion

What feels better on Tokio:

- **The shape is shorter.** `Body::from(vec)` is one line. axum
  signs you up for a working response with no further thought.
- **For small bodies you're done.** The whole-body-resident model
  is a non-issue for a 1 KB response.

What feels better on Tina:

- **Every chunk has a name.** `Next` → `Chunk(bytes)` → write →
  `Wrote(count)` → next `Next`. Each step is a runtime trace event;
  if a chunk source stalls, the trace tells you exactly which
  `Next` is outstanding.
- **The body buffer is bounded by your chunk size.** The connection
  isolate holds at most one chunk in `pending_response`. The
  high-water counter proves this on every run; a regression that
  silently buffers the whole body would push that counter up.
- **Slow client backpressure is real backpressure.** Until the
  current chunk's `tcp_write` finishes, the next `Next` does not
  fire. The chunk source naturally waits — no extra signalling.
- **Failure shape is visible.** If the source returns
  `CallOutcome::Timeout`, the wire is torn down and
  `body_timeout_count` increments. If `tcp_write` returns
  `Wrote(Err(_))`, `body_io_error_count` increments. You can
  assert these in tests.

What this suggests:

- Tokio's body model is shorter for the common case; Tina's body
  model is shorter for the case where you actually need to know how
  big the in-flight body is. Different audiences, both honest.
- "Stream the body" on Tokio means "produce a `futures::Stream`."
  On Tina it means "register a chunk-source isolate and answer
  `Next`." Both are first-class; the names of the steps differ.
- A specimen that wanted to compare *upload* (slow client uploads
  to the server, server reads chunk by chunk and hashes) would
  show the same shape from the other side: streaming-request via
  `HttpLimits::inbound_stream_chunk_size`. That deferral is on
  purpose — one specimen, one direction.

## What this does *not* prove

- Tokio's shape can stream too — `Body::from_stream(...)`,
  `axum::response::sse::Sse`, etc. Pick the right axum API and the
  in-flight footprint shrinks. The point of this specimen is the
  *default* shape of each library, not the upper bound.
- Wall-clock numbers are noisy on a single machine. The
  `tina_response_high_water` line is the property worth pinning,
  not the milliseconds.
