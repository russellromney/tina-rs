//! Tina: a supervised worker. The parent owns the current typed child
//! reference. `spawn_observed(...).then_service_event_with_restarts(...)`
//! delivers both the initial child and every successful replacement as
//! ordinary bounded parent events, so neither the host nor application code
//! reconstructs addresses or service envelopes.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tina::{
    CallRejectedReason, RestartBudget, RestartPolicy, ServiceMessage, SpawnObservedResult,
    prelude::*,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, SplitServiceHandle, SupervisorReport,
    call_request,
};
use tina_supervisor::SupervisorConfig;

use crate::{Job, Report, job_script};

const CALL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerRequest {
    Process(Job),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerReply {
    Processed,
}

struct Worker;

fn take_factory_panic(counter: &AtomicUsize) -> bool {
    let mut remaining = counter.load(Ordering::SeqCst);
    loop {
        if remaining == 0 {
            return false;
        }
        match counter.compare_exchange_weak(
            remaining,
            remaining - 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return true,
            Err(actual) => remaining = actual,
        }
    }
}

#[tina_runtime::isolate(request = WorkerRequest, reply = WorkerReply)]
impl Worker {
    fn handle_request(
        &mut self,
        request: WorkerRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            WorkerRequest::Process(Job::Work(_)) => call.reply(WorkerReply::Processed),
            WorkerRequest::Process(Job::Poison) => {
                panic!("supervised worker hit a poison job")
            }
        }
    }
}

#[derive(Debug)]
enum ParentEvent {
    Started {
        result: SpawnObservedResult<ServiceMessage<Infallible, WorkerRequest>, WorkerReply>,
        request: RequestContext<ParentReply>,
    },
    Restarted(ChildRef<ServiceMessage<Infallible, WorkerRequest>, WorkerReply>),
    WorkDone(RequestContext<ParentReply>, CallOutcome<WorkerReply>),
}

#[derive(Debug, Clone, Copy)]
enum ParentRequest {
    Start,
    Process(Job),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentReply {
    Started,
    StartInProgress,
    Processed,
    WorkerPanicked,
    WorkerFull,
    WorkerClosed,
    WorkerTimeout,
    WorkerRejected(CallRejectedReason),
    NotStarted,
    SpawnRejected(SpawnObservedError),
}

struct Parent {
    worker: Option<ChildRef<ServiceMessage<Infallible, WorkerRequest>, WorkerReply>>,
    starting: bool,
    worker_capacity: usize,
    factory_panics: Arc<AtomicUsize>,
}

#[tina_runtime::isolate(
    event = ParentEvent,
    request = ParentRequest,
    reply = ParentReply,
    spawn = RestartableChildDefinition<Worker>,
    spawn_observed = tina::SpawnObserved<
        RestartableChildDefinition<Worker>,
        ServiceMessage<ParentEvent, ParentRequest>,
        ServiceMessage<Infallible, WorkerRequest>,
        WorkerReply
    >,
)]
impl Parent {
    fn handle_event(
        &mut self,
        event: ParentEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            ParentEvent::Started {
                result: Ok(worker),
                request,
            } => {
                self.starting = false;
                self.worker = Some(worker);
                reply_to(request, ParentReply::Started)
            }
            ParentEvent::Started {
                result: Err(error),
                request,
            } => {
                self.starting = false;
                reply_to(request, ParentReply::SpawnRejected(error))
            }
            ParentEvent::Restarted(worker) => {
                self.worker = Some(worker);
                noop()
            }
            ParentEvent::WorkDone(request, outcome) => {
                reply_to(request, parent_reply_from_worker(outcome))
            }
        }
    }

    fn handle_request(
        &mut self,
        request: ParentRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            ParentRequest::Start if self.worker.is_some() => call.reply(ParentReply::Started),
            ParentRequest::Start if self.starting => call.reply(ParentReply::StartInProgress),
            ParentRequest::Start => {
                self.starting = true;
                let capacity = self.worker_capacity;
                let factory_panics = Arc::clone(&self.factory_panics);
                call.capture(|request| {
                    spawn_observed(RestartableChildDefinition::new(
                        move || {
                            if take_factory_panic(&factory_panics) {
                                panic!("injected supervised-worker factory panic");
                            }
                            Worker
                        },
                        capacity,
                    ))
                    .then_service_event_with_restarts(
                        move |result| ParentEvent::Started { result, request },
                        ParentEvent::Restarted,
                    )
                })
            }
            ParentRequest::Process(job) => match self.worker {
                Some(worker) => {
                    let requests = SplitServiceHandle::from_address(worker.address).requests;
                    call.defer(call_request(
                        requests,
                        WorkerRequest::Process(job),
                        CALL_TIMEOUT,
                    ))
                    .reply_service_event(ParentEvent::WorkDone)
                }
                None => call.reply(ParentReply::NotStarted),
            },
        }
    }
}

