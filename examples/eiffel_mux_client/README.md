# eiffel_mux_client

A *multiplexed client*: one TCP connection, three concurrent
in-flight requests, replies arrive out of order. The responder is
shared between sides and intentionally delays high-id replies less
than low-id replies — `id=3 → 10ms`, `id=2 → 20ms`, `id=1 → 30ms`
— so `arrival_order=[3, 2, 1]` is the proof that real multiplexing
happened.

## Run

```sh
cargo run --manifest-path examples/eiffel_mux_client/Cargo.toml -- both
cargo run --manifest-path examples/eiffel_mux_client/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_mux_client/Cargo.toml -- tina
```

You'll see the same arrival order on both sides:

```
comparison=eiffel_mux_client side=tokio arrival_order=[3, 2, 1]
comparison=eiffel_mux_client side=tina  arrival_order=[3, 2, 1]
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)
- [`src/lib.rs`](src/lib.rs) — the shared Tokio responder both sides
  connect to (it's the test target, not a client driver).

Each side is self-contained: server connection, client logic,
arrival classification.

## Tokio shape

The classic shared-state shape:

- One reader task draining `read_line` in a loop.
- One writer half producing `REQ <id>\n` per request.
- An `Arc<Mutex<HashMap<u32, oneshot::Sender<()>>>>` linking them so
  the reader can resolve the right oneshot for each parsed id.
- A `Vec<oneshot::Receiver<()>>` to await all of them.

Reader and writer touch the shared map; correctness depends on the
caller setting up the oneshot *before* the write goes out (otherwise
a fast reply could arrive before the entry exists).

## Tina shape

One isolate (`MuxClient`) owns everything:

- `tcp_connect` → `tcp_write` (whole REQ batch in one buffer) →
  `tcp_read` loop until enough RESP lines have been parsed →
  `tcp_close_stream` → `stop()`.
- The parser, the read buffer, and the arrival counter all live
  behind the same mailbox. There is no shared map. The runtime
  delivers `tcp_read` replies as bytes land, and the handler walks
  complete lines.
- Out-of-order arrival just works: the responder writes the lines as
  they're ready, the kernel and Tina runtime deliver the bytes in
  arrival order, the handler parses them in arrival order.

## Discussion

What feels better:

- **No shared map.** The Tokio version's `Arc<Mutex<HashMap<id,
  oneshot>>>` is real coupling between two tasks; the Tina version
  doesn't have a story for "multiple tasks racing to mutate state"
  because there's only one mailbox. The data is owned, period.
- **No oneshot-per-request bookkeeping.** Tokio needs N oneshots and
  N `Receiver::await` calls. Tina just counts arrivals against an
  expected total in handler state.
- **Out-of-order arrival doesn't need ceremony.** Tina sees bytes,
  parses lines, increments a counter. There is nothing to "match
  up" because the client never blocked on a specific id.

What feels worse:

- **The arrival log is still a side channel.** The Tina side passes
  an `Arc<Mutex<Vec<u32>>>` into the isolate so the host can read
  the result after `observe_isolate_complete`. App-specific data the
  runtime can't know about — `FINDINGS.md` tracks this as typed
  isolate result waiter work (047 retired the *runtime-knowable*
  side channels like bound-address, but not app-data).
- **The line-parsing loop is hand-rolled.** `position(b == b'\n')`
  + `drain(..=idx)` is fine, but every framed-line client will write
  it. A `tcp_read_lines(stream).reply(...)` shape would be welcome.
- **Two runtimes for the Tina side.** The responder lives in a
  Tokio runtime on a side thread; the client lives in a Tina
  threaded runtime; they exchange the address via `std::sync::mpsc`.
  The plumbing is small but it's there because Tina doesn't have a
  native HTTP/RPC server *here* (this example is about the client
  only).

What this suggests:

- Tina-as-a-client is genuinely usable for multiplexed protocols.
  The pattern that requires `Arc<Mutex<HashMap<...>>>` in Tokio just
  doesn't appear.
- The remaining ergonomics frontier for clients is line/frame
  parsing — a small `tcp_read_lines` or framed-codec helper would
  shrink every connection-handling isolate.
- The "host needs the isolate's app-data after it stops" pattern is
  going to recur (mux arrival order, fetch results, etc.). A typed
  observation handle that resolves to the isolate's final state
  would close the last side-channel category.
