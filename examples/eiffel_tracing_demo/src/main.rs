//! Tina runtime trace -> tracing fmt subscriber, live.
//!
//! Single-shard explicit-step runtime with a [`TracingObserver`]
//! attached. Every recorded event becomes a structured
//! `tracing::Event` as it happens — no end-of-run dump needed.
//!
//! The caller fans out six zero-duration `sleep` calls into a
//! one-slot mailbox. The first reply fits; the rest hit
//! `MailboxFull` until the handler drains. The fmt subscriber
//! prints lifecycle events at TRACE and capacity rejections at
//! WARN, in the order the runtime produced them.
//!
//! Run with:
//!
//! ```text
//! cargo run --manifest-path examples/eiffel_tracing_demo/Cargo.toml
//! RUST_LOG=tina_runtime=trace cargo run --manifest-path examples/eiffel_tracing_demo/Cargo.toml
//! ```

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{DefaultMailboxFactory, Runtime, sleep};
use tina_tracing::TracingObserver;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Clone)]
enum Msg {
    Begin,
    SleepDone,
}

struct Caller {
    fanout: u32,
    delivered: u32,
}

#[tina_runtime::isolate(message = Msg)]
impl Caller {
    fn handle(
        &mut self,
        msg: Msg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            Msg::Begin => {
                let mut effects: Vec<Effect<Self>> = Vec::with_capacity(self.fanout as usize);
                for _ in 0..self.fanout {
                    effects.push(sleep(Duration::ZERO).reply(|_| Msg::SleepDone));
                }
                batch(effects)
            }
            Msg::SleepDone => {
                self.delivered += 1;
                if self.delivered >= self.fanout {
                    stop_with(self.delivered)
                } else {
                    noop()
                }
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .init();

    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    // Wire the observer before any event records. From here on every
    // recorded event becomes a structured tracing::Event.
    runtime.set_trace_observer(Some(Arc::new(TracingObserver::new())));

    let caller = runtime.register_with_capacity::<Caller, Infallible>(
        Caller {
            fanout: 6,
            delivered: 0,
        },
        // One inbound slot. The first sleep reply fits; the next ones
        // hit `MailboxFull` until the handler drains.
        1,
    );
    runtime
        .try_send(caller, Msg::Begin)
        .expect("kick caller off");

    for _ in 0..256 {
        runtime.step();
        if !runtime.has_in_flight_calls() {
            break;
        }
    }

    let summary = runtime.pressure_summary();
    eprintln!("--- pressure summary ---");
    eprintln!("{summary}");

    Ok(())
}
