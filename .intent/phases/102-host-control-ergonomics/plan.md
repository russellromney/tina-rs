# Phase 102 - Host Control Ergonomics

Status: In progress (2026-05-16). Phase 101 is merged. Code work is in
`tina-runtime` plus targeted specimen/docs migrations.

This is not a planning phase. Build the pinned host-control helpers below.

## Grug Truth

Host/test/control-plane code should not need a driver isolate, tracker isolate,
poll loop, or `Arc::try_unwrap(runtime)` dance for ordinary control work.

Tina truth still stays visible:

- a host call is still a normal Tina call;
- shutdown order is still service policy;
- `Full`, `Closed`, `Timeout`, stale address, and worker stopped stay typed;
- host-control command admission cannot block forever behind a full worker
  command queue;
- no helper hides drain ordering, retries, cancellation, or resource close.

## Ship

Ship two host-control surfaces and tighten one existing one:

1. `ThreadedMultiShardRuntime::call_blocking(...)`.
2. `ThreadedShutdownHandle` for threaded runtimes.
3. Existing `ThreadedRuntime::call_blocking(...)` gets the same bounded
   command-admission behavior as the multi-shard helper.

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

Add a host call helper for threaded multi-shard runtimes, and make the existing
single-shard host call obey the same command-admission rule.

Copied public shape:

```rust
runtime.call_blocking(addr, msg, timeout)
```

Return:

```rust
Result<CallOutcome<R>, ThreadedRuntimeError>
```

Behavior:

- route the temporary host-call driver to `addr.shard()`;
- admit the host-control command with a bounded/nonblocking command-queue path;
- issue the normal Tina `call(addr, msg, timeout)` from that driver;
- wait on the host thread for the same timeout;
- return `CallOutcome::{Replied, Full, Closed, Timeout, Rejected}`;
- return `ThreadedRuntimeError::CommandFull` if the worker command queue cannot
  accept the host-control command immediately;
- return `ThreadedRuntimeError::WorkerStopped` if the target shard worker is
  gone before the host call can be driven;
- panic on unknown shard, matching current `ThreadedMultiShardRuntime`
  programmer-error convention for host APIs such as `try_send` and
  `observe_result`;

Why no shard argument:

- existing host APIs route by address shard (`try_send`, `observe_result`);
- the copied path should do the same boring thing;
- host code should not pass the same shard twice and risk mismatches.

Do not ship a second explicit `*_on` / `*_from` variant in this phase. If a
future specimen needs "host call as if from shard A into target shard B," add
that later with a real caller and a remote-path proof.

Proof:

- same-shard call returns `Replied`;
- target mailbox full returns `CallOutcome::Full`;
- target stopped/stale generation returns `CallOutcome::Closed`;
- target rejects unsupported call shape returns `CallOutcome::Rejected`;
- callee holds caller past timeout and host returns `CallOutcome::Timeout`;
- full worker command queue returns `ThreadedRuntimeError::CommandFull` instead
  of blocking past the timeout;
- failed/stopped shard returns `ThreadedRuntimeError::WorkerStopped`;
- unknown address shard panics like `try_send` / `observe_result`;
- existing single-shard `ThreadedRuntime::call_blocking` also returns
  `ThreadedRuntimeError::CommandFull` when its worker command queue is full;
- no one-off tracker isolate remains in the migrated sharded specimen.

Compatibility:

- add `ThreadedRuntimeError::CommandFull`;
- this is a visible behavior improvement for existing single-shard
  `call_blocking`: full command queue becomes a typed error instead of a
  possible host hang;
- do not rename existing `ThreadedTrySendError::IngressFull` in this phase.

Migration:

- migrate `system_session_auth` to real `ThreadedMultiShardRuntime` placement
  for the host smoke path.
