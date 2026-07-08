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

This first sketch replies straight from `handle` to show the shape. A
fire-and-forget send has no caller to answer, though, so in the runnable
program at the end of the chapter `Read` replies from `handle_call` — where a
blocking `call` carries a caller. Same split, spelled out below.

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

## Run It

The pieces above show the shape. Here is a whole program that compiles and runs.

It uses `SingleShard`, the built-in shard, instead of a hand-written one. It
also adds a `handle_call` method: fire-and-forget sends land in `handle`, and a
blocking call from outside the runtime lands in `handle_call`, which replies.

`ThreadedRuntime` starts a worker thread that owns the shard. You register the
isolate, send it messages, ask it a question with `call_blocking`, then shut it
down.

```rust
//! Smallest runnable Tina program.
//!
//! One `Counter` isolate on a threaded single-shard runtime. Add a few numbers
//! with fire-and-forget sends, read the total back with a blocking call, then
//! shut the runtime down.
//!
//! Run with:
//! ```bash
//! cargo run --example hello_world -p tina-runtime
//! ```

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime};

#[derive(Debug, Default)]
struct Counter {
    value: u64,
}

#[derive(Debug, Clone, Copy)]
enum CounterMsg {
    Add(u64),
    Read,
}

#[tina::isolate(message = CounterMsg, reply = u64)]
impl Counter {
    // Fire-and-forget sends land here.
    fn handle(
        &mut self,
        msg: CounterMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CounterMsg::Add(n) => {
                self.value += n;
                noop()
            }
            CounterMsg::Read => noop(),
        }
    }

    // Blocking calls land here; `reply` answers the caller.
    fn handle_call(&mut self, msg: CounterMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            CounterMsg::Read => call.reply(self.value),
            CounterMsg::Add(n) => {
                self.value += n;
                call.reply(self.value)
            }
        }
    }
}

fn main() {
    // Start the worker thread that owns the shard.
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    // Register the isolate and get its typed address.
    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");

    // Fire-and-forget sends. The worker delivers them in order.
    runtime
        .try_send(counter, CounterMsg::Add(2))
        .expect("send add");
    runtime
        .try_send(counter, CounterMsg::Add(3))
        .expect("send add");

    // Blocking call: ask for the total and wait for the reply.
    match runtime.call_blocking(counter, CounterMsg::Read, Duration::from_secs(1)) {
        Ok(CallOutcome::Replied(total)) => println!("counter total = {total}"),
        other => println!("unexpected outcome: {other:?}"),
    }

    // Request shutdown and join the worker.
    runtime.shutdown().expect("clean shutdown");
}
```

Run it:

```bash
cargo run --example hello_world -p tina-runtime
```

Output:

```text
counter total = 5
```

This is the whole loop: start a runtime, register an isolate, talk to it, shut
it down. Everything else in this guide adds shape on top of it. The full source
lives in [`tina-runtime/examples/hello_world.rs`](../../tina-runtime/examples/hello_world.rs).
