# Lifecycle And Shutdown

Tina cares how things end.

Good shutdown tells the truth:

```text
all queues drained
no owned resources left
no pending runtime calls
no worker-held work
no hidden late replies
```

Bad shutdown hides work.

## Isolate Lifecycle

An isolate can be:

- registered
- running
- stopped
- panicked/failed
- restarted by supervision if configured

After an isolate stops, old addresses should reject future work as `Closed`.

## Resource Lifecycle

Runtime-owned resources have IDs:

- `ListenerId`
- `StreamId`
- file/path/persistence handles where applicable
- DNS/TLS/process/signal lane work

The isolate owns the ID as data. The runtime owns the actual OS/backend
resource.

Close should be explicit:

```rust
tcp_close_stream(stream).then(ConnMsg::Closed)
```

If close cancels pending work, the runtime should report that as resource-close
truth, not leave hidden in-flight calls around.

### Lifecycle Matrix

| Surface | Close admission | Close resource | Cancel | Drain | Terminal proof |
|---|---|---|---|---|---|
| isolate address | stopped isolate rejects sends/calls as `Closed` | isolate-owned IDs are closed by that isolate or runtime shutdown | `cancel_call(handle)` closes the caller wait | app protocol, not `wait_idle()` | `IsolateStopped`, typed result waiter, or later `CallOutcome::Closed` |
| `WorkerPool` | `WorkerPoolMsg::Close(Drain/Force)` | not owned by the pool unless the handle type says so | acquire waiter cancel reclaims waiter slot | `Drain` lets leases return; `Force` marks them stale | `WorkerPoolReply::Closed` and `PoolPressureReport { closed: true, ... }` |
| keepalive pool | `WorkerPoolMsg::Close` closes lease admission | `KeepaliveConnectionMsg::Stop` drops the connection transport and stops the isolate | request caller can stop waiting; accepted transport work may still finish late | `shutdown_keepalive_pool(..., Drain, ...)` waits for pool leases to return before stopping connections | `KeepalivePoolShutdownReport` with pool close, drain outcome, requested/stopped/timed-out/rejected/already-closed, and failed slot indexes |
| TCP/TLS stream | owner stops issuing new I/O for that ID | `tcp_close_stream` / `tls_close` effect or runtime shutdown cleanup | pending runtime call is cancelled/tombstoned; started backend work may complete late | owner protocol plus bounded shutdown | close reply, late-reply trace, or terminal remaining-resource report |
| HTTP body stream | connection stops pulling chunks | source releases buffers/files/calls on EOF/error/cancel | `ResponseChunkMsg::Cancel` tells source to release state | body metrics return to zero current bytes | `BodyPressureReport::drained()` plus IO/full/timeout counters |
| external bridge work | bridge closer stops Tina-side admission | remote resource is not Tina-owned unless bridge documents it | caller wait can close; remote work may continue unless explicit cancel exists | bridge-specific bounded drain | bridge metrics and runtime late-result trace |

## Pending Work

Pending work can live in several places:

- isolate mailbox
- cross-shard queue
- runtime call table
- backend lane
- worker thread or substrate-owned operation
- reply continuation waiting for delivery

Shutdown should account for these separately. One number is not enough.

## App Done

There is no blessed `runtime.wait_idle()`.

An app is "done" when the app says it is done. Put that truth in one
driver or coordinator isolate, let it own the terminal condition, and
finish with `stop_with(report)`.

```rust
let waiter = runtime.observe_result::<Report, _, _>(driver)?;
runtime.try_send(driver, DriverMsg::Begin)?;
let report = waiter.wait(timeout)?;
```

This is boring on purpose. The driver knows which mailboxes, calls,
timers, children, and bridges count. The runtime does not guess.

## Drain vs Stop

Stop means the isolate stops taking turns.

Drain means the runtime attempts to let already-started work settle within a
budget.

Do not confuse them.

Grug shape:

```text
stop isolate
close resources
wait bounded time for completions
report what remains
```

## Drain State Helper

For services that drain their own in-flight work (rather than just stop a
listener), `tina_runtime::DrainState` records the four-stage shape:

1. `Open` — admit new work.
2. `Draining` — `begin()` flips admission; new attempts return a typed
   `Stopping` outcome via `admit()`.
3. Settle outstanding work using local permits or pending sets the service
   already owns.
4. `Stopped` — `finish()` emits the final report. Late completions counted
   via `late_completions`; admission stays closed.

