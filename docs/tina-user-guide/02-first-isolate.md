# First Isolate

Start with plain state.

```rust
use tina::prelude::*;

#[derive(Debug, Default)]
struct Counter {
    value: u64,
}

#[derive(Debug, Clone, Copy)]
enum CounterMsg {
    Add(u64),
    Read,
}
```

Need a shard. A shard is the local execution owner.

```rust
#[derive(Debug, Default)]
struct AppShard;

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(1)
    }
}
```

Make the isolate.

```rust
#[tina::isolate(message = CounterMsg, reply = u64, shard = AppShard)]
impl Counter {
    fn handle(&mut self, msg: CounterMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            CounterMsg::Add(n) => {
                self.value += n;
                noop()
            }
            CounterMsg::Read => reply(self.value),
        }
    }
}
```

That is the core.

`Add` mutates owned state.

`Read` replies.

No task. No async trait. No shared state.

## Effect Types

The macro needs to know what the isolate may do.

Smallest:

```rust
#[tina::isolate(message = Msg, shard = AppShard)]
```

Can reply:

```rust
#[tina::isolate(message = Msg, reply = Reply, shard = AppShard)]
```

Can send one kind of outbound message:

```rust
#[tina::isolate(message = Msg, send = Outbound<OtherMsg>, shard = AppShard)]
```

Can spawn children:

```rust
#[tina::isolate(message = Msg, spawn = ChildDefinition<Child>, shard = AppShard)]
```

Can restart children:

```rust
#[tina::isolate(message = Msg, spawn = RestartableChildDefinition<Child>, shard = AppShard)]
```

Runtime I/O should use `tina_runtime::isolate`, covered later.

## Addresses

An `Address<Msg>` is where messages go.

Store addresses in state when the isolate needs to talk to another isolate:

```rust
struct Worker {
    log: Address<LogMsg>,
}
```

Then send:

```rust
send(self.log, LogMsg::SawWork)
```

Simple send is fire-and-forget. For overload-sensitive code, prefer
`send_observed`, covered in boundedness.

## Grug Test

Can you explain the isolate in one sentence?

Good:

```text
Counter owns one number and replies with it.
```

Bad:

```text
Counter coordinates async streams, shared caches, retry queues, background
workers, and cancellation.
```

Split the bad one.
