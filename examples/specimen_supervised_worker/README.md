# specimen_supervised_worker

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
cargo run --manifest-path examples/specimen_supervised_worker/Cargo.toml -- both
cargo run --manifest-path examples/specimen_supervised_worker/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_supervised_worker/Cargo.toml -- tina
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
  `Job::Poison`; replies `Processed` only after a work job actually runs. The
  runtime catches the panic and settles that request as
  `Rejected(HandlerPanicked)`.
- **`Parent`** — spawns the worker as a `RestartableChildDefinition` and owns
  the current typed `ChildRef`. Each restart re-runs the factory closure and
  delivers the fresh ref back to the parent.
- **`runtime.try_supervise(parent, SupervisorConfig::new(OneForOne,
  RestartBudget::new(N)))`** — the budget is typed and finite. If
  the worker exceeds N restarts, the supervisor stops trying.
- **`spawn_observed(...).then_service_event_with_restarts(...)`** — the initial
  spawn result and every successful replacement arrive as ordinary bounded
  parent events. No shared address slot, manual reconstruction, or service
  envelope construction.
- **`runtime.observe_child_restarted(parent).wait(timeout)`** — the
  host's ground-truth synchronization signal per `Poison` job. It counts
  restarts but no longer acts as address authority.

The host starts the parent with a typed request, then calls through that parent
for each job. The parent forwards one typed request to its current worker and
maps `Replied`, `Full`, `Closed`, `Timeout`, and `Rejected` without collapsing
them. Report counters advance only after the exact expected worker outcome.
After a poison job the host waits for restart completion before sending the
next request; the replacement continuation is already ahead of that next
request in the parent's FIFO mailbox.

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
- **The parent receives typed replacements.** Application code never handles
  an untyped isolate id/generation pair and cannot accidentally stamp a
  replacement with the wrong system or shard.
- **The report proves worker outcomes.** Work is counted only after the worker
  replies, while poison is counted only after `HandlerPanicked`; it is not
  inferred from the input script or a fire-and-forget send.
- **The worker stays trivial.** No supervisor logic in the worker;
  no `catch_unwind`; no respawn loop. Tina's runtime catches the
  panic at the isolate boundary and reboots cleanly.

What feels worse:

- **The parent remains an explicit ingress hop.** That is intentional here:
  the parent is the authority that tracks child incarnations. Applications
  that hand a child ref directly to unrelated hosts must still define what
  makes those hosts learn a replacement.
- **Restart continuation delivery is bounded.** A full or stopped parent
  rejects it like any other message. Tina traces that terminal fact but does
  not hide it behind an unbounded lifecycle queue.

What this suggests:

- The supervisor + restart budget is the right shape. The Tokio
  hand-rolled loop is exactly the kind of thing that quietly
  diverges across codebases; Tina names it once.
- `then_service_event_with_restarts` keeps typed address ownership in the
  parent without creating a second observation registry, exposing the split
  service envelope, or asking the host to assert the child's message type.
- Join/stop-child convenience remains separate lifecycle work; this specimen
  no longer needs an address-side-channel workaround.
