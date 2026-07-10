//! Public-path proof for the README bounded-mailbox workflow.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tina::TrySendError;
use tina::prelude::*;
use tina_runtime::{DefaultMailboxFactory, Runtime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Run(u64),
    Stop,
}

#[derive(Debug)]
struct Worker {
    processed: Arc<AtomicUsize>,
}

#[tina::isolate(message = Job)]
impl Worker {
    fn handle(
        &mut self,
        job: Job,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match job {
            Job::Run(_) => {
                self.processed.fetch_add(1, Ordering::Relaxed);
                noop()
            }
            Job::Stop => stop(),
        }
    }
}

#[test]
fn full_returns_the_job_retry_succeeds_and_stop_closes_the_mailbox() {
    let processed = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let worker = runtime.register_with_capacity::<Worker, Infallible>(
        Worker {
            processed: Arc::clone(&processed),
        },
        2,
    );

    runtime.try_send(worker, Job::Run(1)).unwrap();
    runtime.try_send(worker, Job::Run(2)).unwrap();
    let rejected = match runtime.try_send(worker, Job::Run(3)) {
        Err(TrySendError::Full(job)) => job,
        other => panic!("expected Full, got {other:?}"),
    };

    assert_eq!(processed.load(Ordering::Relaxed), 0);
    assert_eq!(runtime.step(), 1);
    assert_eq!(processed.load(Ordering::Relaxed), 1);
    runtime.try_send(worker, rejected).unwrap();

    while runtime.step() > 0 {}
    assert_eq!(processed.load(Ordering::Relaxed), 3);

    runtime.try_send(worker, Job::Stop).unwrap();
    assert_eq!(runtime.step(), 1);
    assert_eq!(
        runtime.try_send(worker, Job::Run(4)),
        Err(TrySendError::Closed(Job::Run(4)))
    );
}
