//! Smallest runnable Tina program.
//!
//! One `Counter` isolate on a single-shard local system. Add a few numbers
//! with fire-and-forget sends, read the total back with a blocking call,
//! then shut the system down through the bounded terminal runner.
//!
//! Run with:
//! ```bash
//! cargo run --example hello_world -p tina-runtime
//! ```

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem};

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

fn main() -> anyhow::Result<()> {
    // Build the canonical live host: a single-shard local system with
    // fallible startup.
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;

    app.run_to_shutdown_reported(Duration::from_secs(5), |app| -> anyhow::Result<()> {
        // Register the isolate and get its typed address.
        let counter = app
            .register_root::<Counter, Infallible>(Counter::default(), 16)
            .map_err(|e| anyhow::anyhow!("register counter: {e:?}"))?;

        // Fire-and-forget sends. The worker delivers them in order.
        app.try_send(counter, CounterMsg::Add(2))
            .map_err(|e| anyhow::anyhow!("send add: {e:?}"))?;
        app.try_send(counter, CounterMsg::Add(3))
            .map_err(|e| anyhow::anyhow!("send add: {e:?}"))?;

        // Blocking call: ask for the total and wait for the reply.
        match app.call_blocking(counter, CounterMsg::Read, Duration::from_secs(1)) {
            Ok(CallOutcome::Replied(total)) => println!("counter total = {total}"),
            other => println!("unexpected outcome: {other:?}"),
        }
        Ok(())
    })?;

    // The runner above performed the bounded consuming shutdown and
    // checked the terminal report before returning.
    Ok(())
}
