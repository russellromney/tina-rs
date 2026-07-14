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
work    (call worker request lane, WorkerRequest::Do)
release (release_effect(lease, pool, Reuse|Retire, ...))
  -> ReleaseOutcome::Released | Retired | StaleLease | DoubleRelease | PoolClosed
```

The worker uses the canonical split-service form: `WorkerRequest::Do`
is caller-authority input, while the timer continuation is a private
`WorkerEvent`. The pool stores the typed request handles, so an event
address cannot accidentally be used as the callable worker lane.

On `Shutdown` the driver sends one `WorkerPoolMsg::Close(CloseMode::Drain)`.
The pool replies `Closed` to every parked waiter in one batch and
acknowledges the close itself. Outstanding leases drain normally; once
the call site releases them, the pool reports `Released` and leaves
the resources idle but unavailable to new acquires. The report waits
for those releases, so close acknowledgement cannot hide an unsettled
lease.

## Tokio shape

The tokio side keeps the original `mpsc::channel` worker pool: a
buffered job channel, oneshot reply per submission, `JoinSet::abort_all`
plus an explicit `drop(rx)` on shutdown. The two sides converge at
the same `Report { completed, closed, failed, shutdown_close_observed }`
shape, so the test proves shutdown was not merely inferred from
callers settling.

## Where Full / Closed / Timeout appear

- `Shutdown` is scheduled by an actor-owned typed timer in the same
  initial effect turn as the bounded caller fanout. Its `CallError`,
  if any, remains in the report. The host only starts the scenario;
  elapsed host time is not used as proof that shutdown began.
- The resulting close call still rides the pool's bounded mailbox.
  Its `Full`, `Closed`, `Timeout`, `Rejected(reason)`, wrong-reply,
  and acknowledged-close outcomes remain distinct.

| outcome     | when                                                          |
|-------------|---------------------------------------------------------------|
| `Acquired`  | a worker was idle (or one was just released to this caller)   |
| `Full`      | all workers busy *and* the waiter table at `max_waiters`      |
| `Closed`    | shutdown landed while caller was parked                       |
| `Timeout`   | caller's `call(pool, Acquire, ...)` timeout fired (and the    |
|             | pool sweeps the slot on its next message)                     |

These four cases are distinct enum variants; nothing collapses them
into a generic error.

The public Tina report also retains every `CallRejectedReason` from
the acquire, worker, release, and close calls. A worker timer failure
is returned as its exact `CallError`; it cannot masquerade as a closed
worker.

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

## Bounded driver state

The driver admits exactly `CALLERS` jobs and stores their state in a
fixed `[Option<JobState>; CALLERS]` table keyed by the bounded job id.
Both the effect producer and the retained in-flight state therefore
share the same explicit bound; the example has no sidecar allocation
that can grow independently of admission.
