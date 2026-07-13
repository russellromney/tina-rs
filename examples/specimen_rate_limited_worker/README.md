# specimen_rate_limited_worker

Tokio-vs-Tina single-consumer rate-limited worker fed by a tight burst.

The host pushes [`BURST_JOBS`] jobs as fast as it can. The worker
processes one job per [`RATE_WINDOW_MS`]. The queue is bounded at
[`QUEUE_CAPACITY`] on both sides. The point of the comparison is how
overload shows up at the producer.

`RATE_WINDOW_MS` must be non-zero, no greater than one second, and divide one
second exactly. A compile-time assertion keeps the configured token rate and
the documented refill interval identical.

Both sides should see at least one rejection at the producer and
finish processing every admitted job. Exact admit/full counts are
timing-sensitive (the worker may have drained one slot by the time
the producer pushes the next job), so the smoke tests assert
structural invariants:

- `admitted + full + terminal == BURST_JOBS`
- `full > 0` (overload was visible)
- `terminal == 0` (the worker remained live for the burst)
- `received == admitted`
- `processed == received`
- `worker_terminal == None`
- Tina's `burst_close_settlement == Delivered`
- `exit_clean`

## Run

```sh
cargo run --manifest-path examples/specimen_rate_limited_worker/Cargo.toml -- both
cargo test --manifest-path examples/specimen_rate_limited_worker/Cargo.toml
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape

`tokio::sync::mpsc::channel::<u32>(QUEUE_CAPACITY)` is the queue. A
consumer task awaits each job, sleeps `RATE_WINDOW`, increments
`processed`. The producer uses `Sender::try_send`:

```rust
match tx.try_send(n) {
    Ok(())                                  => admitted += 1,
    Err(TrySendError::Full(_))              => full     += 1,
    Err(TrySendError::Closed(_))            => bail,
}
```

The pattern is small and well-known. The downside: the queue and the
consumer task are two separate things — nothing structural prevents
adding a second producer task that bypasses the rate limit, or a
sneaky `tx2 = tx.clone()` that wedges into the same queue. Discipline
holds it together.

## Tina shape

The worker is one isolate with mailbox capacity `QUEUE_CAPACITY`.
There is no separate queue; the mailbox *is* the queue. The rate
limit is a single-key `RateLimit` token bucket in the worker's state. The
worker asks `try_admit_at(worker_key, ctx.now())`; an admitted token processes
one pending job, while `RateLimited { retry_after }` schedules exactly
one `sleep(retry_after).then(Tick)`. Explicit `pending` and `pacing`
state keeps one timer in flight. The host closes the burst with a
normal Tina message: `BurstClosed(admitted)`.

```rust
WorkerMsg::Submit => {
    self.report.jobs_received += 1;
    self.pending += 1;
    if self.pacing { noop() } else { self.drive(ctx) }
}
WorkerMsg::Tick(reply) => {
    if let Some(terminal) = pacing_terminal(reply) {
        self.report.worker_terminal = terminal;
        self.report.exit_clean = false;
        return stop_with(std::mem::take(&mut self.report));
    }
    self.pacing = false;
    self.drive(ctx)
}
match self.limiter.try_admit_at(worker_key, ctx.now()) {
    RateLimitDecision::Admitted => {
        self.processed += 1;
        self.pending -= 1;
        // Loop and ask for the next pending job.
    }
    RateLimitDecision::RateLimited { retry_after, .. } => {
        self.pacing = true;
        return sleep(retry_after).then(WorkerMsg::Tick);
    }
    RateLimitDecision::KeyCapacityFull(report) => {
        self.report.worker_terminal = WorkerTerminal::RatePolicy(
            RatePolicyTerminal::KeyCapacityFull(report),
        );
        self.report.exit_clean = false;
        return stop_with(std::mem::take(&mut self.report));
    }
    RateLimitDecision::Closed(report) => {
        self.report.worker_terminal = WorkerTerminal::RatePolicy(
            RatePolicyTerminal::Closed(report),
        );
        self.report.exit_clean = false;
        return stop_with(std::mem::take(&mut self.report));
    }
}
```

The host uses the host burst outcome helper's `try_send_outcome` plus a shared
`HostBurstOutcomes` accumulator. That keeps the burst non-blocking and
preserves every typed outcome (admitted / mailbox_full / mailbox_closed
/ ingress_full / worker_stopped) without per-send observer ceremony:

```rust
let outcomes = HostBurstOutcomes::new();
for _ in 0..BURST_JOBS {
    let _ = app.try_send_outcome(worker_addr, Submit, &outcomes);
}
outcomes.wait_complete(deadline)?;
let snap = outcomes.snapshot();
```

After every observer fires, the host sends `BurstClosed(admitted)`
through `send_observed_until`. If the
mailbox is full of admitted `Submit`s, the helper retries until a slot
opens or the deadline elapses; the typed `Closed` / `Timeout` /
`WorkerStopped` outcomes stay distinct. That is deliberate: "done
sending" is app control state, so it travels as a message rather than
an `Arc<AtomicU32>` side channel.

If the worker has already stopped with a typed worker terminal, the host
still waits for `observe_result` before interpreting a `Closed` or
`WorkerStopped` control-send result. That preserves the worker's exact
`KeyCapacityFull`/`Closed` terminal kind and the policy's rejection report; the
same control outcomes remain errors when no worker terminal explains them.
`Timeout` and provenance failures return immediately with their typed error
source instead of being hidden by a later result-wait timeout. The report also
retains the exact `Delivered`/`Closed`/`WorkerStopped` control settlement
instead of projecting all three to success.

The report keeps the complete `HostBurstSnapshot` alongside its cross-runtime
`admitted`/`full`/`terminal` projection. That makes `MailboxFull`,
`IngressFull`, `MailboxClosed`, and `WorkerStopped` independently auditable.
If the pacing sleep itself fails, its exact `CallError` is retained in
`worker_terminal` rather than disappearing into `exit_clean = false`.

The final `Report` comes back via `app.observe_result::<Report>` —
no mpsc, no atomics for the value, no host-side accumulator.

## Discussion

What feels better:

- **One thing to pin pressure on.** The mailbox capacity is the rate
  limit's queue cap. There is no second `mpsc` to also size, no
  shared `Arc<...>` to also bound. Sizing the worker is one knob.
- **Rate window is in the trace.** Every processed job is one
  `Sleep` + one `Tick`. Reading the trace tells you the rate the
  worker actually achieved without any extra instrumentation.
- **Producer learns the truth.** `try_send_outcome` distinguishes
  `MailboxFull` (the queue is at cap) from `IngressFull` (the worker
  thread can't even pick up the command), while also retaining
  `MailboxClosed` and `WorkerStopped` as terminal buckets. Tokio's
  `try_send` has `Full` and `Closed` only.
- **The rate gate is honestly exhaustive.** `RateLimitDecision` contains
  only `Admitted`, `RateLimited`, `KeyCapacityFull`, and `Closed`. The specimen
  reports the two terminal policy outcomes separately instead of hiding them
  behind a wildcard or a shared failure bucket.
- **Terminal projections retain their source truth.** The comparison still
  has compact `full` and `terminal` totals, but Tina's exact burst snapshot,
  pacing failure, and end-of-burst control settlement remain available.
- **Final value via `stop_with`.** The host reads the worker's
  `Report` through `observe_result`. No mpsc plumbing, no `Arc<Mutex>`
  for the answer.

What feels worse:

- **Two messages per job feels heavy.** Each job is one `Submit` and
  one `Tick`; reading `WorkerMsg`'s enum, the rate-limit shape isn't
  obvious until you see the `sleep(...).then(Tick)` line. With
  Tokio, `recv().await; sleep().await` is the rate limit textually.
