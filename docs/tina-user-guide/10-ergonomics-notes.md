# Ergonomics Notes

This page is a scratchpad for Tina paper cuts found while porting.

Use it like this:

```text
comparison: eiffel_real_io_chat
pain: observed send burst requires nested batch and outcome message
why it matters: common fanout path is noisy
possible fix: helper for collect observed outcomes
```

## Known Sharp Edges

### Continuation Enum Growth

Every runtime call wants a reply message.

This is explicit, but enums grow fast:

```rust
Read(Result<Vec<u8>, CallError>)
Wrote(Result<usize, CallError>)
Closed(Result<(), CallError>)
TimerDone(Result<(), CallError>)
```

Possible improvement:

- typed helper aliases
- smaller result wrappers
- generated continuation names

### Batch Verbosity

Fanout often wants:

```rust
batch(items.map(|item| send_observed(...).reply(...)).collect())
```

This is honest but noisy.

Possible improvement:

- `batch_iter`
- `send_all_observed`
- bounded fanout helper with summary reply

### TCP State Machines

Connection handlers become explicit state machines.

That is good for control. It can be rough for simple echo/chat examples.

Possible improvement:

- protocol loop helper
- framed TCP helper
- write-all helper
- close-on-error helper

### Runtime Config Budget Surface

Capacities are powerful but many.

Possible improvement:

- small preset configs
- named overload profiles
- manifest dump explaining every capacity

### Error Names

`CallError`, `CallOutcome`, `SendOutcome`, `TrySendError` are close but not the
same.

Possible improvement:

- guide table
- method naming consistency
- examples showing each failure path

## Add Notes Here

Keep notes blunt.

Template:

```text
date:
comparison:
tokio shape:
tina shape:
pain:
good:
possible fix:
```

The point is not to look polished.

The point is to find where Tina is wrong, half-formed, or better than expected.

---

```text
date: 2026-05-06
comparison: eiffel_mini_keyspace
tokio shape: TcpListener accept, BTreeMap on the task stack, sequential
  read_to_end + write_all
tina shape: Listener isolate that tcp_binds and spawns Connection isolate per
  accept; Connection drives a state machine Begin → Read → (StoreReturned)*
  → Wrote → Closed; Store isolate owns the BTreeMap and replies via
  call(...).reply(...)
pain: needed a hand-rolled next_effect() helper because there is no "process
  this list" effect; bound SocketAddr smuggled out via Arc<Mutex<Option<...>>>;
  shutdown polled complete_trace() for a TcpStreamClose event; mailbox capacities
  load-bearing without compile-time hint
good: store ownership genuinely enforced; call/reply continuation is honest;
  state-machine match arms read well after written
possible fix: default in-process MailboxFactory; bind reply that exposes
  SocketAddr without a side channel; "wait for isolate to stop" handle;
  iteration combinator
```

```text
date: 2026-05-06
comparison: eiffel_axum_counter
tokio shape: Arc<Mutex<u64>> behind axum::State, two handlers, four lines each
tina shape: Counter isolate consuming BridgeRequest<CounterRequest, CounterReply>
  via tina-tokio-bridge; BridgeHandle::new dropped straight into
  Router::with_state; bridge.call(req).await in the handlers
pain: BridgeMailbox+BridgeMailboxFactory boilerplate (third copy);
  ThreadedRuntime + BridgeHandle wiring is more setup than the entire Tokio
  side; bridge service stacks two runtimes (Tina thread + Tokio
  current_thread); shutdown requires Arc::try_unwrap dance
good: BridgeError::{Full,Closed,Timeout} surface as real handler error
  variants — HTTP-shaped pushback is genuinely visible at the call site;
  composing axum::State with BridgeHandle is a strong story
possible fix: one-call shutdown on BridgeHost / BridgeHandle; default
  in-process MailboxFactory; built-in single-shard so shard = ... is optional
```
