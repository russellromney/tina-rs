# eiffel_outbound_fetch

Tina as a *TCP client*: connect to a real loopback server N times,
send a one-line request, drain the response, classify outcomes.
Tokio writes the same shape with `TcpStream::connect + write_all +
read_to_end` in a `for` loop. Tina writes a `Fetcher` isolate that
walks `tcp_connect → tcp_write (loop on partial writes) → tcp_read
(loop until EOF) → tcp_close_stream` per iteration.

## Run

```sh
cargo run --manifest-path examples/eiffel_outbound_fetch/Cargo.toml -- both
cargo run --manifest-path examples/eiffel_outbound_fetch/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_outbound_fetch/Cargo.toml -- tina
```

Both sides report:

```
side=tokio successful=4 failed=0 bytes=12 exit_clean=true
side=tina  successful=4 failed=0 bytes=12 exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)
- [`src/lib.rs`](src/lib.rs) — the shared `TestServer` (just the
  test target; doesn't constrain client implementation).

## Tokio shape

A `for _ in 0..FETCH_COUNT { fetch_one().await }` loop. `fetch_one`
is `TcpStream::connect → write_all → read_to_end`. Anything that
returns `Err` collapses into a `failed += 1`.

## Tina shape

A `Fetcher` isolate with a state machine:

- `Begin → tcp_connect.reply(Connected)`.
- `Connected(Ok(...)) → tcp_write(GET).reply(Wrote)`. Partial writes
  re-issue `tcp_write` on the remaining bytes.
- `Wrote(Ok(_)) → tcp_read.reply(Read)`. Empty `Read` is EOF, which
  classifies the gathered buffer.
- Either way: `tcp_close_stream.reply(Closed) → next iteration or
  stop()`.

The host watches for completion with
`runtime.observe_isolate_complete(fetcher).wait(...)`. Per-fetch
counts land in a shared `Outcome` slot (atomics).

## Discussion

What feels better:

- **Partial writes are visible.** Tina's `tcp_write` returns the
  number of bytes written; the handler decides whether to issue
  another `tcp_write` for the remainder. Tokio's `write_all` papers
  over the loop but you can't observe it. (See `docs/tcp-loops.md`
  for the canonical pattern.)
- **Per-step trace events.** Every connect, write, read, and close
  is a typed runtime event. `complete_trace()` is a real audit log
  for what happened on the wire.

What feels worse:

- **The `Wrote(Ok(count))` partial-write loop is hand-rolled.**
  Every TCP client will write it. A `tcp_write_all` driver-level
  helper is deliberately deferred (047 finding) so that partial-
  write progress remains observable in the trace; the user-side
  loop is the documented pattern for now.
- **The `Read(Ok(bytes))` read-until-EOF loop is hand-rolled.**
  Same shape, same finding. A `tcp_read_to_eof` would shrink every
  outbound client.
- **The `Outcome` side channel.** Per-fetch counts go through atomics
  on a shared `Arc<Outcome>` because the host needs to read them
  after the isolate stops. App-specific data the runtime can't know
  about — same pattern as `eiffel_mux_client::ArrivalLog`.

What this suggests:

- TCP-loop helpers (`tcp_write_all`, `tcp_read_to_eof`) are the
  next ergonomics step for client isolates. The trade-off is real
  (helpers hide per-step trace events), but a documented helper
  with the trade-off named would shrink real code.
- The "isolate's app-data after it stops" pattern is the same one
  showing up in mux_client and persistent_counter. A typed
  observation handle that resolves to the isolate's final state
  would close the loop.
