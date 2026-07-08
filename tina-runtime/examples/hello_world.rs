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