- If Phase 100 changes service handles first, use the new blessed handle shape,
  but still migrate `system_session_auth`.

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
shutdown.request_shutdown()?;
let report = shutdown.wait_report(timeout);
```

Add this to both `ThreadedRuntime` and `ThreadedMultiShardRuntime`.

Required public types:

- `ThreadedShutdownHandle`
- `ShutdownRequestError`
- `ShutdownWaitError`

The contract is pinned:

- handle is cloneable;
- `request_shutdown()` is idempotent and non-blocking;
- `request_shutdown()` reports immediate request-side failure instead of
  hanging behind a full command queue;
- `wait_report(timeout)` returns terminal truth;
- `wait_report(timeout)` does not request shutdown by itself;
- terminal report is cached and cloneable;
- multiple waiters get the same terminal report;
- timeout is visible as `ShutdownWaitError::Timeout`;
- failed/stopped workers are visible in the terminal report when a report can
  be produced;
- a broken wait path is visible as `ShutdownWaitError::WorkerStopped`;
- no sole ownership of `ThreadedRuntime` / `ThreadedMultiShardRuntime` is
  required by the caller.

Request shape:

```rust
Result<(), ShutdownRequestError>
```

`ShutdownRequestError` must distinguish at least:

- `CommandFull`;
- `WorkerStopped`.

For multi-shard runtimes, include the shard id in the error when one shard
cannot accept the request.

Wait shape:

```rust
Result<LocalSystemTerminalReport, ShutdownWaitError>
```

`ShutdownWaitError` must distinguish at least:

- `Timeout`;
- `WorkerStopped`.

Implementation rules:

- Refactor threaded runtime internals into one shared shutdown state that owns
  the worker join handles, terminal-report cache, and waiter notification.
- That shared state can be claimed by consuming `shutdown_report(self)`,
  `ThreadedShutdownHandle::wait_report`, or runtime `Drop`.
- Reuse existing shutdown command/report machinery.
- Do not create a second shutdown path with different trace/topology truth.
- Make `LocalSystemTerminalReport` cloneable if needed; its fields already carry
  cloneable/copyable runtime facts.
- Cache the terminal report after the first successful join.
- Dropping the handle must not silently shut down the runtime.
- Dropping the runtime requests shutdown and joins through the same shared state
  as today; if a handle later waits, it sees the cached report.
- Dropping the runtime after a handle already joined must not panic or attempt a
  second join.
- Calling consuming `shutdown_report(self)` after a handle requested shutdown
  but before any waiter joined must still return the normal terminal report.
- Calling consuming `shutdown_report(self)` after a handle already joined must
  return the cached terminal report.
- Calling `wait_report(timeout)` before any shutdown request returns
  `ShutdownWaitError::Timeout` while the runtime is still live.

Service policy rule:

- `request_shutdown()` asks the runtime/control plane to begin shutdown.
- It does not invent a service-specific drain order.
- Services that need graceful app drain still expose their own Stop/Drain
  message or Phase 101 `DrainState`.

Proof:

- request then wait returns a terminal report without consuming the original
  runtime handle first;
- request is idempotent;
- request on a full/stopped command path returns a typed request error and does
  not block forever;
- wait before request times out while the runtime remains live;
- wait timeout returns `ShutdownWaitError::Timeout`;
- two waiters get equal terminal reports;
- consuming `shutdown_report(self)` after a handle wait returns the same cached
  terminal report;
- dropping the runtime before a handle wait caches terminal truth for that
  handle;
- shutdown report includes retained trace/topology same as consuming
  `shutdown_report`;
- no `Arc::try_unwrap(runtime)` remains in the migrated system/specimen.

Migration:

- migrate `system_metrics_shipper`.
- It already uses Phase 101 `DrainState`, `RecurringTick`, and
  `LocalPermitGate`; keep those helpers and replace only the host/runtime
  shutdown ceremony.
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

- when to use host `call_blocking`;
- warning: do not call blocking host helpers from inside isolate handlers;
- shutdown handle contract;
- `request_shutdown()` must be called before `wait_report()` unless the runtime
  is being dropped elsewhere;
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