```rust
self.drain.begin();
self.tick.clear();
if self.drain.can_stop(self.gate.current() as u64) {
    self.drain.finish();
    return call.reply(Reply::Stopped { /* final report */ });
}
self.pending_stop = Some(call.into_request_context());
```

`DrainState` does not close resources, choose ordering, or hide messages.
It is small state plus a report. Resource close still belongs to the
service. `examples/systems/system_metrics_shipper` is the worked example
for a service that owns its own drain handshake and reaches `Stopped`;
`examples/systems/mini_saas_api` is the worked example for a host-driven
HTTP service where the controller publishes the typed `drain.stage` field in
its `/debug/capacity` report, the host closes the surrounding resources, and
the helper stays at `Draining` until the runtime tears the controller down.

## Service Shutdown Skeleton

`examples/systems/mini_saas_api` is the current R&D proof of this local-service
shutdown order:

1. Begin the controller `DrainState` so the next public request reads
   `drain.is_open() == false`.
2. Let an already-admitted notify request finish with a typed reply while
   later public work is rejected as `ingress_stopped`.
3. Probe `/ready` and surface `ingress_stopped`.
4. Probe `/debug/capacity` and surface `drain.stage=draining`.
5. Send one post-drain POST and prove the typed `503 ingress_stopped`
   response.
6. Close the SQLite bridge admission with its closer.
7. Probe `/ready` and surface `db_closed`.
8. Call `shutdown_keepalive_pool(..., CloseMode::Drain, ...)` for the outbound
   pool.
9. Stop the private notification listener.
10. Stop the public listener, then shutdown the runtime and inspect
    trace/capacity facts.

The controller never calls `drain.finish()`: the host owns terminal proof
through the runtime trace and the keepalive pool shutdown report, so the
controller's drain stays at `Draining` until the runtime tears it down.
That keeps host-driven services and service-owned drains visibly
different shapes — `system_metrics_shipper` is the worked example for the
service-owned form where the service answers a `Stop` call and reaches
`Stopped` itself.

The exact smoke command is:

```sh
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- smoke
```

The pressure variant holds the outbound keepalive lease and proves a second
notify request sees typed pool pressure:

```sh
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- pressure
```

## Timeout During Shutdown

Shutdown timeout is not "everything is fine".

If the deadline fires while work remains, the terminal report should say what
remains:

- pending runtime calls
- worker-held calls
- owned resources
- failed shards
- not-closed systems
- runtime errors

