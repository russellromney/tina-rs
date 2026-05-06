# Eiffel Mux Client

Paired Tokio-vs-Tina implementation of a *multiplexed client* talking to a
shared Tokio TCP responder. The responder accepts a single connection,
reads `REQ <id>\n` lines, sleeps for `(40 - id*10)ms`, then writes back
`RESP <id>\n`. Because higher-id replies have shorter delays, a real
multiplexed client should observe responses in `[3, 2, 1]` order even
though the requests were submitted as `[1, 2, 3]`.

Both sides assert the arrival order is exactly `[3, 2, 1]`. If a side
observes `[1, 2, 3]` the client is not actually multiplexing — it's
processing one request at a time.

Run both sides:

```bash
cargo run --manifest-path examples/eiffel_mux_client/Cargo.toml -- compare
```

Run one side:

```bash
cargo run --manifest-path examples/eiffel_mux_client/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_mux_client/Cargo.toml -- tina
```

## What this comparison taught us

### Tokio side

- The classic shape: one `tokio::spawn` for the reader, a
  `HashMap<u32, oneshot::Sender>` of pending requests behind an `Arc<Mutex>`,
  and an explicit `await` on each `oneshot::Receiver`. About 60 lines.
- The reader task does ID parsing + dispatch; the submitter writes a line
  and inserts a oneshot. Out-of-order responses fall out of the design
  with no extra effort.
- The hidden cost is the `Arc<Mutex<HashMap>>` — every reader-submit
  interaction goes through it. Easy to write, easy to leak a oneshot
  forever if no one cleans up on stream close.

### Tina side

- The `MuxClient` isolate owns the `StreamId`, the read buffer, and the
  pending count. There is no shared map and no `Arc<Mutex<_>>` — the
  state lives behind a single mailbox.
- Out-of-order arrival just works: the runtime delivers `tcp_read`
  replies in the order bytes hit the socket, the parser walks complete
  lines as they appear, and the `arrival_order` records exactly that.
- The isolate-as-state-machine shape (Begin → Connected → Wrote → Read*
  → Closed) reads cleanly. Each transition is one match arm.

### What was awkward

- **Cannot batch writes followed by reads on the same stream.** First
  attempt issued `batch([write1, write2, write3, read])` because the
  three writes are independent. The runtime wedged. Fix was to
  concatenate the three requests into one `tcp_write(...)` and chain
  `tcp_read` after the `Wrote` reply. This is a real ergonomics gap —
  Tokio's "many concurrent awaits on one stream" pattern doesn't have a
  clean Tina analogue, so multiplexing currently requires either
  concatenated payloads or a more careful sequence of effects.
- **Server in a dedicated Tokio runtime on its own thread.** The Tina
  side has no native HTTP/multiplex client primitives, so the test
  harness puts the responder behind its own `block_on(...)` on a
  separate thread and signals shutdown via `tokio::sync::oneshot`. A
  first attempt used `std::sync::mpsc::Receiver::recv` to block on
  shutdown; that froze the responder's runtime because the executor
  could not progress while the OS thread was blocked on a sync recv.
  This is a small but real "two runtimes don't compose" footgun.
- **Side-channel for the arrival log.** The arrivals end up in
  `Arc<Mutex<Vec<u32>>>` because there is no clean Tina affordance for
  "harvest results from this isolate when it finishes." Same shape as
  the bound-addr smuggling we saw in earlier comparisons.
- **Mailbox boilerplate, fifth copy.** The `MuxMailbox` /
  `MuxMailboxFactory` block is identical to the previous four
  comparisons modulo type names.
- **`#[allow(dead_code)]` on the message enum.** The message variants
  carry `Result<_, CallError>` payloads we don't read, but rustc warns
  on the unread `Ok` payload — the warning cannot be suppressed at the
  variant level cleanly without disabling for the whole enum.