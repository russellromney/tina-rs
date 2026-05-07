# eiffel_graceful_pool_shutdown

Stop a worker pool while callers are pending. Every still-pending
caller must see a typed terminal reply — no silent drop, no host
hang.

## Run

```sh
cargo test --manifest-path examples/eiffel_graceful_pool_shutdown/Cargo.toml
```

## Tina shape

`Frontend` holds `PendingReplies::with_capacity(MAX_PENDING)`. On
`Shutdown` it drains the box and replies `Closed` to every
pending caller in one `Effect::Batch`:

```rust
FrontendMsg::Shutdown => {
    let mut effects = Vec::new();
    for (_qid, slot) in self.pending.drain() {
        effects.push(reply_to(slot, FrontendReply::Closed));
    }
    effects.push(stop());
    Effect::Batch(effects)
}
```

## What feels good

- `pending.drain()` is the canonical "release every captured
  caller" operation. The terminal reply is a regular
  `reply_to(slot, ...)` — no special path.
- After `Shutdown`, the frontend stops cleanly with `stop()`.
  Callers that submitted *after* the frontend stopped see a typed
  bridge-layer outcome (mailbox-full or closed) at their original
  `call(...).reply(...)` site.

## What feels worse

- The host has to send `Shutdown` through the same bounded
  mailbox the regular Submit traffic uses. With six in-flight
  callers and a 64-slot frontend mailbox there's plenty of room,
  but a saturated frontend forces the host into a
  `send_and_observe` retry loop. Same lifecycle paper-cut as
  `eiffel_graceful_drain_server` (Round 2 finding 12).

## Tokio footgun

The first version of `tokio_impl::run` aborted the workers but
forgot to drop the shared `Arc<Mutex<mpsc::Receiver>>`. Buffered
jobs (and their reply oneshots) stayed alive, so callers queued
behind the in-flight ones blocked forever. The fix was a single
`drop(rx)` after `abort_all` — easy to miss because the workers
were correctly aborted and the test passed under low burst.

Tina's path makes this structurally impossible: `pending.drain()`
returns *every* captured slot, the `Effect::Batch(reply_to)` ships
a typed `Closed` to each, and `stop()` settles the runtime. There
is no second container holding live promises behind the explicit
queue.