The deadline itself is a [`Deadline`] value (see
[ergonomics-checklist § Deadlines](11-ergonomics-checklist.md#deadlines)).
Build it from `ctx.deadline_after(budget)` and pass
`deadline.remaining_or_zero(ctx.now())` to each downstream call so the
shutdown budget shrinks honestly across hops. Cancellation is its own
primitive — see "Cancellation" below.

## Cancellation

Cancellation closes a *wait*, not the *work*. `cancel_call(handle)`
reclaims caller-side capacity and reports `CallCancelled { cause }` in
the trace; if the callee already accepted the work, it may still finish
and its late reply becomes a typed `CallReplyRejected` event. There is
no "kill this worker."

### Cancellation truth table

| Surface | Cancel can stop waiting? | Cancel can stop work? | Late result visible? |
|---|---|---|---|
| isolate call before delivery | yes | yes | no |
| isolate call after delivery | yes | no, unless callee cooperates | yes |
| deferred reply slot | yes | callee owns cleanup | yes |
| HTTP response body source | yes | yes — `ResponseChunkMsg::Cancel` tells source to release state | body metric + trace |
| SQLx bridge | yes | best-effort via `pg_cancel_backend` if enabled | metrics/trace |
| SQLite bridge | yes | no, blocking call runs to completion | metrics/trace |
| reqwest bridge | yes | maybe future abort handle; today be honest | trace/metrics |
| pool acquire waiter | yes | yes, reclaim waiter slot | no late work |

### Examples

**Isolate call cancel.** Call with `call_cancelable` so the caller
owns a [`CallHandle`]. Later, `cancel_call(handle)` reclaims the
waiter slot and records `CallCancelled` in the trace. If the callee
already accepted the work, the late reply becomes
`CallReplyRejected` — visible, not a ghost.

**Response streaming cancel.** If the client drops the connection
mid-stream, the connection isolate sends
[`ResponseChunkMsg::Cancel`](../../tina-http/src/streaming.rs) to the
body source. The source can release files, downstream calls, and
pending slots. `body_io_error_count` still increments so the
truncation is visible.

**SQLx best-effort DB cancel.** Opt-in via
`PgConfig::with_cancel_on_timeout`. When the bridge per-attempt
timeout fires, a sidecar pool fires `pg_cancel_backend(pid)`. Postgres
may or may not honor it, and a small race exists between cancel firing
and the connection returning to the pool. `db_cancels_sent` counts
attempts, not guaranteed query deaths.

**SQLite no-cancel late result.** `rusqlite` work runs on a blocking
std thread. If the caller times out, the worker thread runs to
completion. The terminal outcome is recorded and the dropped reply
shows up as `CallReplyRejected` in the trace, incrementing
`late_results`. No hidden work, no fake kill.

Owners that hold many in-flight calls should store the handles in a
bounded `PendingCallSet<K, R>` keyed by request id. The set rejects
duplicate keys loudly (it deliberately does **not** auto-sweep
settled handles, to avoid an ABA bug when a stale `Returned`
continuation can still fire); `sweep_terminal()` is the explicit
opt-in for foreground reclaim at known-safe points. See
[ergonomics-checklist § Bounded pending call handles](11-ergonomics-checklist.md#bounded-pending-call-handles)
for the shape.

Owner-stop already cancels every caller-owned pending call with cause
`OwnerStopped`; an explicit `drain` + `cancel_call` per entry is the
right shape when the owner needs the cancels acked back through its own
mailbox before stopping.

### Request-Scoped Cancellation

A request is a tree. When the request dies, its children should stop
waiting. The runtime primitive is [`RequestScope`]: a bounded child
registry plus a cancellation flag. Wire it into a service like this:

```text
let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
self.scopes.try_insert(request_id, scope.clone())?;

let admission = call_ctx
    .defer_scoped(&scope, "db_lookup", call_cancelable(db, query, t))
    .try_admit(&mut self.pending, request_id, Msg::DbReturned);

// Later, when the client disconnects or a per-request deadline fires:
let (report, cancel_effects) = scope.cancel_into_effects(
    ScopeCancelCause::ClientDisconnect,
    |id, label, outcome| Msg::ScopeChildCancelled { id, label, outcome },
);
return batch(cancel_effects);
```

What the scope can honestly do:

- Close Tina-owned waits (isolate calls, pool acquire, anything issued
  through `call_cancelable`).
- Reclaim caller-side capacity for those rails immediately.
- Provide a synchronous [`ScopeCancelReport`] listing every registered
  rail and its state at cancel time.

First cause wins. The first `cancel_into_effect(...)` sets the cause; a
second cancel keeps the original cause and returns an empty effect, so a
client disconnect that races a per-request timeout reports one cause, not
two. Wrap the cancel report, the post-removal
`RequestScopeSetCapacityReport`, and the late-result / ignored-timer
counts in a `ScopedRequestReport` — the request-level aggregate that says,
in one typed value: what cause, which children were cancelled vs already
settled, how much capacity came back, and any rails that could not be
scope-cancelled.

What the scope cannot do:

- Stop external work a bridge already accepted. A SQLx query that
  reached the database server, an HTTP request the reqwest pool has
  already sent, a SQLite blocking call mid-flight — these run to their
  own conclusion. Their replies become typed `late_results` /
  `CallReplyRejected` trace facts the same as for a plain cancel.
- Physically cancel a `sleep`. Plain `sleep` has no `CallHandle`. A
  per-request deadline uses a `ScopedTimerSet`: cancelling tombstones the
  ticket, and when the physical sleep fires later the continuation reads
  `ScopedTimerFire::IgnoredLate` and skips the user work. The ignored
  count is visible truth, not a pretended physical cancel.
- Cancel a rail that exposes no cancel handle. A buffered body already in
  the handler's hand, a fire-and-forget send — these are recorded as an
  `UnsupportedScopeRow` in the report, never pretended-cancelled. HTTP
  body pulls, WebSocket session operations, and response sources *do*
  have honest cancels through the `tina_http::scope` adapters
  (`scoped_request_body_pull`, `scoped_websocket_send`/`_report`/`_close`,
  `cancel_response_source`).

The bounded [`RequestScopeSet`] holds one scope per concurrent request:

```text
RequestScopeSet::with_capacity(max_requests)
  ├── try_insert(key, scope)        → Full / DuplicateKey returns the
  │                                    scope so you can answer Busy
  ├── remove(key)                   → free one slot after request done
  └── drain()                       → on owner stop, cancel each
                                      scope with ScopeCancelCause::OwnerStopped
```

Late completions from already-accepted bridge work flow through normal
trace facts. The service's contract to its caller is still honest: "we
stopped waiting; this child rail may still finish under the hood." See
[bridge crates § What ships today](18-bridge-crates.md#what-ships-today)
for the per-bridge late-result columns; scope cancellation does not
change any of those answers.

## What To Test

For any service with real I/O, test:

- clean request path
- close while read pending
- close while write pending
- caller timeout before callee reply
- destination mailbox full
- shutdown with no outstanding work
- shutdown with outstanding work

This is where many runtime bugs hide.

## Fallible Host Workloads

For an application-shaped owner with one fallible host workload, prefer the
consuming runner on `LocalSystem` (or the identical multi-shard facade):

```rust
let report = app.run_to_shutdown(Duration::from_secs(5), |app| {
    let service = app.register_request_service(MyService::new(), 64)?;
    let report = app.call_blocking_request(service, Request::Report, timeout)?;
    validate(report)?;
    Ok::<_, AppError>(report)
})?;
```

The closure makes early `?` safe: after success or failure, the owner requests
shutdown, uses one total budget for admission and terminal observation, and
requires an observed terminal report to prove clean. The budget does not cover
the workload itself. After the bounded attempt, consuming the owner does not
start a second blocking shutdown attempt. A timed-out worker may finish later;
an escaped shutdown handle can retry partial admission or observe the eventual
cached report. That escaped handle retains shutdown control and must eventually
retry or be dropped. Without one, owner consumption disconnects the remaining
control senders and makes no claim that terminal truth was observed.
`RunToShutdownError<E>` distinguishes
workload-only, shutdown-only, and dual failure without converting either error
to text. Its dual variant retains both typed values, and the `workload()` and
`shutdown()` accessors expose both source chains.

This runner does not replace an application's service-level drain protocol.
Drive `Stop` / `Drain` inside the closure when the service contract requires it;
the runner guarantees the bounded final runtime-owner shutdown attempt. A
workload panic still propagates as a panic. Unwinding uses the owner's existing
blocking `Drop` teardown because the runner disarms that path only after the
workload closure returns and its bounded shutdown attempt completes; panic
payloads are not converted into `RunToShutdownError`.

Without the runner, every honest host has to reproduce the same four-way merge:

```rust
let workload = run_application(&app);
let terminal = shutdown.request_and_wait_report(Duration::from_secs(5));
drop(app);
match (workload, terminal.and_then(require_clean)) {
    // success, workload failure, shutdown failure, both failures
}
```

Use a raw shutdown handle when another host thread controls lifetime, when the
owner is shared, or when shutdown request and terminal observation deliberately
happen in different parts of the program.

## Host Shutdown Handle

`ThreadedRuntime` and `ThreadedMultiShardRuntime` expose a cloneable
shutdown handle so host threads and tests can drive runtime teardown
without `Arc::try_unwrap(runtime)`:

```rust
let handle = runtime.shutdown_handle();
handle.request_shutdown()?;
let report = handle.wait_report(Duration::from_secs(5))?;
```

Pinned contract:

- `request_shutdown()` is **idempotent** and **non-blocking**. A full
  command queue surfaces as `ShutdownRequestError::CommandFull`; a
  worker that has already stopped surfaces as
  `ShutdownRequestError::WorkerStopped`. On multi-shard runtimes both
  variants name the offending shard.
- `wait_report(timeout)` **does not** request shutdown. While the
  runtime is still live and no one has requested or dropped shutdown,
  it returns `ShutdownWaitError::Timeout`. Call `request_shutdown()`
  first (or rely on the runtime owner's `Drop`).
- The terminal `LocalSystemTerminalReport` is **cached** after the
  first join. Every later waiter — including consuming
  `runtime.shutdown_report(self)` and a future `Drop` — gets the same
  cloned report.
- Dropping the handle does **not** trigger shutdown. The runtime owner
  controls lifetime.

This is **runtime-level** control. Service-level drain — the app's
own `Stop` / `Drain` protocol (see `DrainState`) — stays the service's
responsibility. The handle only asks the runtime/control plane to
begin shutdown.

## Substrate Question

Runtime people will ask: what wakes the loop, and what work can never be
preempted?

Answer honestly. Today Betelgeuse provides the portable live I/O substrate.
Some backend work may be started and later complete; Tina owns the visible
timeout, cancellation, tombstone, and shutdown accounting around it.
