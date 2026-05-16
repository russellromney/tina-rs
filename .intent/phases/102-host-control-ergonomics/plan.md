# Phase 102 - Host Control Ergonomics

Status: Ready to implement. Can run in parallel with 094/100/101 because the
code work is in `tina-runtime` plus targeted specimen/docs migrations.

This is not a planning phase. Build the pinned host-control helpers below.

## Grug Truth

Host/test/control-plane code should not need a driver isolate, tracker isolate,
poll loop, or `Arc::try_unwrap(runtime)` dance for ordinary control work.

Tina truth still stays visible:

- a host call is still a normal Tina call;
- shutdown order is still service policy;
- `Full`, `Closed`, `Timeout`, stale address, and worker stopped stay typed;
- no helper hides drain ordering, retries, cancellation, or resource close.

## Ship

Ship two host-control surfaces:

1. `ThreadedMultiShardRuntime::call_blocking_on(...)`.
2. `ThreadedShutdownHandle` for threaded runtimes.

Also migrate one sharded system/specimen and one shutdown-heavy system/specimen.

## Do Not Ship

- No service-level shutdown framework.
- No hidden `Stop` messages to user isolates.
- No hidden drain ordering.
- No retry loop.
- No async runtime bridge.
- No natural-key pending helpers. That is a separate follow-up after 100/101.

## API Homes

- `tina-runtime`: all new runtime APIs and tests.
- `examples/systems`: only the proof migrations.
- docs/user guide: copied host-control examples.

No `tina` trait-crate changes in this phase.

## Rock 1 - Multi-Shard Host Call

Add a host call helper for threaded multi-shard runtimes.

Public shape:

```rust
runtime.call_blocking_on(shard, addr, msg, timeout)
```

Return:

```rust
Result<CallOutcome<R>, ThreadedRuntimeError>
```

Behavior:

- route the temporary host-call driver to `shard`;
- issue the normal Tina `call(addr, msg, timeout)` from that driver;
- wait on the host thread for the same timeout;
- return `CallOutcome::{Replied, Full, Closed, Timeout, Rejected}`;
- return `ThreadedRuntimeError::WorkerStopped` if the owning shard worker is
  gone;
- panic on unknown shard, matching current `ThreadedMultiShardRuntime`
  programmer-error convention for host APIs such as `try_send` and
  `observe_result`;

Address rule:

- `shard` must equal `addr.shard()`;
- mismatch panics with a clear message before registering the temporary driver;
- test this panic.

Do not infer shard silently in this phase. The explicit shard argument keeps
host tests honest and makes cross-shard intent visible.

Proof:

- same-shard call returns `Replied`;
- target mailbox full returns `CallOutcome::Full`;
- target stopped/stale generation returns `CallOutcome::Closed`;
- target rejects unsupported call shape returns `CallOutcome::Rejected`;
- callee holds caller past timeout and host returns `CallOutcome::Timeout`;
- failed/stopped shard returns `ThreadedRuntimeError::WorkerStopped`;
- shard/address mismatch is tested;
- no one-off tracker isolate remains in the migrated sharded specimen.

Migration:

- migrate `system_session_auth` to real `ThreadedMultiShardRuntime` placement
  for the host smoke path.
- If the implementation branch lands after Phase 100 changes service handles,
  use the new blessed handle shape, but still migrate `system_session_auth`.

## Rock 2 - Non-Consuming Shutdown Request Handle

Current threaded runtimes expose consuming shutdown:

```rust
runtime.shutdown_report()
```

That is correct for owned runtime values, but systems that share runtime handles
for host threads/tests can fall into `Arc::try_unwrap(runtime)` ceremony.

Add a request/wait shape that can be cloned/held separately from the runtime.

Public shape:

```rust
let shutdown = runtime.shutdown_handle();
shutdown.request_shutdown();
let report = shutdown.wait_report(timeout);
```

Add this to both `ThreadedRuntime` and `ThreadedMultiShardRuntime`.

Required public types:

- `ThreadedShutdownHandle`
- `ShutdownWaitError`

The contract is pinned:

- handle is cloneable;
- request is idempotent;
- `wait_report(timeout)` returns terminal truth;
- terminal report is cached and cloneable;
- multiple waiters get the same terminal report;
- timeout is visible as `ShutdownWaitError::Timeout`;
- worker stopped is visible as `ShutdownWaitError::WorkerStopped`;
- no sole ownership of `ThreadedRuntime` / `ThreadedMultiShardRuntime` is
  required by the caller.

Return shape:

```rust
Result<LocalSystemTerminalReport, ShutdownWaitError>
```

`ShutdownWaitError` must distinguish at least:

- `Timeout`;
- `WorkerStopped`.

Implementation rules:

- Refactor threaded runtime internals so the join handle can be claimed by
  either consuming `shutdown_report(self)` or `ThreadedShutdownHandle`.
- Reuse existing shutdown command/report machinery.
- Do not create a second shutdown path with different trace/topology truth.
- Make `LocalSystemTerminalReport` cloneable if needed; its fields already carry
  cloneable/copyable runtime facts.
- Cache the terminal report after the first successful join.
- Dropping the handle must not silently shut down the runtime.
- Dropping the runtime after a handle already joined must not panic or attempt a
  second join.
- Calling consuming `shutdown_report(self)` after a handle requested shutdown
  but before any waiter joined must still return the normal terminal report.
- Calling consuming `shutdown_report(self)` after a handle already joined must
  return the cached terminal report.

Service policy rule:

- `request_shutdown()` asks the runtime/control plane to begin shutdown.
- It does not invent a service-specific drain order.
- Services that need graceful app drain still expose their own Stop/Drain
  message or Phase 101 `DrainState`.

Proof:

- request then wait returns a terminal report without consuming the original
  runtime handle first;
- request is idempotent;
- wait timeout returns `ShutdownWaitError::Timeout`;
- two waiters get equal terminal reports;
- consuming `shutdown_report(self)` after a handle wait returns the same cached
  terminal report;
- shutdown report includes retained trace/topology same as consuming
  `shutdown_report`;
- no `Arc::try_unwrap(runtime)` remains in the migrated system/specimen.

Migration:

- migrate `system_metrics_shipper`.
- README must say the helper controls the runtime, not the service's internal
  drain policy.

## Rock 3 - Docs

Update:

- `docs/tina-user-guide/10-service-patterns.md`
- `docs/tina-user-guide/13-lifecycle-and-shutdown.md`
- `docs/tina-user-guide/11-ergonomics-checklist.md`
- relevant system README(s)
- `examples/FINDINGS.md` if the finding closes.

Docs must include:

- when to use host `call_blocking_on`;
- warning: do not call blocking host helpers from inside isolate handlers;
- shutdown handle contract;
- how service-level drain differs from runtime-level shutdown;
- the exact copied pattern for a sharded smoke test.

## Required Checks

Run focused checks:

- `cargo fmt --all --check`
- `cargo test -p tina-runtime`
- `cargo test -p tina-runtime --test threaded_call_blocking`
- any multi-shard runtime test touched
- migrated system/specimen `cargo test --manifest-path ...`
- touched docs doctests if new snippets compile
- clippy for touched crates/specimens with `-D warnings`

If a live test fails twice, treat it as a bug. Do not rerun until green by luck.

## Done Means

- Host code can call a service on a chosen live shard without a tracker isolate.
- Host code can request/wait runtime shutdown without `Arc::try_unwrap`.
- The helpers preserve normal Tina outcomes and terminal reports.
- At least one sharded specimen uses real multi-shard placement.
- At least one shutdown-heavy specimen uses the shutdown handle.
