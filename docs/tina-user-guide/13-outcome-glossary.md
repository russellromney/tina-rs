# Outcome Glossary

Tina has several result words. They are close, but not the same.

## `Full`

The destination has no capacity right now.

Grug meaning:

```text
queue full
work not accepted
caller still alive
try later or shed load
```

This is good overload. It is not a crash.

## `Closed`

The destination is gone, stopped, failed, or no longer accepts that operation.

Grug meaning:

```text
address/resource dead
work not accepted
retry only if you have a new target
```

## `Timeout`

The caller waited until its deadline and no answer arrived.

Timeout does not always mean the callee stopped working. It means the caller
stopped waiting.

Late replies should be discarded visibly by the runtime.

## `CallOutcome<T>`

Result of isolate request/reply:

```text
Replied(T)
Full
Closed
Timeout
Rejected(reason)
```

Use this when one isolate calls another isolate.

## `SendOutcome`

Result of observed send:

```text
Accepted
Full
Closed
```

Use this when you need to know whether a fire-and-forget message was accepted.

Plain `send(...)` is simpler but does not report pressure back to the sender.

## `AcquireOutcome<H>` / `ReleaseOutcome`

Result of `tina_runtime::pool::WorkerPool` calls.

```text
AcquireOutcome::Acquired(PoolLease<H>)
AcquireOutcome::Full         // resources busy + waiter table full
AcquireOutcome::Closed       // pool closed
AcquireOutcome::WrongShard   // caller on a different shard
```

```text
ReleaseOutcome::Released      // resource returned to idle (or next waiter)
ReleaseOutcome::Retired       // caller asked Retire, or pool override
ReleaseOutcome::StaleLease    // wrong pool / wrong resource / wrong gen
ReleaseOutcome::DoubleRelease // (resource_id, generation) already returned
ReleaseOutcome::PoolClosed    // pool was force-closed
```

`AcquireFailure` / `ReleaseFailure` are the flat results returned by
`try_acquired(call_outcome)` / `try_released(call_outcome)` — they
fold the pool-layer outcomes above with the transport-layer
`CallOutcome` (`CallTimeout`, `CallFull`, `CallClosed`,
`WrongReply`) into one typed `Err` enum so consumers don't need a
three-layer match. Pool-layer and transport-layer variants stay
distinct: a `Full` from the pool means "all resources busy + waiter
cap"; a `CallFull` means "the pool's own mailbox refused the call."

## `CallError`

Runtime-owned I/O calls return `Result<T, CallError>`.

Examples:

```rust
tcp_read(stream, 4096).then(ConnMsg::Read)
```

The continuation sees:

```rust
ConnMsg::Read(Result<Vec<u8>, CallError>)
```

`CallError` is for runtime operations like TCP, file, DNS, TLS, process, timer,
and shutdown rails. It is not the same as `CallOutcome`.

## Which One Do I Use?

| Situation | Use |
| --- | --- |
| call another isolate and expect a reply | `CallOutcome<T>` |
| send a message and care about queue pressure | `SendOutcome` |
| receive result of runtime I/O | `Result<T, CallError>` |
| call times out locally | `CallOutcome::Timeout` |
| TCP read fails | `Err(CallError)` |
| mailbox cannot accept work | `Full` |
| target/resource is gone | `Closed` |
| acquire from a `WorkerPool` | `AcquireOutcome<H>` (or `try_acquired`) |
| release a pool lease | `ReleaseOutcome` (or `try_released`) |
| many callers wait on one key | `SharedWork<K, R>` (`Full` / `KeyFull`) |
| one active cancelable request per key | `PendingCancelableCallSet<K, Q, R>` (`Full` / `DuplicateKey`) |
| many cancelable attempts grouped by key | `CancelableWork<K, Q, R>` (`Full` / `KeyFull`) |
| reply later to current caller | `call.defer(effect).reply(...)` |

## Project Vocabulary

A few words recur across these docs and the code. They are Tina jargon, not
standard Rust terms.

### rail

A runtime-owned lane for one class of work: a TCP rail, file rail, DNS rail,
TLS rail, storage rail, timer rail. Effects like `tcp_read` or `journal_append`
flow through a rail and return as ordinary continuation messages. Rails are
bounded and report their own pressure.

### first-form

The first shipped version of a feature: proven and honest, but minimal. A
first-form implementation does the core job and states what it does not do yet,
instead of pretending to be the hardened final version. "No transactions in
first form" means the feature works, but that capability is not there yet.

### fact

A typed value the runtime records instead of leaving behavior implicit.
Overload, timeout, cancellation, a stopped child, capacity, a replayed run —
each is a fact in the trace or a typed return, not a log line or a convention.
Facts are the thing you assert on.

### battery

A blessed-but-optional crate on top of core Tina: HTTP, gRPC, SQLite, AWS, and
the bridge crates. Batteries depend on core, never the other way. You can learn
and use core Tina without reaching for any battery.

### specimen

A paired Tokio-vs-Tina example of one service shape, kept for comparing shape
and behavior. Specimens are not a shared framework and not a release artifact;
they exist to find rough edges. They live under `examples/`.

## Rule

Do not flatten these too early.

`Full`, `Closed`, `Timeout`, and I/O failure mean different things. Good Tina
services keep that truth until the boundary where they intentionally map it to
HTTP status, RPC error, metric, or shutdown report.
