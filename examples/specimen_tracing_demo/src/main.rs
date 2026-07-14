//! Live Tina runtime trace -> tracing fmt subscriber.
//!
//! Single-shard runtime with a [`TracingObserver`] wired before the
//! first event. Caller fans out a bounded set of zero-duration
//! `sleep` calls and reports both delivered completions and exact
//! timer failures alongside the runtime's pressure summary.
//!
//! ```text
//! cargo run --manifest-path examples/specimen_tracing_demo/Cargo.toml
//! RUST_LOG=tina_runtime=trace cargo run --manifest-path examples/specimen_tracing_demo/Cargo.toml
//! ```

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{BoundedItems, CallError, DefaultMailboxFactory, Runtime, bounded_batch, sleep};
use tina_tracing::TracingObserver;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Clone)]
enum Msg {
    Begin,
    SleepDone(Result<(), CallError>),
    Finish,
}

struct Caller {
    fanout: u32,
    delivered: u32,
    timer_failures: Vec<CallError>,
}

const DEFAULT_FANOUT: usize = 6;
const MAX_FANOUT: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub fanout: usize,
    pub delivered: u32,
    pub timer_failures: Vec<CallError>,
    pub completion_mailbox_full: u64,
    pub completion_requester_closed: u64,
    pub stopped_with_result: bool,
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
                let items = BoundedItems::try_from_iter(MAX_FANOUT, 0..self.fanout as usize)
                    .expect("fanout was validated before actor construction");
                bounded_batch(items.map_effects(|_| sleep(Duration::ZERO).then(Msg::SleepDone)))
            }
            Msg::SleepDone(outcome) => {
                match outcome {
                    Ok(()) => self.delivered += 1,
                    Err(error) => self.timer_failures.push(error),
                }
                noop()
            }
            Msg::Finish => stop_with((self.delivered, std::mem::take(&mut self.timer_failures))),
        }
    }
}

pub fn run_demo(fanout: usize) -> anyhow::Result<Report> {
    if fanout == 0 {
        anyhow::bail!("fanout must be greater than zero");
    }
    BoundedItems::try_from_iter(MAX_FANOUT, 0..fanout)
        .map_err(|error| anyhow::anyhow!("invalid tracing fanout: {error}"))?;

    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    runtime.set_trace_observer(Some(Arc::new(TracingObserver::new())));

    let caller = runtime.register_with_capacity::<Caller, Infallible>(
        Caller {
            fanout: fanout as u32,
            delivered: 0,
            timer_failures: Vec::new(),
        },
        1,
    );
    let result = runtime
        .observe_result::<(u32, Vec<CallError>), _, _>(caller)
        .map_err(|error| anyhow::anyhow!("observe caller result: {error:?}"))?;
    runtime
        .try_send(caller, Msg::Begin)
        .map_err(|error| anyhow::anyhow!("kick caller: {error:?}"))?;

    for _ in 0..256 {
        runtime.step();
    }
    runtime
        .try_send(caller, Msg::Finish)
        .map_err(|error| anyhow::anyhow!("finish caller: {error:?}"))?;
    for _ in 0..16 {
        runtime.step();
    }
    let (delivered, timer_failures) = result
        .wait(Duration::from_millis(10))
        .map_err(|error| anyhow::anyhow!("caller result: {error:?}"))?;
    let pressure = runtime.pressure_summary();

    Ok(Report {
        fanout,
        delivered,
        timer_failures,
        completion_mailbox_full: pressure.completion_rejected_mailbox_full,
        completion_requester_closed: pressure.completion_rejected_requester_closed,
        stopped_with_result: true,
    })
}

fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .init();

    let report = run_demo(DEFAULT_FANOUT)?;
    eprintln!("--- pressure summary ---");
    eprintln!("{report:?}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_demo_reports_delivery_pressure_and_typed_stop() {
        for fanout in [DEFAULT_FANOUT, MAX_FANOUT] {
            let report = run_demo(fanout).expect("demo runs");
            assert_eq!(
                report.delivered as usize
                    + report.timer_failures.len()
                    + report.completion_mailbox_full as usize,
                report.fanout
            );
            assert_eq!(report.completion_requester_closed, 0, "{report:?}");
            assert!(report.stopped_with_result);
        }
    }

    #[test]
    fn rejects_unbounded_fanout_before_runtime_construction() {
        assert!(run_demo(0).is_err());
        assert!(run_demo(MAX_FANOUT + 1).is_err());
    }
}
