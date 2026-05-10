# specimen_cancellation_chain

Mid-flight cancellation of a fan-out. The driver dispatches `FANOUT`
slow worker calls (each takes `WORK_MS` ≫ `CANCEL_AFTER_MS`), then
the host asks for cancellation before any worker has finished.

## What this teaches

Tina has no public *external* cancellation primitive. There is no
`runtime.cancel(addr)` and no public `IsolateCall::abort()`. The
closest thing is "send a `Stop` message to the requester isolate,
which causes it to stop itself." Stopping the requester closes its
pending IsolateCalls; later worker replies are rejected by the
runtime as `CallReplyRejected { RequesterClosed }` and never reach
the handler.

Tokio: `JoinSet::abort_all()`. Preempts at the next await boundary;
aborted tasks never deliver.

Tina: the `Stop` envelope on `DriverMsg`, plus
`runtime.try_send(driver, DriverMsg::Stop)` from the host. The
driver's `Stop` arm ends with `stop_with(report)` and the host
reads the typed `Report` through
`runtime.observe_result::<Report>(driver)` — no
`Arc<Mutex<Option<Report>>>` side channel.

## Run

```sh
cargo run --manifest-path examples/specimen_cancellation_chain/Cargo.toml -- both
cargo test --manifest-path examples/specimen_cancellation_chain/Cargo.toml
```

## What feels worse

- **Cancellation is a domain message, not a runtime verb.** Every
  isolate that wants to be externally cancellable must add its own
  `Stop` (or equivalent) variant. That's fine for an isolate that
  already has a domain shutdown. It is verbose for libraries that
  want generic mid-flight cancellation.
- **No handle for "abort just these N calls."** The driver has to
  stop *itself* — there is no way to keep the driver alive but
  abandon its currently in-flight IsolateCalls.

## What feels better

- **Late replies are visibly accounted for.** Every worker reply
  that arrives after the driver stops produces a typed
  `CallReplyRejected { RequesterClosed }` event in the trace. There
  is no silent task-leak.
- **Resource cleanup is the same path as graceful exit.** Whatever
  cleanup runs when the driver stops naturally also runs here.

## Findings touched

- See FINDINGS finding 8 (external cancellation API).
