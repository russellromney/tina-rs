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

## `CallError`

Runtime-owned I/O calls return `Result<T, CallError>`.

Examples:

```rust
tcp_read(stream, 4096).reply(ConnMsg::Read)
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

## Rule

Do not flatten these too early.

`Full`, `Closed`, `Timeout`, and I/O failure mean different things. Good Tina
services keep that truth until the boundary where they intentionally map it to
HTTP status, RPC error, metric, or shutdown report.
