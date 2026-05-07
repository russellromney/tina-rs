# Effects And Runtime Calls

An effect is the only thing a handler gives back to the runtime.

Common effects:

```rust
noop()
send(addr, msg)
reply(value)
stop()
batch(vec![effect_a, effect_b])
spawn(child_definition)
```

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
            Msg::Start => sleep(Duration::from_millis(10)).reply(|result| match result {
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

## Continuation Carries Call Context

Important grug truth:

`reply(...)` is not only for the first handler turn.

If an isolate is handling a request/reply call, and it returns a runtime call
with `.reply(...)`, Tina carries the original reply context through that
continuation chain.

That means a service can do this:

```text
caller calls store
store starts journal_append runtime call
runtime later sends store Journaled
store replies to original caller
```

Shape:

```rust
match msg {
    StoreMsg::Store(request) => {
        journal_append(self.path.clone(), request.bytes.clone())
            .reply(|result| StoreMsg::Journaled(result, request))
    }
    StoreMsg::Journaled(Ok(()), request) => {
        self.apply(request);
        reply(StoreReply::Stored)
    }
    StoreMsg::Journaled(Err(_), _request) => {
        reply(StoreReply::Failed)
    }
}
```

This is why service-shaped clients work:

```rust
call(http_client, OutboundCall { target, request }, timeout)
    .reply(MyMsg::HttpReturned)
```

The `http_client` isolate may need many TCP turns before it answers. That is
fine if those turns are built as runtime-call continuations. The original caller
still receives the final reply or the call times out.

Grug warning:

- Context is carried by Tina continuation machinery.
- Do not stash arbitrary reply handles in side channels.
- Do not spawn a separate child just to route one reply back unless topology
  really needs that.
- Always keep the caller timeout honest.

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
            ConnMsg::Begin => tcp_read(self.stream, 4096).reply(ConnMsg::Read),
            ConnMsg::Read(Ok(bytes)) => tcp_write(self.stream, bytes).reply(ConnMsg::Wrote),
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
    sleep(Duration::from_secs(1)).reply(|_| Msg::Tick),
])
```

Batch is useful. Batch can also make ergonomics feel clunky when many effect
types are involved. When that happens, write it down as a Tina paper cut.

## No Async In Handler

This is expected to feel strange at first.

Tokio preserves stack-shaped code across awaits.

Tina preserves runtime control between effects.

You trade local linear code for explicit continuations and better runtime
accounting.
