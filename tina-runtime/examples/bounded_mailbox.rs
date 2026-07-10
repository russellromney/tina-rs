//! Smallest runnable boundedness example.
//!
//! A worker has room for two queued jobs. The third send returns typed `Full`
//! with the undelivered job, which the caller retries after one runtime step.
//! Once the worker stops, another send returns typed `Closed`.
//!
//! Run with:
//! ```bash
//! cargo run --locked -p tina-runtime --example bounded_mailbox
//! ```

use std::convert::Infallible;

use tina::TrySendError;
use tina::prelude::*;
use tina_runtime::{DefaultMailboxFactory, Runtime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Run(u64),
    Stop,
}

#[derive(Debug)]
struct Worker;

#[tina::isolate(message = Job)]
impl Worker {
    fn handle(
        &mut self,
        job: Job,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match job {
            Job::Run(id) => {
                println!("processed job={id}");
                noop()
            }
            Job::Stop => stop(),
        }
    }
}

fn main() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let worker = runtime.register_with_capacity::<Worker, Infallible>(Worker, 2);

    runtime.try_send(worker, Job::Run(1)).expect("job 1 fits");
    runtime.try_send(worker, Job::Run(2)).expect("job 2 fits");

    let rejected = match runtime.try_send(worker, Job::Run(3)) {
        Err(TrySendError::Full(job)) => {
            println!("send {job:?} -> Full({job:?}); caller retains the job");
            job
        }
        other => panic!("expected typed Full, got {other:?}"),
    };

    assert_eq!(runtime.step(), 1, "one worker handles one queued job");
    runtime
        .try_send(worker, rejected)
        .expect("retry fits after one step");

    while runtime.step() > 0 {}

    runtime.try_send(worker, Job::Stop).expect("stop fits");
    assert_eq!(runtime.step(), 1, "worker handles stop");

    match runtime.try_send(worker, Job::Run(4)) {
        Err(TrySendError::Closed(Job::Run(4))) => println!("mailbox closed"),
        other => panic!("expected typed Closed, got {other:?}"),
    }
}
