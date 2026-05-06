# Eiffel Outbound Fetch

Paired Tokio-vs-Tina implementation of the *client* side of TCP. Up
until now every Eiffel comparison has put Tina on the server side
(chat fanout, keyspace, axum counter, supervised worker, persistent
counter, replay DST). This one swaps the role: a tiny test server
spins up, and both sides act as a client that connects, writes a
small request, reads the response, and closes.

The script is fixed:

```text
- spin up a loopback test server that accepts FETCH_COUNT (=4) connections
- each connection: read the client request, write "OK\n", close
- client side: open FETCH_COUNT connections to that addr, sequentially,
  collect every "OK\n"
```

Both sides emit the same numbers and the run is asserted in
`assert_equivalent`:

```text
successful=4 failed=0 bytes=12 exit_clean=true
```

Run both sides:

```bash
cargo run --manifest-path examples/eiffel_outbound_fetch/Cargo.toml -- compare
```

Run one side:

```bash
cargo run --manifest-path examples/eiffel_outbound_fetch/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_outbound_fetch/Cargo.toml -- tina
```

## What this comparison taught us

### Tokio side

- Trivial. Three lines per fetch:
  ```rust
  let mut stream = TcpStream::connect(addr).await?;
  stream.write_all(b"GET\n").await?;
  stream.read_to_end(&mut buf).await?;
  ```
  This is the shape Tokio is most graceful at and Tina is most
  verbose at. The async/await preserves the linear flow of the
  client protocol exactly the way you'd want to read it.
- `read_to_end` is the move people learn last but want first. It
  papers over the EOF/length-prefix question entirely; the server
  closes the stream when it's done, the read returns, the client
  parses what it got.
- Errors fall out as `?`. On the happy path the operator visually
  isn't visible; on the unhappy path `match` works fine. The
  comparison reports `failed_fetches` from a single `match` arm.

### Tina side

What worked well:

- The `Fetcher` isolate is genuinely a TCP client written as a state
  machine: `Begin -> Connected -> Wrote -> Read* -> Closed -> Begin`.
  Every step of the protocol is a match arm; the handler is one
  function. Once the shape is in your head, it reads honestly: this
  is what TCP looks like when no part of it is hidden.
- `tcp_connect(addr).reply(FetchMsg::Connected)` is the same shape as
  `tcp_bind` and `tcp_accept` from the server-side comparisons.
  Naming is consistent; the reply continuation pattern transfers
  directly. *Tina-as-client and Tina-as-server are the same Tina.*
- `Connected(Ok((stream, _local, _peer)))` gives both endpoints back
  to user code. `tokio::net::TcpStream::connect` returns just the
  stream; you have to call `.local_addr()` separately. Small win,
  but a real one for code that wants to log or correlate.
- The "iterate" pattern (next_iteration after each close) folds back
  into `tcp_connect` cleanly. The state machine doesn't grow when
  iterating; a counter-decrement and one re-connect effect are all
  it takes.

What was awkward or surprising:

- `tcp_read` returning `Vec<u8>` of zero bytes for EOF is the right
  signal but it has to be hand-detected. There is no equivalent of
  `read_to_end` at the runtime-call layer — every example has to
  loop on `tcp_read` until it sees an empty payload, and
  `response_buf.extend_from_slice(&bytes)` is hand-rolled
  buffering. This is a genuine ergonomic tax for small client
  protocols.
- Write loop. `tcp_write` returns `Ok(count)` and may write less than
  the buffer; the example handles partial writes with
  `pending_write.drain(..count)` plus a self-loop into `Wrote`. The
  Tokio side has `write_all`. Tina does not, at the runtime-call
  surface. Probably correct — `write_all` hides retries — but the
  pattern is going to be in every Tina TCP client.
- ~~Same `Mailbox` + `MailboxFactory` boilerplate as the other four
  comparisons.~~ **Resolved in phase 047:** the example uses
  `DefaultThreadedMailboxFactory`.
- `FetchMsg` ballooned: `Begin`, `Connected`, `Wrote`, `Read`,
  `Closed`. Plus their `Ok`/`Err` arms. This is the
  "Continuation Enum Growth" sharp edge from the user guide,
  applied to a five-step protocol.
- ~~The driver thread waits on a shared `AtomicBool::done` flag.~~
  **Resolved in phase 047:** the driver registers
  `runtime.observe_isolate_complete(fetcher)` before kicking the isolate
  and waits on the typed waiter instead.

### Tokio shape vs. Tina shape, in one paragraph

Tokio wins on raw line count and reads-like-the-protocol clarity for
client code, full stop. Tina makes you write the state machine
explicitly — `Begin -> Connected -> Wrote -> Read* -> Closed` — and
the resulting code is a third again as long. The trade is what the
state machine *gives you*: every step is a separately observable
event in the runtime trace, every `Err` branch is forced to be an
arm rather than a `?` you might forget to grep for, and the same
isolate runs unchanged under `tina-sim` (see
`eiffel_replay_dst`). For a quick HTTP client, Tokio is what you
want. For a long-lived protocol client where every retry, timeout,
and backoff has to be inspectable, Tina is closer to what you'd
end up writing on top of Tokio anyway, just made explicit.
