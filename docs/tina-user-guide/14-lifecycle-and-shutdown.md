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
