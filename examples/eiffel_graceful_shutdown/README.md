# Eiffel Graceful Shutdown

Paired Tokio-vs-Tina implementation of "the operator hits Ctrl-C while
the service has work in flight, and the service exits cleanly without
dropping anything."

The shape is the same on both sides:

```text
- a Producer pushes work items at 5 ms intervals (up to TOTAL_PLANNED_ITEMS)
- a Consumer drains the queue and counts each item
- after SIGNAL_AFTER_MS, an "operator" raises SIGINT to the process itself
- the Producer stops scheduling new items
- the Consumer drains whatever was already produced
- the report names: items_produced, items_processed, signal_received,
  remaining_in_queue_at_exit, exit_clean
```

To keep the Tokio and Tina signal-handler installations from contaminating
each other, the `compare` mode spawns each side as a separate subprocess
and parses the machine-readable report it prints.

Both sides assert the same properties:

```text
signal_received=true
items_remaining_in_queue_at_exit=0
items_produced == items_processed   (whatever was produced was drained)
0 < items_produced <= TOTAL_PLANNED_ITEMS  (signal stopped the producer)
exit_clean=true
```

Run both sides:

```bash
cargo run --manifest-path examples/eiffel_graceful_shutdown/Cargo.toml -- compare
```

Run one side:

```bash
cargo run --manifest-path examples/eiffel_graceful_shutdown/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_graceful_shutdown/Cargo.toml -- tina
```

## What this comparison taught us

### Tokio side

- `tokio::signal::ctrl_c()` is the affordance, but using it inside
  `select!` requires `tokio::pin!(signal_stream)` to satisfy the
  `Unpin` requirement on the future. Easy fix once you've seen it;
  the diagnostic does name the right knob. First-time readers will
  pause.
- `select!` is the Tokio idiom for "wait for either work or
  shutdown". It is correct, succinct, and *invisible*: there is no
  trace of "we cancelled the timer because shutdown won the race."
  If a peer reviewer wants to know in what order things happened,
  the answer is "the scheduler decided".
- The drain-on-shutdown shape is "drop the sender, let the consumer
  run until `recv` returns `None`". This works perfectly here, but
  it depends on every producer respecting "if the channel send
  fails, exit" semantics. A Tokio service with three producers
  needs three places to remember to drop their senders, plus a
  watchdog that drops the *last* one when shutdown begins. None of
  that is enforced by the compiler.
- No "shutdown report". When the runtime shuts down, what was in
  flight, when, in what order, against which task — gone. Whatever
  the application was tracking via shared atomics is what you have.

### Tina side

What worked well:

- `signal_wait("sigint", timeout).reply(SignalMsg::Received)` is the
  whole signal story at the user-code surface. The runtime owns
  the OS-level installation; the isolate just receives a typed
  reply when the signal arrives or the timeout elapses. The
  `Result<String, CallError>` reply names *which* signal fired
  ("sigint" vs "sigterm"), so a single watcher could distinguish
  graceful from forced shutdown.
- The shape is three small isolates: `Producer`, `Consumer`,
  `SignalWatcher`. The watcher's only job is to translate "signal
  arrived" into `send(producer, ProducerMsg::Stop)`. Everything else
  is unaware of signals; the producer just learns it should stop.
  Decoupling shutdown-trigger from shutdown-effect is one of the
  cleaner properties to fall out of the model.
- The `Producer::Stop` arm flips a `bool` and sets a shared atomic.
  When the next `Tick` or `TimerFired` arrives, the producer sees
  `self.stopped` and returns `noop()`. *No cancellation of the
  outstanding timer is needed* — the timer fires, the handler sees
  the flag, and emits no further effects. State machines absorb
  shutdown the way they absorb every other event: a match arm.
- `signal_hook::low_level::raise(SIGINT)` from a worker thread is
  caught by the runtime's signal-hook registration and delivered
  through the existing `signal_wait` machinery. Same code path as
  a real Ctrl-C from a terminal.

What was awkward or surprising:

- Three isolates plus shared `Telemetry` (`Arc<Telemetry>` with four
  atomics) for a service that, on the Tokio side, fits in a
  single `tokio::spawn` plus a `select!`. The Tokio version is
  ~25 lines of business logic; the Tina version is closer to ~80.
  This is the same "more parts, more visible" trade as the other
  comparisons, applied to a domain where the Tokio shape is genuinely
  near-optimal.
- "Wait for the runtime to be drained" still requires polling shared
  atomics from the driver thread. The pattern is now showing up in
  every comparison: chat (slow consumer counts), keyspace (trace
  poll for `TcpStreamClose`), supervised worker (slot generation),
  persistent counter (op id), outbound fetch (`done` AtomicBool),
  and now this comparison's "produced == processed && producer
  stopped" three-fact check. There should be a public Tina API
  surface for "this isolate finished" or "the runtime has nothing
  left to do".
- `TimerFired(u32, Result<(), CallError>)` continuations carry a
  `Result<(), CallError>` we generally do not care about (the timer
  effectively never fails on healthy systems). Every Tina handler
  that uses `sleep(...).reply(...)` ends up with this dead-error
  branch. A `sleep(...).reply_ignore_error(...)` shorthand or a
  `Result<(), Infallible>`-style narrower outcome would be a real
  ergonomics win.
- `runtime.shutdown_report()` exists and would be the natural place
  to find "what was in flight at shutdown". This example does not
  use it because we already track `produced - processed` via
  telemetry; mentioning it here so readers know it is the path to
  an even cleaner report than what the Tokio side can offer.
- Running both sides in one process does not work cleanly: the
  signal handlers chain, and a SIGINT raised during the Tina run
  also wakes the (now-dead) Tokio handler chain. The comparison
  binary works around this by spawning each side as a subprocess.
  Worth a public note: Tina + Tokio in one program both want to
  own SIGINT, and they coexist mostly-harmlessly via signal-hook,
  but for clean tests, separate processes are the right answer.

### Tokio shape vs. Tina shape, in one paragraph

This is the comparison where Tokio looks the best on a first pass:
`select! { _ = sleep(...) => ... res = ctrl_c => ... }` is exactly
the right shape for "do work or shutdown, whichever arrives first",
and the drop-the-sender drain pattern is correct and short. The
Tina side has more parts. What it offers in exchange is that every
shutdown trigger is a typed `signal_wait` reply, every shutdown
*effect* is a normal message the producer chooses to handle, and
the runtime's trace knows when each step happened. The pitch isn't
"this service is shorter in Tina" — it isn't — it's "every part of
this service is observable in Tina, including the parts that
involve the kernel."