fn parent_reply_from_worker(outcome: CallOutcome<WorkerReply>) -> ParentReply {
    match outcome {
        CallOutcome::Replied(WorkerReply::Processed) => ParentReply::Processed,
        CallOutcome::Full => ParentReply::WorkerFull,
        CallOutcome::Closed => ParentReply::WorkerClosed,
        CallOutcome::Timeout => ParentReply::WorkerTimeout,
        CallOutcome::Rejected(CallRejectedReason::HandlerPanicked) => ParentReply::WorkerPanicked,
        CallOutcome::Rejected(reason) => ParentReply::WorkerRejected(reason),
    }
}

pub fn run() -> anyhow::Result<Report> {
    let script = job_script();
    let poison_count = script.iter().filter(|j| matches!(j, Job::Poison)).count() as u32;

    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    let parent = app
        .register_split_service::<Parent, ParentEvent, ParentRequest, Infallible>(
            Parent {
                worker: None,
                starting: false,
                worker_capacity: 8,
                factory_panics: Arc::new(AtomicUsize::new(0)),
            },
            8,
        )
        .map_err(|error| anyhow::anyhow!("register parent: {error:?}"))?;

    app.try_supervise(
        parent.address(),
        SupervisorConfig::new(
            RestartPolicy::OneForOne,
            RestartBudget::new(poison_count + 2),
        ),
    )
    .map_err(|error| anyhow::anyhow!("supervise parent: {error:?}"))?
    .map_err(|error| anyhow::anyhow!("supervise: {error:?}"))?;

    expect_parent_reply(
        app.call_blocking_request(parent.requests, ParentRequest::Start, CALL_TIMEOUT)?,
        ParentReply::Started,
        "start parent",
    )?;

    let mut processed = 0;
    let mut poisoned = 0;
    let mut restarts = 0;
    for job in script {
        let restart_waiter = matches!(job, Job::Poison)
            .then(|| app.observe_child_restarted(parent.address()))
            .transpose()?;
        let expected = match job {
            Job::Work(_) => ParentReply::Processed,
            Job::Poison => ParentReply::WorkerPanicked,
        };
        expect_parent_reply(
            app.call_blocking_request(parent.requests, ParentRequest::Process(job), CALL_TIMEOUT)?,
            expected,
            "process job",
        )?;
        match job {
            Job::Work(_) => processed += 1,
            Job::Poison => poisoned += 1,
        }
        if let Some(waiter) = restart_waiter {
            waiter
                .wait(CALL_TIMEOUT)
                .map_err(|error| anyhow::anyhow!("supervisor restart: {error:?}"))?;
            restarts += 1;
        }
    }

    app.shutdown().drain().join_report().ensure_clean()?;

    Ok(Report {
        processed,
        poisoned,
        restarts,
        exit_clean: true,
    })
}

/// Factory-panic point exercised by [`run_factory_panic_correction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactoryPanicPhase {
    /// The initial observed child factory panics.
    Initial,
    /// The initial child starts, then its replacement factory panics.
    Replacement,
}

/// Exact typed fact produced by the factory-panic correction runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactoryPanicObserved {
    /// Initial spawn returned its typed error to the parent request.
    Initial(SpawnObservedError),
    /// Supervision recorded one replacement factory panic.
    Replacement { skipped_factory_panicked: u64 },
}

