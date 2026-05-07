# eiffel_rate_limited_worker

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
cargo run --manifest-path examples/eiffel_rate_limited_worker/Cargo.toml -- both
cargo test --manifest-path examples/eiffel_rate_limited_worker/Cargo.toml
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
`sleep(RATE_WINDOW).reply(Tick)`, and the matching `Tick` increments
`processed`.

```rust
WorkerMsg::Submit(_) => {
    self.report.jobs_admitted += 1;
    sleep(self.rate_window).reply(WorkerMsg::Tick)
}
WorkerMsg::Tick(_) => {
    self.processed += 1;
    if self.processed >= expected_total.load(Acquire) { stop_with(self.report) }
    else { noop() }
}
```

The host calls `runtime.send_and_observe(...)` to get the actual
mailbox outcome:

```rust
match runtime.send_and_observe(worker_addr, Submit(n)) {
    Ok(())                                            => admitted += 1,
    Err(MailboxFull) | Err(IngressFull)               => full     += 1,
    Err(other)                                        => bail,
}
```

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
- **`expected_total` still needs an `Arc<AtomicU32>`.** The worker
  needs to know "host stopped sending" to know when to stop. The
  host can't easily push a `Stop` message through the bounded
  mailbox after a saturating burst (the mailbox is full of `Submit`s).
  The atomic is small but it's still a side channel — Tina-shaped
  control plane next to the data plane.
- **`send_and_observe` is sync per-call.** Each producer step is a
  worker-thread roundtrip. For this specimen it's the right shape;
  for high-rate ingress benchmarks it would be the bottleneck.
  `try_send` is the lighter path but loses the typed `Full` / `Closed`
  split.

What this suggests:

- A bounded "control" mailbox separate from the data mailbox would
  let the host send `Stop` / `Drain` after a saturating burst
  without racing for a slot. Today the canonical answer is "use a
  separate isolate as the control plane" — but for one shutdown
  signal that feels heavy. (Related: FINDINGS notes on
  reply-vs-incoming capacity in Rock 4.)
- A `runtime.try_send_with_outcome(...)` that returns
  `Sent`/`MailboxFull`/`IngressFull`/`Closed` *without* a
  worker-thread roundtrip would let high-rate ingress code stay
  precise without blocking. Today the choice is fast-but-coarse
  (`try_send`) vs precise-but-blocking (`send_and_observe`). The
  bounded host-send helpers from FINDINGS Rock 5 (`send_blocking` /
  `send_retrying`) are planned but not yet shipped — the closest
  current shape is `send_and_observe`, used here.
