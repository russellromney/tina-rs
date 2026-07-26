# Effects And Runtime Calls

An effect is the only thing a handler gives back to the runtime.

Common effects:

```rust
noop()
send(addr, msg)
stop()
batch(vec![effect_a, effect_b])
spawn(child_definition)
```

Reply effects consume visible caller authority: use `call.reply(value)` for
the current call or `reply_to(request, value)` for a captured deferred request.

Runtime effects are for time, network, storage, process, DNS, TLS, and signals.

Use the runtime prelude from `tina_runtime`:

```rust
use tina::prelude::*;
use tina_runtime::{sleep, tcp_read, tcp_write, CallError, StreamId};
```

Runtime isolates usually use this macro:

```rust
#[tina_runtime::isolate(message = Msg, shard = AppShard)]
impl MyIsolate {
    fn handle(&mut self, msg: Msg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            Msg::Start => sleep(Duration::from_millis(10)).then(|result| match result {
                Ok(()) => Msg::TimerDone,
                Err(err) => Msg::TimerFailed(err),
            }),
            Msg::TimerDone => noop(),
            Msg::TimerFailed(_err) => stop(),
        }
    }
}
```

Important shape:

```text
Msg::Start
  -> return runtime call effect
runtime does call
runtime sends Msg::TimerDone or Msg::TimerFailed
```

The continuation is a normal message.

## Ordinary Continuations

Important grug truth:

Runtime calls produce ordinary continuation messages.

Use `then(...)` when you want work to complete and send one later message back
to the same isolate:

```rust
journal_append(self.path.clone(), request.bytes.clone())
    .then(|result| StoreMsg::Journaled(result, request))
```

`then(...)` does not reply to a caller and does not preserve caller authority.
It is the ordinary message-continuation vocabulary — use it when no caller is
being answered.

When a call handler must answer its caller after visible work, root the
expression at `CallContext`:

```rust
call_ctx
    .defer(journal_append(self.path.clone(), request.bytes.clone()))
    .then(StoreMsg::Journaled)
```

The continuation message still carries `RequestContext<StoreReply>`, and the
later handler turn must consume it with `reply_to(...)`. Tina does not
create a hidden async stack or a hidden final reply.

Grug warning:

- `then(...)` is ordinary continuation only.
- `call_ctx.defer(...).reply(...)` consumes caller authority into a visible
  `RequestContext`.
- If `handle_call` returns `then(...)` without consuming `CallContext`, the
  caller gets `ReplyAbandoned`; the ordinary continuation still runs.

## TCP Read Example

```rust
#[derive(Debug, Clone)]
enum ConnMsg {
    Begin,
    Read(Result<Vec<u8>, CallError>),
    Wrote(Result<usize, CallError>),
}

struct Conn {
    stream: StreamId,
}

#[tina_runtime::isolate(message = ConnMsg, shard = AppShard)]
impl Conn {
    fn handle(&mut self, msg: ConnMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ConnMsg::Begin => tcp_read(self.stream, 4096).then(ConnMsg::Read),
            ConnMsg::Read(Ok(bytes)) => tcp_write(self.stream, bytes).then(ConnMsg::Wrote),
            ConnMsg::Read(Err(_)) => stop(),
            ConnMsg::Wrote(_) => stop(),
        }
    }
}
```

Tina does not suspend `handle`.

Tina exits `handle`, runs the I/O outside the isolate, then later calls
`handle` again with the result.

## Batch

Use `batch` when one message wants many effects:

```rust
batch(vec![
    send(log, LogMsg::Started),
    sleep(Duration::from_secs(1)).then(|_| Msg::Tick),
])
```

Batch is useful. Batch can also make ergonomics feel clunky when many effect
types are involved. When that happens, write it down as a Tina paper cut.

## Timer Helpers

`sleep(delay).then(...)` is still the runtime truth. The small helpers in
`tina::time` only decide the delay and report visible state such as missed
ticks, retry attempt number, deadline caps, and exhausted attempts.

Pattern:

```text
helper decides delay
user returns sleep(delay).then(...)
continuation handles the result and records any user-visible outcome
```

Interval shape:

```rust
use tina::prelude::*;
use tina::time::{RecurringTick, RecurringTickDecision};
use tina_runtime::sleep;

match self.interval.next(ctx.now()) {
    RecurringTickDecision::Sleep { delay, token, report } => {
        let tick = report.tick_number;
        sleep(delay).then(move |reply| Msg::Tick(tick, token, reply))
    }
    RecurringTickDecision::Skip(report) => {
        self.missed_ticks += report.missed_ticks;
        noop()
    }
}
```

`self.interval` is a `RecurringTick` state. Carry the token in the
continuation message and call `self.interval.validate(token)` when it arrives;
a stale token means the schedule was re-armed or cleared underneath the
in-flight tick.

Backoff shape:

```rust
match self.backoff.next_delay_until(ctx.now(), self.deadline) {
    TimerDecision::Sleep(delay) => {
        sleep(delay.delay()).then(move |reply| Msg::Retry(delay.attempt(), reply))
    }
    TimerDecision::DeadlineElapsed | TimerDecision::Exhausted => reply(Msg::Failed),
}
```

No helper retries user work. No helper samples `Instant::now()`. If a deadline
matters, build it from runtime time with `ctx.deadline_after(...)` or
`Deadline::from_instant(ctx.now(), ...)`.

## No Async In Handler

This is expected to feel strange at first.

Tokio preserves stack-shaped code across awaits.

Tina preserves runtime control between effects.

You trade local linear code for explicit continuations and better runtime
accounting.
