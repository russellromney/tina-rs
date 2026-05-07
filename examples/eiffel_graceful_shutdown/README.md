# eiffel_graceful_shutdown

The operator hits Ctrl-C while a service has work in flight; the
service stops accepting new work, drains what's already queued, and
exits cleanly. Both sides observe a real SIGINT, raised by a side
thread `SIGNAL_AFTER_MS` after start.

## Run

```sh
cargo run --manifest-path examples/eiffel_graceful_shutdown/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_graceful_shutdown/Cargo.toml -- tina
```

There is **no `both` mode**. Each side installs its own process-wide
signal handler, and running them in one process means one side's
handler fires during the other's run. Run them separately.

You'll see something like:

```
side=tokio items_produced=4 items_processed=4 signal_received=true remaining_in_queue=0 exit_clean=true
side=tina  items_produced=3 items_processed=3 signal_received=true remaining_in_queue=0 exit_clean=true
```

(The exact `items_produced` count varies slightly based on
scheduler timing — both sides stop at the first item after SIGINT.)

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

Both files are self-contained — producer, consumer, signal watcher.

## Tokio shape

A producer task using `tokio::select!` to race an
`ITEM_INTERVAL_MS` timer against `tokio::signal::ctrl_c()`. On
signal, drop the sender; the consumer's `while let Some(_) =
rx.recv().await {}` exits naturally when the channel closes. State
is shared via `Arc<AtomicU32>` counters and `Arc<AtomicBool>` flags.

## Tina shape

Three isolates, one job each:

- **`Producer`** — `Tick → sleep → TimerFired → batch(send(consumer,
  Item) + sleep)`. Carries a `stopped: bool`. On `Stop`, just sets
  the flag; existing in-flight `TimerFired`s see it and bail.
- **`Consumer`** — `Item → sleep(work) → Done`, increments
  `processed` on `Done(Ok)`.
- **`SignalWatcher`** — `Begin → signal_wait("sigint", t).reply →
  Received(Ok(_)) → send(producer, Stop)`. The runtime owns the
  signal handler installation; the watcher just `await`s the named
  signal.

The host thread polls a shared `Telemetry` slot for
`signal_received && producer_stopped && processed >= produced` and
then shuts down.

## Discussion

What feels better:

- **`signal_wait("sigint", timeout)` is a typed runtime call.** No
  `tokio::pin!`, no `select!` arm, no async stream type. The
  watcher just waits for a signal name with a deadline.
- **Stop is a message, not a `select!` branch.** The producer's
  `Stop` arm sets a flag; existing in-flight timers see the flag
  and bail. The Tokio version's "produce or quit" race lives inside
  a `select!` block, which means the signal observation is tangled
  with the production loop.
- **Counters and flags live in one `Telemetry` struct.** Same
  ownership shape as the other examples.

What feels worse:

- **The host's drain-wait is a poll loop on `Telemetry`.** Same
  app-data side channel pattern as `eiffel_outbound_fetch`,
  `eiffel_persistent_counter`, etc. A typed observation handle
  for "this isolate's work has settled" would close it.
- **No `both` mode.** Tokio's `tokio::signal::ctrl_c` and
  `tina_runtime::signal_wait` both install process-wide handlers
  that don't cleanly coexist (FINDINGS.md "Tokio + Tina signal
  handlers do not coexist cleanly in one process"). For the
  smoke-test isolation, `cargo test -- --test-threads=1` is
  sufficient because each test fires its own SIGINT during its
  own run.

What this suggests:

- The signal-handler coexistence problem is real but narrow — it
  only affects examples that compare signal-handling shape directly.
  Documented; not a blocker for either runtime in production
  (you'd typically pick one or the other, not both).
- The per-isolate-completion observation handle would close several
  side-channel patterns at once: arrival logs, op correlators,
  drain-status telemetry. Worth tracking as an ergonomics frontier.
