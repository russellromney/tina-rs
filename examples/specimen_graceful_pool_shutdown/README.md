# specimen_graceful_pool_shutdown

Stop a bounded `WorkerPool` while callers are still parked. Every
still-pending caller must see a typed terminal reply — no silent drop,
no host hang.

## Run

```sh
cargo test --manifest-path examples/specimen_graceful_pool_shutdown/Cargo.toml
```

## Tina shape

`tina_runtime::pool::WorkerPool` owns the worker addresses as
resources. Each caller does the explicit three-step:

```text
acquire (call WorkerPoolMsg::Acquire)
  -> AcquireOutcome::Acquired(lease) | Full | Closed | Timeout
work    (call worker, WorkerMsg::Do)
release (release_effect(lease, pool, Reuse|Retire, ...))
  -> ReleaseOutcome::Released | Retired | StaleLease | DoubleRelease | PoolClosed
```

On `Shutdown` the driver sends one `WorkerPoolMsg::Close(CloseMode::Drain)`.
The pool replies `Closed` to every parked waiter in one batch and
acknowledges the close itself. Outstanding leases drain normally; once
the call-site releases them the pool reports `Retired` (the close
turns every release into a retire) so capacity is honestly accounted.

## Tokio shape

The tokio side keeps the original `mpsc::channel` worker pool: a
buffered job channel, oneshot reply per submission, `JoinSet::abort_all`
plus an explicit `drop(rx)` on shutdown. The two sides converge at
the same `Report { completed, closed, failed }` shape.

## Where Full / Closed / Timeout appear

- `Shutdown` rides the same bounded mailbox as the regular
  `Submit` traffic. With six in-flight callers and a 64-slot
  frontend mailbox there is plenty of room; the host calls
  `runtime.send_observed_until(...)` (Phase 062 Rock 4) which
  retries `MailboxFull` / `IngressFull` up to a deadline. The
  hand-rolled retry loop is gone, but the underlying shape (a
  control message rides the data mailbox) is the same one in
  `specimen_hot_key_fairness`'s `Drain(admitted)`. See FINDINGS
  finding 9 (drain helper for `PendingReplies` at service stop)
  for the related product gap.

| outcome     | when                                                          |
|-------------|---------------------------------------------------------------|
| `Acquired`  | a worker was idle (or one was just released to this caller)   |
| `Full`      | all workers busy *and* the waiter table at `max_waiters`      |
| `Closed`    | shutdown landed while caller was parked                       |
| `Timeout`   | caller's `call(pool, Acquire, ...)` timeout fired (and the    |
|             | pool sweeps the slot on its next message)                     |

These four cases are distinct enum variants; nothing collapses them
into a generic error.

## Why explicit release is more verbose but safer

A pool lease is move-only and identity-checked (pool id, resource id,
generation). Forgetting `release(lease, ...)` leaks a resource until
the pool closes — but **dropping the lease silently does nothing on
purpose**. There is no `Drop` magic that auto-returns the lease, and
no auto-retry on a busy release mailbox. You either name the
disposition (`Reuse` / `Retire`) at the call site or you keep the
lease.

## How caller cancellation removes waiters

Acquire via `call_cancelable(pool, WorkerPoolMsg::Acquire, timeout)`
gives the caller a `CallHandle`. Firing `cancel_call(handle)` closes
the caller-side wait; the pool's deferred reply slot for that waiter
moves to `Closed`. The pool sweeps closed slots on every incoming
message, so capacity is reclaimed without a separate `CancelWaiter`
ping. FIFO order of remaining waiters is preserved across mid-queue
cancels.

If the cancel races the dispatch — caller cancels between the pool
emitting `Acquired(lease)` and the runtime delivering it — the
deferred reply is rejected, the value drops, and the pool's
`sweep_in_flight` notices the rejection on its next handler turn and
returns the resource to Idle (counted under `dispatch_recovered`). No
cancel timing leaks a resource.

## Driver shape caveat

This specimen's `Driver` keeps in-flight per-job state in a
`HashMap<u32, JobState>` for clarity. A production driver should
use a fixed-capacity table (slab, ring, or `PendingReplies`) so the
in-flight set has the same kind of bound the pool itself enforces;
`HashMap` is unbounded.
