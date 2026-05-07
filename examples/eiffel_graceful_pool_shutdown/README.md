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
pending caller in one `Effect::Batch` plus a trailing `stop()`,
expressed with the typed `drain_into_stop` helper:

```rust
FrontendMsg::Shutdown => {
    self.pending.drain_into_stop::<Self>(FrontendReply::Closed)
}
```

The helper is compile-time typed so a `PendingReplies<K, R>`
only produces `Effect<I>` when `I::Reply = R`. The method name
says `stop` on purpose — nothing else in the helper appends
`stop()` for you, and the underlying `pending.drain()` /
`reply_to(slot, ...)` semantics are unchanged.

## What feels good

- `drain_into_stop::<Self>(R::Closed)` is the one-liner for the
  common service-stop pattern. The slot ordering follows the
  internal slot table (first-allocated first).
- The terminal reply is still a regular `reply_to(slot, ...)`
  under the hood — no special path. Use the longer-form
  `pending.drain()` + manual loop when the per-caller reply
  needs to carry the key.
- After `Shutdown`, the frontend stops cleanly. Callers that
  submitted *after* the frontend stopped see a typed
  bridge-layer outcome (mailbox-full or closed) at their
  original `call(...).reply(...)` site.

## What feels worse

- `Shutdown` rides the same bounded mailbox as the regular
  `Submit` traffic. With six in-flight callers and a 64-slot
  frontend mailbox there is plenty of room; the host calls
  `runtime.send_observed_until(...)` (Phase 062 Rock 4) which
  retries `MailboxFull` / `IngressFull` up to a deadline. The
  hand-rolled retry loop is gone, but the underlying shape (a
  control message rides the data mailbox) is the same one in
  `eiffel_hot_key_fairness`'s `Drain(admitted)`. See FINDINGS
  finding 9 (drain helper for `PendingReplies` at service stop)
  for the related product gap.

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
