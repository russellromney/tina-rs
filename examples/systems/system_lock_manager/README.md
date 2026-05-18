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
  handle, and expiry processing is one equality check. In a Tokio
  build this would be `JoinHandle::abort` plus a oneshot cancel
  channel; here the wake just gets ignored.
- `PendingReplies::try_insert` with a manual waiter id worked well for
  parking acquire-callers behind one global cap. The same table
  protects every key.
- The `(key, holder_id)` pair is enough for stale-handle detection;
  `holder_id` is monotonic and never reused.
- Hand-off as a single helper used by both release and expiry paths
  kept the FIFO invariant in one place.
- The split-service shape made the public surface much cleaner:
  `LockRequest` is callable, private `LockEvent::LeaseExpired` is an
  internal continuation, and host code uses `call_blocking_request`.
  Stale-handle detection stays trivial: the slot is either in the
  pending box or it isn't.
- The compiler caught mistakes fast. `Effect<I>` typing meant pasting
  the wrong reply variant failed loudly; `RequestEffect<I>` now means
  forgetting to consume `RequestCall` fails before runtime.

What felt rough:
- Per-key wait queue length is a hand-rolled `VecDeque<u64>` next to a
  global `PendingReplies`, including the "skip waiters whose slot was
  reclaimed" loop. Same shape as `system_cache_with_fill`. Promoted to
  `examples/FINDINGS.md` as a `WaitList` helper candidate.
- Lease bookkeeping is two unrelated `u64`s (`holder_id`,
  `expiry_token`) that have to be bumped in lockstep at the right
  times (new acquire, renew, hand-off). Easy to mis-pair if a future
  change forgets to bump one. A typed `Lease` newtype with one
  `bump()` would make this a compile-time invariant instead of a
  discipline thing. Local to this specimen for now.
- Parking a waiter now goes through `RequestCall::capture(...)`, which
  is safer than raw `CallContext::into_request_context()`, but still
  makes the `PendingReplies` insertion ceremony visible.
- Whether a single global `PendingReplies` cap is the right shape for
  a lock manager is genuinely unclear. One noisy key can starve
  waiters on every other key. A real lock manager probably wants the
  per-key cap as primary admission control and a global cap only as
  a backstop. Followed the prompt's "every wait queue and per-isolate
  map needs a cap" but flagging the sharing pattern as worth more
  thought for non-toy versions.

What felt rough in the smoke harness, not the framework:
- The contended-FIFO test needed a sequential admission gate
  (sleep 20ms between threads) because `Barrier` releases all four
  threads simultaneously and the order in which their `Acquire`
  messages reach the isolate is racy. Always true of concurrent FIFO
  testing, but a `host_burst_in_order` helper would pay for itself
  across cache, queue, and lock.
- The first cut of the busy-overflow test deadlocked because granted
  threads didn't auto-release, so the hand-off chain blocked behind a
  long lease (lease_ms == call_timeout_ms). Fixed by releasing on
  grant, but the `lease_ms` / `call_timeout_ms` interaction is a
  sharp edge for test design.

Tina capability pulled:
- Bounded deferred replies (`PendingReplies`).
- Explicit caller authority (`CallContext`).
- Runtime-owned time (`sleep().then(...)`).
- Generation-stamped messages for stale-event dedup.

Suggested follow-up:
- See `examples/FINDINGS.md` items for the cross-system patterns.
- Local: try a `Lease` newtype if a future change to this specimen
  forces a third bump-site for `expiry_token`.

Verdict:
- keep
