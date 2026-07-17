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

- `waiter_capacity` — total parked acquire-callers across every key.
- `max_waiters_per_key` — per-key FIFO capacity. `SharedWork` owns both
  limits and returns typed `GlobalFull` / `KeyFull` replies.
- `max_keys` — number of active keys with a holder or waiters.
- `mailbox` — lock manager mailbox capacity.

`RunConfig::validate` rejects zero and oversized waiter, key, mailbox, and
duration fields before the runtime or `SharedWork` table is constructed.

## Run

```bash
cargo run  --manifest-path examples/systems/system_lock_manager/Cargo.toml
cargo test --manifest-path examples/systems/system_lock_manager/Cargo.toml
cargo test --manifest-path examples/systems/system_lock_manager/Cargo.toml --test public_smoke public_smoke -- --exact
```

The smoke suite exercises nine behaviours:

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
   `max_waiters_per_key` returns `Busy(KeyFull)` without consuming the
   remaining global capacity.
6. **Global overflow** — two independently held keys prove
   `Busy(GlobalFull)` is distinct from per-key pressure.
7. **Caller-gone refill** — a timed-out FIFO head is reclaimed and a new
   caller reuses the exact capacity-one slot before hand-off.
8. **Keyspace overflow** — a new key is rejected at `max_keys` while the
   existing holder remains valid.
9. **Fallible bounded shape** — zero capacity is returned as configuration
   failure instead of reaching a constructor panic.

Focused unit probes inject `CallError::TimerFull` and prove a current lease is
retired rather than becoming immortal, while a stale timer failure cannot
revoke the current holder. Another probe closes a selected FIFO caller before
delivery and proves the generation-stamped lease timer rolls the ghost holder
back without overlapping a second owner.

## Findings

What felt good:
- `sleep(d).then(...)` plus a per-entry `expiry_token` is a clean
  substitute for a real cancel: renewals never need to chase a timer
  handle, and expiry processing is one equality check. In a Tokio
  build this would be `JoinHandle::abort` plus a oneshot cancel
  channel; here the wake just gets ignored.
- `SharedWork::with_key_limit` is the complete lock-waiter vocabulary:
  `wait` consumes `RequestCall` authority, `take_next` selects the oldest
  live caller, and one table owns FIFO order, both caps, and reclamation.
- The `(key, holder_id)` pair is enough for stale-handle detection;
  `holder_id` is monotonic and never reused.
- Hand-off as a single helper used by both release and expiry paths
  kept the FIFO invariant in one place.
- The split-service shape made the public surface much cleaner:
  `LockRequest` is callable, private `LockEvent::LeaseExpired` is an
  internal continuation, and host code uses `call_blocking_request`.
  Stale-handle detection stays trivial: the holder generation either
  matches or it does not.
- `LocalSystem` supplies every host operation this specimen needs:
  fallible startup, split registration, typed blocking request calls,
  scoped concurrent callers, and a consuming `run_to_shutdown_reported`
  terminal report that keeps workload and shutdown failures distinct.
- Carrying `SleepReply` into `LeaseExpired` makes timer admission failure
  visible and typed instead of treating every continuation as a wake. A
  current timer failure retires or hands off the lease immediately because an
  unscheduled expiry cannot enforce lease ownership.
- The compiler caught mistakes fast. `Effect<I>` typing meant pasting
  the wrong reply variant failed loudly; `RequestEffect<I>` now means
  forgetting to consume `RequestCall` fails before runtime.

What felt rough:
- Lease bookkeeping is two unrelated `u64`s (`holder_id`,
  `expiry_token`) that have to be bumped in lockstep at the right
  times (new acquire, renew, hand-off). Easy to mis-pair if a future
  change forgets to bump one. A typed `Lease` newtype with one
  `bump()` would make this a compile-time invariant instead of a
  discipline thing. Local to this specimen for now.
- The global cap remains a deliberate backstop shared by all keys. One
  hot key can consume it up to its per-key limit; production tuning must
  choose the per-key cap with that fairness policy in mind.

What felt rough in the smoke harness, not the framework:
- A first contended-FIFO probe used a 20ms sequential-admission delay. The
  adversarial pass replaced that scheduling assumption with a handshake on the
  lock manager's observed waiter count before each next admission. The proof
  now establishes the actual server-side FIFO order without a host timing
  guess or a framework helper.
- The first cut of the busy-overflow test deadlocked because granted
  threads didn't auto-release, so the hand-off chain blocked behind a
  long lease (lease_ms == call_timeout_ms). Fixed by releasing on
  grant, but the `lease_ms` / `call_timeout_ms` interaction is a
  sharp edge for test design.

Tina capability pulled:
- Bounded keyed FIFO hand-off (`SharedWork`).
- Linear caller authority (`RequestCall`).
- Runtime-owned time with typed completion (`sleep().then_service_event(...)`).
- Generation-stamped messages for stale-event dedup.
- Fallible `LocalSystem` ownership and truthful bounded consuming shutdown.

Suggested follow-up:
- Local: try a `Lease` newtype if a future change to this specimen
  forces a third bump-site for `expiry_token`.

Verdict:
- keep
