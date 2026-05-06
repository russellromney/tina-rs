# Eiffel Supervised Worker

Paired Tokio-vs-Tina implementation of a worker that processes a queue of
jobs and panics on a "poison" job. The point of the comparison is not the
panic — it is what each side has to write to recover from the panic and
keep processing the next job.

The job script is fixed:

```text
Work(1)
Work(2)
Poison
Work(3)
Poison
Work(4)
Work(5)
```

Both sides emit the same numbers and the run is asserted in
`assert_equivalent`:

```text
processed=5 poisoned=2 restarts=2 exit_clean=true
```

Run both sides:

```bash
cargo run --manifest-path examples/eiffel_supervised_worker/Cargo.toml -- compare
```

Run one side:

```bash
cargo run --manifest-path examples/eiffel_supervised_worker/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_supervised_worker/Cargo.toml -- tina
```

(Two panic backtraces print to stderr per side. That is the worker dying;
the test harness still completes successfully.)

## What this comparison taught us

### Tokio side

- Recovery is hand-rolled. The pattern is: outer `loop`, `tokio::spawn` a
  worker task, `JoinHandle::await` it, on `JoinError::is_panic()` count a
  restart and respawn. None of this is in the `tokio` crate; it is just
  what every shop independently writes when they decide their service
  should not die from one bad message.
- Channel ownership becomes load-bearing. If the worker owns the receiver,
  the channel dies with the worker and the rest of the queue is lost. The
  fix is to wrap the receiver in `Arc<Mutex<_>>` so successive workers
  share it. That is a real foot-gun: the obvious shape silently drops
  work.
- Counters become load-bearing. The worker mutates state when it crashes.
  We share `Arc<AtomicU32>` so the supervisor outside can still read the
  numbers after the panic. With anything more complex than counters
  (e.g., a partial result needed for a reply), reconstructing post-panic
  state is the application's problem.
- The "shouldn't have happened" case is invisible. There is no Tokio
  primitive that says "this task was restarted N times" — every restart
  is something the supervisor in our code chose to do, with our own
  budget logic. Two shops will write two different versions of this
  loop.

### Tina side

What worked well:

- `RestartableChildDefinition::new(|| Worker { ... }, mailbox_capacity)`
  plus `runtime.supervise(parent, SupervisorConfig::new(OneForOne,
  RestartBudget::new(N)))` is the entire restart story. Two lines. The
  policy is named (`OneForOne`) and the budget is finite; both are
  visible in source and in the trace.
- The `RuntimeEventKind::SupervisorRestartTriggered { policy, ... }`
  event is in the trace. We assert the restart count from the trace, not
  from a counter we maintained ourselves. That is a real property: the
  runtime *knows* a restart happened, in its own observable timeline.
- `RestartableChildDefinition` takes a factory closure. Each restart
  rebuilds the worker from scratch — no implicit "carry-over" state.
  This is the right default for "did the worker get into a bad state?".
- `with_initial_message(|| WorkerMsg::Boot)` runs every time, so the new
  incarnation can re-publish its address into the shared slot exactly
  once per Boot. No surprise.

What was awkward or surprising:

- The driver thread still needs the *current* worker's address, so there
  is still a small `Arc<Mutex<Option<Address<...>>>>` slot for the child
  to publish its boot address. **Partly resolved in phase 047:** the
  old `AtomicU64` generation counter is gone; the driver now registers
  `runtime.observe_child_restarted(parent)` before sending a poison job.
- `ThreadedTrySendError` only carries `IngressFull` and `WorkerStopped`
  — no `Closed`. Sending to an address whose isolate has already died
  returns `Ok(())` from the ingress, and the runtime drops the message
  silently on the worker side. That is fine for "send to current
  worker", but it is a different shape from the explicit-step
  `Runtime::try_send`, which returns the message on `Closed`. Two
  similar APIs with different failure surfaces is the kind of thing
  `tina-runtime` users will trip on.
- `try_send` consumes the message even on `IngressFull`. To retry on
  full, the message must be `Copy` or hand-rebuilt. For a `Job` that's
  fine; for anything heavier, it is friction.
- ~~The four-line copy of `WorkerMailbox` + `WorkerMailboxFactory`
  boilerplate is here too.~~ **Resolved in phase 047:** the comparison
  uses `DefaultThreadedMailboxFactory`.
- ~~Driving the comparison requires `complete_trace()` polling to notice
  the next restart event.~~ **Resolved for this use in phase 047:**
  `observe_child_restarted(parent)` gives the driver a typed waiter for
  each restart. The trace is still used as audit truth for the final count.
- `runtime.supervise(...)` returns `Result<(), ThreadedRuntimeError>`
  while the inner explicit-step `Runtime::supervise(...)` returns `()`.
  The `.expect("supervise parent")` at every call site is small but
  asymmetric.

### Tokio shape vs. Tina shape, in one paragraph

Tokio gives you the primitives — spawn, JoinHandle, JoinError — and the
*policy* (when to restart, how many times, what to restart) is yours to
write, every time, slightly differently. Tina gives you the policy
(`OneForOne`, `RestartBudget`) and the *visibility* (a trace event the
runtime emits whether or not your code tracks it), and asks you to write
the worker like it might genuinely die. The Tokio version compiled in 30
seconds and is correct enough; it would be fragile in a production shop
without a careful review. The Tina version asserts its restart count
from the runtime trace, which is a property no amount of careful Tokio
code can offer.
