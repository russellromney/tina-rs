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
tcp_close_stream(stream).reply(ConnMsg::Closed)
```

If close cancels pending work, the runtime should report that as resource-close
truth, not leave hidden in-flight calls around.

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

**Isolate call cancel.** Call with `call_with_handle` so the caller
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

## Substrate Question

Runtime people will ask: what wakes the loop, and what work can never be
preempted?

Answer honestly. Today Betelgeuse provides the portable live I/O substrate.
Some backend work may be started and later complete; Tina owns the visible
timeout, cancellation, tombstone, and shutdown accounting around it.
