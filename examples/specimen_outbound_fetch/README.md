# specimen_outbound_fetch

Tina as a *TCP client*: connect to a real loopback server N times,
send a one-line request, drain the response, classify outcomes.
Tokio writes the same shape with `TcpStream::connect + write_all +
read_to_end` in a `for` loop. Tina writes a `Fetcher` isolate that
walks `tcp_connect → tcp_write (loop on partial writes) → tcp_read
(loop until EOF) → tcp_close_stream` per iteration.

## Run

```sh
cargo run --manifest-path examples/specimen_outbound_fetch/Cargo.toml -- both
cargo run --manifest-path examples/specimen_outbound_fetch/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_outbound_fetch/Cargo.toml -- tina
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

A `Fetcher` isolate with a state machine. Typed results and TCP loop helpers own
the loops:

- `Begin → tcp_connect.then(Connected)`.
- `Connected(Ok(...)) → TcpWriteAll(GET).next_effect(Wrote)`. The
  helper retries the next chunk on partial writes; the handler arm
  is "advance and dispatch".
- `Wrote(Done) → TcpReadToEof.next_effect(Read)`. The helper accumulates
  bytes until EOF or a `RESPONSE_MAX` cap.
- `Read(Done(buffer)) → classify → tcp_close_stream.then(Closed)`.
- `Closed → next iteration or stop_with(self.outcome)`.

The host registers a typed result waiter and waits on it:

```rust
let result = runtime.observe_result::<FetchOutcome, _, _>(fetcher)?;
runtime.try_send(fetcher, FetchMsg::Begin)?;
let outcome = result.wait(Duration::from_secs(5))?;
```

The isolate owns its `FetchOutcome` (plain `u32`/`usize` fields) and
publishes the final value via `stop_with(self.outcome.clone())`: no
`Arc`, no atomics, no side channel.

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

What the TCP loop helpers already closed:

- **No hand-rolled partial-write or read-until-EOF loops.** The
  `TcpWriteAll`, `TcpReadExact`, and `TcpReadToEof` helpers in
  `tina_runtime::tcp_loops` own the loops as small client-side
  state machines. Each underlying `tcp_write` / `tcp_read` is still
  one trace event, so partial progress is still visible.
What this suggests:

- TCP-loop helpers are the copied path now. They should stay
  step-shaped (`next_effect` / `advance`) so they shrink real code
  without hiding per-step trace events.

What Tina already closes here:

- The shared `Arc<Outcome>` with three atomics is gone. The isolate
  owns plain fields and publishes the final value with `stop_with`;
  the host receives it through `observe_result::<FetchOutcome>(addr)`.
