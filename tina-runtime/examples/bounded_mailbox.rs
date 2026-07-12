//! Smallest runnable boundedness example.
//!
//! A host producer fills a worker's two-slot mailbox. The third send returns
//! typed `Full` with the undelivered job, which the host retries after one
//! runtime step. Once the worker stops, another send returns typed `Closed`.
//!
//! Run with:
//! ```bash
//! cargo run --locked -p tina-runtime --example bounded_mailbox
//! ```

use std::convert::Infallible;
use std::fmt;

use tina::prelude::*;
use tina_runtime::IngressSendError;
use tina_runtime::{DefaultMailboxFactory, Runtime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Run(u64),
    Stop,
}

#[derive(Debug)]
pub struct Worker;

#[tina_runtime::isolate(message = Job)]
impl Worker {
    fn handle(
        &mut self,
        job: Job,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match job {
            Job::Run(_) => noop(),
            Job::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScenarioReport {
    pub rejected: Job,
    pub retried: Job,
    pub closed: Job,
}

impl fmt::Display for ScenarioReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "send {:?} -> Full({:?}); host retains the job",
            self.rejected, self.rejected
        )?;
        writeln!(
            formatter,
            "retry {:?} after one step -> Accepted",
            self.retried
        )?;
        write!(
            formatter,
            "send {:?} after stop -> Closed({:?}); host retains the job",
            self.closed, self.closed
        )
    }
}

pub fn run_scenario() -> ScenarioReport {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let worker = runtime.register_with_capacity::<Worker, Infallible>(Worker, 2);

    runtime.try_send(worker, Job::Run(1)).expect("job 1 fits");
    runtime.try_send(worker, Job::Run(2)).expect("job 2 fits");

    let rejected = match runtime.try_send(worker, Job::Run(3)) {
        Err(IngressSendError::Full(job)) => job,
        Err(IngressSendError::ForeignSystem { .. }) => panic!("worker address became foreign"),
        other => panic!("expected typed Full, got {other:?}"),
    };
    assert_eq!(rejected, Job::Run(3), "Full returns the attempted job");

    assert_eq!(runtime.step(), 1, "one worker handles one queued job");
    runtime
        .try_send(worker, rejected)
        .expect("retry fits after one step");

    while runtime.step() > 0 {}

    runtime.try_send(worker, Job::Stop).expect("stop fits");
    assert_eq!(runtime.step(), 1, "worker handles stop");

    let closed = match runtime.try_send(worker, Job::Run(4)) {
        Err(IngressSendError::Closed(job)) => job,
        Err(IngressSendError::ForeignSystem { .. }) => panic!("worker address became foreign"),
        other => panic!("expected typed Closed, got {other:?}"),
    };
    assert_eq!(closed, Job::Run(4), "Closed returns the attempted job");

    ScenarioReport {
        rejected,
        retried: rejected,
        closed,
    }
}

fn main() {
    println!("{}", run_scenario());
}
