# specimen_real_io_chat

Same workload, two implementations, real loopback TCP:

- One client connects, writes a burst size N, shuts down its write side.
- The server tries to fan out N messages into a slow consumer with
  capacity 1.
- The server writes back what it observed, then closes.
- Read both sides; see how each one expresses "the consumer can't keep up."

## Run

```sh
cargo run --manifest-path examples/specimen_real_io_chat/Cargo.toml -- both
cargo run --manifest-path examples/specimen_real_io_chat/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_real_io_chat/Cargo.toml -- tina
cargo run --manifest-path examples/specimen_real_io_chat/Cargo.toml -- both 256
```

You'll see one line per side:

```
comparison=specimen_real_io_chat side=tokio burst=64 accepted=64 full=0 closed=0 delivered=1 buffered=63
comparison=specimen_real_io_chat side=tina  burst=64 accepted=1  full=63 closed=0 delivered=1 buffered=0
```

`accepted + full + closed == burst` always — every fanout attempt is
accounted for. `delivered` is what reaches the slow consumer; everything
else is implicit somewhere.

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

Both files are self-contained — server, client, classification.

## Tokio shape

Tokio looks like a normal async server: `TcpListener::bind`, an
`mpsc::unbounded_channel` between the producer loop and the consumer,
a single `rx.recv().await` to pull one message. The "slow consumer"
is just "we only call `recv` once."

When you run it, `accepted=N` always. Every send into the channel
succeeds. Only `delivered=1` reaches the consumer — the rest sit in
the channel's buffer (`buffered=N-1`). There is no wire-side or
protocol-side signal that the consumer is overloaded; the producer
has to know out of band that the channel is unbounded.

## Tina shape

Tina uses one isolate per role:

- **`SlowClient`** — the bounded slow consumer (mailbox capacity 1).
- **`Connection`** — owns the accepted TCP stream; reads the burst,
  fans out via `send_observed(...)`, classifies each admission outcome
  (`Accepted` / `Full` / `Closed`), writes the count back.
- **`Listener`** — `tcp_bind` → `tcp_accept` → `spawn(Connection)`.

`send_observed` is the load-bearing primitive: it tells the producer
*at the moment of send* whether the target took the message or
rejected it as `Full`. The Connection isolate counts these and only
writes its response after every fanout attempt is observed.

When you run it, `accepted=1` (the slow consumer's capacity) and
`full=N-1` (every over-cap admission visible). Nothing buffered.

## Discussion

What feels better:

- **Tina makes the slow consumer visible to the producer.** The
  Connection isolate *counts* `Full` outcomes; the Tokio version has
  no equivalent because `tx.send` returns `Ok(())` regardless of
  drain rate.
- **The bound is a number, not a discipline.** `SlowClient`'s
  mailbox capacity is set at registration and the runtime enforces
  it. The Tokio shape relies on the operator to never use
  `unbounded_channel` if backpressure matters — and most code does
  use unbounded.

What feels worse:

- **Tina's setup has more pieces.** Three isolates (SlowClient,
  Connection, Listener) plus the `send_observed(...).then(...)`
  fanout pattern. Tokio is one async block with a `for` loop and a
  channel. Each Tina piece is small (047 retired the mailbox-factory
  and per-shard-type boilerplate, and `runtime.observe_next_bound()`
  retired the `Arc<Mutex<Option<SocketAddr>>>` side channel) but
  there are more of them.
- **The `send_observed` ceremony.** Fanout that wants admission
  outcomes is `batch((0..N).map(|i| send_observed(...).then(...)).collect())`
  plus a per-message `Observed(SendOutcome)` arm in the Connection.
  Clear, but verbose for a "broadcast to subscribers" shape.
- **The connection mailbox sizing rule.** Each `send_observed` reply
  lands in the Connection's mailbox, so the Connection's capacity
  must be `burst + slack`. This is documented (047, see
  `docs/mailbox-capacity.md`) but it's still a number you have to
  size correctly per workload.

What this suggests:

- Visible-overload backpressure is the right default for fanout
  workloads. The Tokio shape is a footgun unless every operator
  knows the channel is unbounded; Tina makes the condition
  observable at the producer.
- The `send_observed` + per-message `Observed` arm is the clean
  shape today, but a "broadcast with bounded admission and one
  aggregated outcome" combinator would fit chat-room and pubsub
  patterns better.
- The remaining setup cost — three isolates plus reply-slot mailbox
  sizing — is where future ergonomics work for fanout services
  should focus.
