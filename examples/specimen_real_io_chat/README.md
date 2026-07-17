# specimen_real_io_chat

Same workload, two implementations, real loopback TCP:

- One client connects, writes a burst size N, shuts down its write side.
- The server tries to fan out N messages into a slow consumer with
  capacity 1.
- The Tina side only admits up to its service-owned broadcast cap into
  runtime effects; extra requested targets become visible `Full`.
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

`accepted + full + closed == burst` always — every requested fanout
attempt is accounted for. `delivered` is what reaches the slow consumer;
everything else is implicit somewhere.

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
  caps fanout with `BroadcastTargets`, fans out via
  `broadcast_observed(...)`, classifies each admission outcome
  (`Accepted` / `Full` / `Closed`), retries partial writes, and stops
  with an exact typed protocol, broadcast construction/tracking,
  read/write, or close terminal. Its only outbound is still the
  broadcast target channel.
- **`Listener`** — `tcp_bind` → `tcp_accept` →
  `spawn_observed(Connection).then_result(...).then(...)` → close.
  The runtime delivers the connection's `stop_with` payload as a
  parent event; the listener folds it into `ListenerTerminal::ClosedClean
  { connection }` for the host.

`RunConfig::validate` rejects zero and over-cap burst, target, and mailbox
values before `LocalSystem` starts. `run_to_shutdown_reported` preserves
workload and bounded shutdown failures separately. The host claims
`observe_result::<ListenerTerminal>` before start and requires both a clean
listener close and `ConnectionTerminal::ClosedClean`.

The decimal request is accumulated across TCP reads until the client's
write-half EOF, under a 32-byte protocol cap, before parsing. TCP packet
boundaries are not treated as message boundaries, and an overlong request
stops with a typed protocol terminal before fanout effects exist.

`broadcast_observed` is the copied path for this shape. It builds on
`send_observed`, so the producer still learns *at the moment of send*
whether the target took the message or rejected it as `Full`. The
Connection isolate keeps a `BroadcastTracker` and only writes its
response after every admitted target is observed. Targets over the
service cap are counted as `Full` before they become effects.

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
  Connection, Listener) plus the `BroadcastTargets` / `BroadcastTracker`
  state. Tokio is one async block with a `for` loop and a channel.
  Each Tina piece is small, but there are more of them.
- **The broadcast helper still exposes the message turn.**
  `broadcast_observed(...)` removes the hand-written batch, but each
  target still replies through an ordinary
  `Observed(target, SendOutcome)` message. That is more code than a
  callback, and it is also the traceable Tina truth.
- **The connection mailbox sizing rule.** Each `send_observed` reply
  lands in the Connection's mailbox, so the Connection's capacity must
  be `max_broadcast_targets + slack`. This is documented (see
  `docs/mailbox-capacity.md`) but it's still a number you have to size
  correctly per workload.
- **The listener must wait for both facts.** Listener close and connection
  terminal can arrive in either order. The listener holds state until both
  are known, then stops with one combined terminal. That is still more
  bookkeeping than "fire and forget," but it is application state rather
  than a second outbound or host sidecar.

What this suggests:

- Visible-overload backpressure is the right default for fanout
  workloads. The Tokio shape is a footgun unless every operator
  knows the channel is unbounded; Tina makes the condition
  observable at the producer.
- `BroadcastTargets` makes the service-owned bound explicit before
  runtime effects exist. That is the important difference from
  `for item in request { spawn/send }`.
- Typed child terminal observation lets a multi-outbound child keep its
  application send channel while the runtime still routes `stop_with`
  to the parent. That closes the chat's host-observation gap without a
  generic multi-outbound abstraction.
