# specimen_rate_limited_worker

Tokio-vs-Tina single-consumer rate-limited worker fed by a tight burst.

The host pushes [`BURST_JOBS`] jobs as fast as it can. The worker
processes one job per [`RATE_WINDOW_MS`]. The queue is bounded at
[`QUEUE_CAPACITY`] on both sides. The point of the comparison is how
overload shows up at the producer.

Both sides should see at least one rejection at the producer and
finish processing every admitted job. Exact admit/full counts are
timing-sensitive (the worker may have drained one slot by the time
the producer pushes the next job), so the smoke tests assert
structural invariants:

- `admitted + full == BURST_JOBS`
- `full > 0` (overload was visible)
- `processed == admitted`
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
limit lives in the worker's own state machine: each `Submit` returns
`sleep(RATE_WINDOW).reply(Tick)`. A `SingleCallGate` (Phase-062 Rock 5)
keeps at most one `sleep` in flight; the matching `Tick` increments
`processed`. The host closes the burst with a normal Tina message:
`BurstClosed(admitted)`.

```rust
WorkerMsg::Submit(_) => {
    self.report.jobs_admitted += 1;
    if self.gate.submit() { sleep(self.rate_window).reply(WorkerMsg::Tick) }
    else { noop() }
}
WorkerMsg::Tick(_) => {
    self.processed += 1;
    let more_work = self.gate.complete();
    if self.is_done() { stop_with(self.report) }
    else if more_work { sleep(self.rate_window).reply(WorkerMsg::Tick) }
    else { noop() }
}
WorkerMsg::BurstClosed(admitted) => {
    self.expected = Some(admitted);
    if self.is_done() { stop_with(self.report) } else { noop() }
}
```

The host uses Phase-062 Rock 3's `try_send_outcome` plus a shared
`HostBurstOutcomes` accumulator. That keeps the burst non-blocking and
preserves every typed outcome (admitted / mailbox_full / mailbox_closed
/ ingress_full / worker_stopped) without per-send observer ceremony:

```rust
let outcomes = HostBurstOutcomes::new();
for n in 0..BURST_JOBS {
    let _ = runtime.try_send_outcome(worker_addr, Submit(n), &outcomes);
}
outcomes.wait_complete(deadline)?;
let snap = outcomes.snapshot();
```

After every observer fires, the host sends `BurstClosed(admitted)`
through Phase-062 Rock 4's `send_observed_until` retry helper. If the
mailbox is full of admitted `Submit`s, the helper retries until a slot
opens or the deadline elapses; the typed `Closed` / `Timeout` /
`WorkerStopped` outcomes stay distinct. That is deliberate: "done
sending" is app control state, so it travels as a message rather than
an `Arc<AtomicU32>` side channel.

The final `Report` comes back via `runtime.observe_result::<Report>` —
no mpsc, no atomics for the value, no host-side accumulator.

## Discussion

What feels better:

- **One thing to pin pressure on.** The mailbox capacity is the rate
  limit's queue cap. There is no second `mpsc` to also size, no
  shared `Arc<...>` to also bound. Sizing the worker is one knob.
- **Rate window is in the trace.** Every processed job is one
  `Sleep` + one `Tick`. Reading the trace tells you the rate the
  worker actually achieved without any extra instrumentation.
- **Producer learns the truth.** `send_and_observe` distinguishes
  `MailboxFull` (the queue is at cap) from `IngressFull` (the worker
  thread can't even pick up the command). Tokio's `try_send` only
  has the former.
- **Final value via `stop_with`.** The host reads the worker's
  `Report` through `observe_result`. No mpsc plumbing, no `Arc<Mutex>`
  for the answer.

What feels worse:

- **Two messages per job feels heavy.** Each job is one `Submit` and
  one `Tick`; reading `WorkerMsg`'s enum, the rate-limit shape isn't
  obvious until you see the `sleep(...).reply(Tick)` line. With
  Tokio, `recv().await; sleep().await` is the rate limit textually.
