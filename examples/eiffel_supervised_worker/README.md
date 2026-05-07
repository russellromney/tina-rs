# eiffel_supervised_worker

A worker that processes a fixed job script and panics on every
`Job::Poison`. The comparison is not the panic — it's what each side
has to write to recover from the panic and keep processing the next
job.

```text
Work(1)
Work(2)
Poison    <- worker panics here
Work(3)
Poison    <- and again here
Work(4)
Work(5)
```

Both sides should produce:

```text
processed=5 poisoned=2 restarts=2 exit_clean=true
```

Two panic messages print to stderr per side. That's the worker
genuinely dying — the comparison is whether the supervisor (Tokio:
hand-rolled; Tina: built-in) brings it back cleanly.

## Run

```sh
cargo run --manifest-path examples/eiffel_supervised_worker/Cargo.toml -- both
cargo run --manifest-path examples/eiffel_supervised_worker/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_supervised_worker/Cargo.toml -- tina
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

Both files are self-contained — supervisor loop, worker, accounting.

## Tokio shape

A hand-rolled supervisor: `tokio::spawn` the worker, `await` its
`JoinHandle`, check `is_panic()`, respawn with the same channel and
the same shared counters. The state that survives the panic
(`Arc<Mutex<Receiver>>`, `Arc<AtomicU32>` counters) lives in the
supervisor scope, not the worker scope, because anything in the
worker scope dies with the worker.

You write the policy. There is no notion of a *budget* — if the
worker panics in a loop, this code respawns forever.

## Tina shape

A worker, a parent, and a supervisor config:

- **`Worker`** — owns no state worth preserving. Panics on
  `Job::Poison`. The runtime catches the panic.
- **`Parent`** — spawns the worker as a `RestartableChildDefinition`
  with an initial `WorkerMsg::Boot`. Each restart re-runs the
  factory closure to produce a fresh `Worker`.
- **`runtime.try_supervise(parent, SupervisorConfig::new(OneForOne,
  RestartBudget::new(N)))`** — the budget is typed and finite. If
  the worker exceeds N restarts, the supervisor stops trying.
- **`runtime.observe_child_restarted(parent).wait(timeout)`** — the
  ground-truth signal that a restart happened, used per-`Poison`
  job. No `AtomicU64` generation counter, no trace polling.

The host loops over the script: send a job; if it was a `Poison`,
wait for the restart waiter to fire and re-read the worker's address
from a one-shot publish slot.

## Discussion

What feels better:

- **Restart budget is typed and finite.** `RestartBudget::new(N)`
  caps the supervisor. Tokio's hand-rolled loop has no equivalent —
  you'd add a counter, but every codebase reinvents it slightly
  differently.
- **`OneForOne` is a name, not a recipe.** The policy lives on the
  supervisor config, separate from the loop body. In Tokio you
  re-discover what "one-for-one" means at every site.
- **`observe_child_restarted` is ground truth.** The host waits for
  a typed event the runtime emits. No "did it work?" guessing, no
  trace polling, no atomic generation counters.
- **The worker stays trivial.** No supervisor logic in the worker;
  no `catch_unwind`; no respawn loop. Tina's runtime catches the
  panic at the isolate boundary and reboots cleanly.

What feels worse:

- **The "first worker address" still uses a side channel.**
  `Arc<Mutex<Option<WorkerAddr>>>` because Tina doesn't yet ship an
  observe-child-spawned waiter for the *initial* spawn. Each restart
  *publishes* through the same slot when its `Boot` message fires.
  `FINDINGS.md` tracks the broader typed-result / observation gap.
- **`send_until_accepted` is a manual ingress-full retry loop.**
  `runtime.try_send` returns `IngressFull` when the worker's mailbox
  ingress is saturated; we yield + retry. Bounded inboxes mean this
  shape is real, but every supervisor will write it the same way —
  a small "send-with-retry" helper would be welcome.
- **Wait for "next worker boot" after restart.** Same shape as the
  initial wait, polling `slot.current()` for a *different* address.
  Same root cause: no child-spawned waiter yet.

What this suggests:

- The supervisor + restart budget is the right shape. The Tokio
  hand-rolled loop is exactly the kind of thing that quietly
  diverges across codebases; Tina names it once.
- The next ergonomics win for supervised systems is a
  child-spawned / first-boot observation handle. Once that lands,
  the `WorkerSlot` side channel and both `wait_until` calls can go.
- A higher-level "send with bounded backoff" would retire
  `send_until_accepted` across every example that touches a bounded
  ingress.