/// Exercise initial or replacement factory panic through the specimen's live
/// LocalSystem path without changing the normal workload.
pub fn run_factory_panic_correction(
    phase: FactoryPanicPhase,
) -> anyhow::Result<FactoryPanicObserved> {
    let factory_panics = Arc::new(AtomicUsize::new(usize::from(matches!(
        phase,
        FactoryPanicPhase::Initial
    ))));
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    let parent = app
        .register_split_service::<Parent, ParentEvent, ParentRequest, Infallible>(
            Parent {
                worker: None,
                starting: false,
                worker_capacity: 8,
                factory_panics: Arc::clone(&factory_panics),
            },
            8,
        )
        .map_err(|error| anyhow::anyhow!("register parent: {error:?}"))?;
    app.try_supervise(
        parent.address(),
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(2)),
    )
    .map_err(|error| anyhow::anyhow!("supervise parent: {error:?}"))?
    .map_err(|error| anyhow::anyhow!("supervise: {error:?}"))?;

    let start = app.call_blocking_request(parent.requests, ParentRequest::Start, CALL_TIMEOUT)?;
    if phase == FactoryPanicPhase::Initial {
        let observed = match start {
            CallOutcome::Replied(ParentReply::SpawnRejected(error)) => {
                FactoryPanicObserved::Initial(error)
            }
            other => anyhow::bail!("initial factory panic returned {other:?}"),
        };
        app.shutdown().drain().join_report().ensure_clean()?;
        return Ok(observed);
    }

    expect_parent_reply(start, ParentReply::Started, "start parent")?;
    factory_panics.store(1, Ordering::SeqCst);
    expect_parent_reply(
        app.call_blocking_request(
            parent.requests,
            ParentRequest::Process(Job::Poison),
            CALL_TIMEOUT,
        )?,
        ParentReply::WorkerPanicked,
        "poison worker",
    )?;

    let terminal = app.shutdown().drain().join_report();
    let supervision = SupervisorReport::from_events(terminal.trace(), parent.address().isolate());
    terminal.ensure_clean()?;
    Ok(FactoryPanicObserved::Replacement {
        skipped_factory_panicked: supervision.skipped_factory_panicked,
    })
}

fn expect_parent_reply(
    outcome: CallOutcome<ParentReply>,
    expected: ParentReply,
    operation: &str,
) -> anyhow::Result<()> {
    match outcome {
        CallOutcome::Replied(actual) if actual == expected => Ok(()),
        CallOutcome::Replied(actual) => {
            anyhow::bail!("{operation} returned {actual:?}, expected {expected:?}")
        }
        CallOutcome::Full => anyhow::bail!("{operation}: parent mailbox full"),
        CallOutcome::Closed => anyhow::bail!("{operation}: parent closed"),
        CallOutcome::Timeout => anyhow::bail!("{operation}: timed out"),
        CallOutcome::Rejected(reason) => anyhow::bail!("{operation}: rejected: {reason:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{ParentReply, WorkerReply, parent_reply_from_worker};
    use tina::CallRejectedReason;
    use tina_runtime::CallOutcome;

    #[test]
    fn worker_call_terminals_remain_distinct() {
        assert_eq!(
            parent_reply_from_worker(CallOutcome::Replied(WorkerReply::Processed)),
            ParentReply::Processed
        );
        assert_eq!(
            parent_reply_from_worker(CallOutcome::Full),
            ParentReply::WorkerFull
        );
        assert_eq!(
            parent_reply_from_worker(CallOutcome::Closed),
            ParentReply::WorkerClosed
        );
        assert_eq!(
            parent_reply_from_worker(CallOutcome::Timeout),
            ParentReply::WorkerTimeout
        );
        assert_eq!(
            parent_reply_from_worker(CallOutcome::Rejected(CallRejectedReason::HandlerPanicked)),
            ParentReply::WorkerPanicked
        );
        assert_eq!(
            parent_reply_from_worker(CallOutcome::Rejected(CallRejectedReason::ReplyAbandoned)),
            ParentReply::WorkerRejected(CallRejectedReason::ReplyAbandoned)
        );
        assert_eq!(
            parent_reply_from_worker(CallOutcome::Rejected(
                CallRejectedReason::UnsupportedMessage
            )),
            ParentReply::WorkerRejected(CallRejectedReason::UnsupportedMessage)
        );
    }
}
