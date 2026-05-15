# Lock Manager

Local lock manager. One holder per key, FIFO wait queue per key, leases
that expire on a timer, renewal that extends the lease without dropping
the holder, and stale-handle detection on release / renew.

The manager isolate is the only place that mints `holder_id`s. Every
grant carries a `LockHandle { key, holder_id }`. Release and renew
compare the handle against the current entry; if they don't match, the
caller gets `StaleHandle`.

Lease expiry is a `sleep(lease).then(LeaseExpired { ... })`. Each
scheduled wake carries an `expiry_token` that's bumped on every
acquisition or renewal, so a renewed lease silently ignores the old
wake without needing cancellation. When the matching wake fires and the
holder is still installed, the lock hands off to the next FIFO waiter
(or the entry is removed if the queue is empty).

Caps in this specimen:

- `pending_capacity` — total parked acquire-callers across every key
  (one bounded `PendingReplies`).
- `max_waiters_per_key` — per-key wait queue length.
- `max_keys` — number of active keys with a holder or waiters.
- `mailbox` — lock manager mailbox capacity.

## Run

```bash
cargo run  --manifest-path examples/systems/system_lock_manager/Cargo.toml
cargo test --manifest-path examples/systems/system_lock_manager/Cargo.toml
```

The smoke suite exercises five behaviours:

1. **FIFO fairness** — four contenders queue behind a host-held key;
   admission order matches grant order.
2. **Lease expiry hand-off** — first holder never releases; the parked
   waiter is granted automatically when the lease fires; the original
   release is then rejected as stale.
3. **Renewal extends** — holder renews at half-lease; a parked waiter
   stays parked past where the original lease would have fired.
4. **Stale release rejected** — releasing the same handle twice is
   rejected on the second call.
5. **Per-key overflow** — bursting more contenders than
   `max_waiters_per_key` drains the surplus to `Busy` without
   consuming pending slots.

## Findings

What felt good:
- `sleep(d).then(...)` plus a per-entry `expiry_token` is a clean
  substitute for a real cancel: renewals never need to chase a timer
  handle, and expiry processing is one equality check.
- `PendingReplies::try_insert` with a manual waiter id worked well for
  parking acquire-callers behind one global cap. The same table
  protects every key.
- The `(key, holder_id)` pair is enough for stale-handle detection;
  `holder_id` is monotonic and never reused.
- Hand-off as a single helper (`hand_off`) used by both release and
  expiry paths kept the FIFO invariant in one place.

What felt rough:
- Per-key wait queue length is a hand-rolled `VecDeque<u64>` next to a
  global `PendingReplies`. The pattern "bounded global pending box plus
  per-bucket FIFO order" repeats in every place that wants fair queuing
  (cache_with_fill is the obvious neighbour). A small `WaitList` helper
  could share the cap accounting and the "skip waiters whose slot was
  reclaimed" loop.
- Lease bookkeeping is two numbers (`holder_id`, `expiry_token`) plus
  a reply discriminator. Easy to mis-pair across renew / hand-off if a
  future change forgets to bump one of them. A typed `Lease` newtype
  with a single `bump()` would catch this at compile time.
- `LockMsg::LeaseExpired` is rejected from `handle_call` as
  `UnsupportedMessage`. The split between `handle` (internal events)
  and `handle_call` (caller authority) is correct, but the rejection
  ceremony repeats once per internal variant.
- The smoke harness still hand-rolls the "fan threads through a
  barrier, count outcome variants" shape that `system_cache_with_fill`
  also writes by hand. Worth a small host-side helper if a third
  system needs it.

Tina capability pulled:
- Bounded deferred replies (`PendingReplies`).
- Explicit caller authority (`CallContext`).
- Runtime-owned time (`sleep().then(...)`).
- Generation-stamped messages for stale-event dedup.

Suggested follow-up:
- Consider a `WaitList` helper if `system_tenant_rate_limiter` ends up
  with the same per-key FIFO + global pending shape.
- Promote the host barrier-burst pattern to a shared scenarios module
  if a fourth system writes it again.

Verdict:
- keep
