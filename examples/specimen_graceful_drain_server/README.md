# specimen_graceful_drain_server

Tokio-vs-Tina graceful drain. The driver fires a burst of 16 jobs at
a worker with a 4-slot queue, then signals shutdown. The contract is:
no admissions after shutdown, but every already-admitted job must
complete before exit.

## Run

```sh
cargo run --manifest-path examples/specimen_graceful_drain_server/Cargo.toml -- both
cargo test --manifest-path examples/specimen_graceful_drain_server/Cargo.toml
```

```
side=tokio items_admitted=4 items_full=12 items_processed=4 shutdown_observed=true exit_clean=true
side=tina  items_admitted=4 items_full=12 items_processed=4 shutdown_observed=true exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape

Two channels: `mpsc::channel(QUEUE_CAPACITY)` for jobs, `oneshot` for
shutdown. The consumer task runs `tokio::select!` between them:

```rust
loop {
    if shutdown_seen {
        match rx.try_recv() {                // drain: drain queue, exit on empty
            Ok(_) => { sleep(work).await; processed += 1; }
            Empty | Disconnected => break,
        }
    } else {
        tokio::select! {
            biased;
            item = rx.recv() => { /* ... process ... */ }
            _ = &mut shutdown_rx => { shutdown_seen = true; }
        }
    }
}
```

Two pieces of host knowledge are required to make this work:

1. The producer must NOT drop `tx` before the consumer has entered
   drain mode. If it does, `recv()` returns `None` first and the
   `shutdown_rx` arm never wins; `shutdown_observed` ends up
   `false`. The fix here is "drop `tx` *after* the consumer task
   joins."
2. The drain branch must use `try_recv` (not `recv`), or the task
   parks waiting on a channel the producer no longer feeds. That's
   not a bug in the channel; it's a discipline you have to remember.

## Tina shape

One mailbox carries everything. `Drain` is just another `WorkerMsg`
variant alongside `Submit` and `Tick`:

```rust
enum WorkerMsg {
    Submit(u32),
    Tick(SleepReply),
    Drain,
}
```

After `Drain` arrives, the worker captures the observed admitted
count as `expected: Option<u32>` and stops admitting (the host
already stopped sending after the burst, but if a late `Submit`
landed first the worker would still process it because it was
admitted in good faith). The drain completes when
`pending == 0 && processed >= expected`:

```rust
WorkerMsg::Drain => {
    self.report.shutdown_observed = true;
    self.expected = Some(self.report.items_admitted);
    if self.drained_and_done() { stop_with(self.report) } else { noop() }
}
```

`drained_and_done()` is the explicit drain-truth check, named once
on `impl Worker`. The host blocks on
`runtime.observe_result::<Report>(worker_addr)` and the
`stop_with(report)` effect publishes the typed final value to that
waiter.

The host delivers `Drain` through the same bounded mailbox the
submits went through. The mailbox may still hold queued admissions
when shutdown is requested (the worker drains at one per
`JOB_WORK_MS`); the host calls
`runtime.send_observed_until(...)` (Phase 062 Rock 4) which
retries `MailboxFull` / `IngressFull` up to a deadline. The
producer side uses `try_send_outcome` + `HostBurstOutcomes`
(Phase 062 Rocks 3 & 4) so the per-shard admit / mailbox-full /
ingress-full counts come back as a typed snapshot, no observer
closure. The Worker's "one Tick in flight, plus N queued"
invariant is `SingleCallGate` (Phase 062 Rock 5).

## Discussion

What feels better:

- **One mailbox, one shape.** There is no second channel for
  shutdown, no `select!`, no "did I drop tx at the right time"
  ordering question. `Drain` is a message; `pending == 0 &&
  processed >= expected` is the drain-truth.
- **Drain-truth is a property of state, not a control-flow phase.**
  The handler can answer "are we done?" at any turn. There is no
  state machine where "drain mode" is invisible to the rest of the
  isolate.
- **The final report reaches the host without an mpsc.**
  `stop_with(report)` + `observe_result` is the blessed Phase 059
  Rock 1 path.

What feels worse:

- **`Drain` send through a bounded mailbox can hit `Full`.** When
  shutdown arrives, the mailbox may still be queueing admitted
  Submits. The host's retry loop polls until a slot opens. That's
  correct but "shutdown signal can be Full" is an awkward sentence
  to defend. A separate "control" mailbox (lower-priority lane,
  always-accepts) would make this cleaner. (Same finding as Round
  2 #4.)
- **No drain timeout is built in.** Real production drains often
  have a timeout — "give in-flight 5 seconds, then force-stop and
  report what's still pending." Today the user implements this by
  hand: an additional `DrainDeadlineFired` message scheduled via
  `sleep`, and a check in `is_done()` that returns true on
  deadline-fired even with `pending > 0`. The
  `BridgeShutdownReport` in `tina-tokio-bridge` has a
  `drained_within_timeout` flag for the bridge case; an isolate-
  level equivalent would make this specimen's pattern reusable.

## What this suggests

A small `runtime.observe_pending(addr)` (count of mailbox + in-flight
calls for an isolate) would give the host an alternative to passing
`expected_total` into the worker. The worker still tracks its own
`pending`; the host can poll `observe_pending(addr)` from the
shutdown sequence to know when drain is complete without needing the
isolate to publish a separate count. Not urgent — the
`observe_result + stop_with` path here is fine — but it would
generalize to cases where the isolate cannot stop with a single
typed value.
