# specimen_rpc

Same framed request burst, two implementations:

- One client connection sends a burst of N requests in one TCP write.
- The server has bounded concurrency: only one request can be
  "in flight" at a time.
- Read both sides; see how each one expresses that bound.

## Run

```sh
cargo run --manifest-path examples/specimen_rpc/Cargo.toml -- both
cargo run --manifest-path examples/specimen_rpc/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_rpc/Cargo.toml -- tina
cargo run --manifest-path examples/specimen_rpc/Cargo.toml -- both 8
```

You'll see one line per side:

```
comparison=specimen_rpc side=tokio burst=4 ok=4 full=0 other=0
comparison=specimen_rpc side=tina  burst=4 ok=1 full=3 other=0
```

`ok` is `Reply` frames received. `full` is server-reported wire
`Error(Full)` frames. `other` covers anything unexpected so totals
never silently shrink.

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

Both files are self-contained — server, client, classification.

## Tokio shape

Tokio looks like a normal async server: `TcpListener::bind`, an
`mpsc::unbounded_channel` between the request-reading task and a
single worker, a `tokio::spawn` for each side. The "bounded
concurrency" is implicit — there's one worker draining one channel.
The channel is unbounded.

When you run it, `full=0` always. Every request is accepted, queued,
eventually replied to. Overload doesn't show up on the wire because
the queue absorbs it. If the producer outruns the worker, the queue
just grows.

## Tina shape

Tina uses the `#[tina_rpc::service]` macro to define the service:

```rust
#[service]
trait Echo {
    fn ping(&mut self, payload: Vec<u8>) -> Vec<u8>;
}

impl Echo for EchoState {
    fn ping(&mut self, payload: Vec<u8>) -> Vec<u8> {
        payload
    }
}
```

That's the whole service definition. The macro emits the dispatcher
and per-method JSON encode/decode. The handler is the user's `impl`.

The bound — "one in flight per connection" — lives on
`Connection::tiny_pressure()`, a config preset on the connection
isolate that wraps the TCP stream. Over-cap requests come back as
wire `Error(Full)` frames immediately. The client sees them on the
read side of the same socket.

When you run it, `full=N-1` always. The first request grabs the
slot, gets a `Reply`. The next N-1 come back as `Error(Full)`. The
overload is on the wire.

## Discussion

What feels better:

- **Tina makes overload visible.** "Server is at capacity" is a
  frame the client can read. The Tokio version requires
  out-of-band knowledge — looking at queue length, latency
  histograms, cgroup memory — to detect the same condition.
- **The `#[service]` macro removes byte plumbing.** Server-side
  dispatch, args/return JSON encoding, error mapping — all
  generated. Adding a second method is one `fn` line.
- **The Tina connection's bound is a number, not an architecture
  choice.** `tiny_pressure()` sets `max_in_flight = 1` for the
  demo; production presets are higher but still finite. There's
  no "and then we added a queue and a worker" story — the bound
  is the bound.

What feels worse:

- **Tina's setup has more pieces.** A Listener isolate that walks
  `Bound → Accepted → spawn(Connection)`, a Registry that maps
  wire service name to dispatch isolate, the `SingleService`
  adapter wrapping `Dispatch`. Each piece is small (047 retired
  the mailbox-factory and per-shard-type boilerplate, and
  `runtime.observe_next_bound()` retired the
  `Arc<Mutex<Option<SocketAddr>>>` side channel) but there are
  more of them. The Tokio side is `bind / accept / spawn`.
- **The wire shape is JSON-tuple-encoded.** The macro decodes
  `fn ping(payload: Vec<u8>)` from a JSON `[<bytes>]`. Clients
  must produce that shape. Positional tuples are not additive —
  adding an arg changes the JSON array length and silently
  breaks old clients.
- **The Tokio side reads cleanly top-to-bottom.** The Tina side
  has more pieces and the wiring between them is the cost of the
  runtime model.

What this suggests:

- The bounded-in-flight + wire-error pattern is the right
  default for backpressure-sensitive RPC. Tokio's unbounded
  queue is a footgun that production code papers over with
  observability — Tina makes the condition addressable.
- The `#[service]` macro is the right place for the typed
  surface. The boilerplate it removes is exactly the part that
  rots in hand-written services (method match, decode/encode,
  error mapping).
- The remaining setup cost is the Listener / Registry / Service
  separation. That's load-bearing — each piece has a real job —
  but it's still where future ergonomics work should focus.
