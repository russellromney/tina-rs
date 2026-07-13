//! Tina: a supervised worker. The parent owns the current typed child
//! reference. `spawn_observed(...).then_with_restarts(...)` delivers both the
//! initial child and every successful replacement as ordinary bounded parent
//! messages, so neither the host nor application code reconstructs addresses.

use std::time::Duration;

use tina::{RestartBudget, RestartPolicy, ServiceMessage, SpawnObservedResult, prelude::*};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem};
use tina_supervisor::SupervisorConfig;

use crate::{Job, Report, job_script};

const CALL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerMsg {
    Process(Job),
}

struct Worker;

#[tina_runtime::isolate(message = WorkerMsg)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Process(Job::Work(_)) => noop(),
            WorkerMsg::Process(Job::Poison) => panic!("supervised worker hit a poison job"),
        }
    }
}

#[derive(Debug)]
enum ParentEvent {
    WorkerStarted {
        result: SpawnObservedResult<WorkerMsg>,
        request: RequestContext<ParentReply>,
    },
    WorkerRestarted(ChildRef<WorkerMsg>),
}

#[derive(Debug, Clone, Copy)]
enum ParentRequest {
    Start,
    Process(Job),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentReply {
    Started,
    Accepted,
    NotStarted,
    SpawnRejected(SpawnObservedError),
}

struct Parent {
    worker: Option<ChildRef<WorkerMsg>>,
    worker_capacity: usize,
}

#[tina_runtime::isolate(
    event = ParentEvent,
    request = ParentRequest,
    reply = ParentReply,
    send = Outbound<WorkerMsg>,
    spawn = RestartableChildDefinition<Worker>,
    spawn_observed = tina::SpawnObserved<RestartableChildDefinition<Worker>, ServiceMessage<ParentEvent, ParentRequest>, WorkerMsg>,
)]
impl Parent {
    fn handle_event(
        &mut self,
        event: ParentEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            ParentEvent::WorkerStarted {
                result: Ok(worker),
                request,
            } => {
                self.worker = Some(worker);
                reply_to(request, ParentReply::Started)
            }
            ParentEvent::WorkerStarted {
                result: Err(error),
                request,
            } => reply_to(request, ParentReply::SpawnRejected(error)),
            ParentEvent::WorkerRestarted(worker) => {
                self.worker = Some(worker);
                noop()
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
            ParentRequest::Start => {
                let capacity = self.worker_capacity;
                call.capture(|request| {
                    spawn_observed(RestartableChildDefinition::new(move || Worker, capacity))
                        .then_with_restarts(
                            move |result| ServiceMessage::Event(ParentEvent::WorkerStarted {
                                result,
                                request,
                            }),
                            |worker| {
                                ServiceMessage::Event(ParentEvent::WorkerRestarted(worker))
                            },
                        )
                })
            }
            ParentRequest::Process(job) => match self.worker {
                Some(worker) => call.reply_and(
                    ParentReply::Accepted,
                    vec![send(worker.address, WorkerMsg::Process(job))],
                ),
                None => call.reply(ParentReply::NotStarted),
            },
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let script = job_script();
    let poison_count = script.iter().filter(|j| matches!(j, Job::Poison)).count() as u32;
    let work_count = script.iter().filter(|j| matches!(j, Job::Work(_))).count() as u32;

    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    let parent = app
        .register_split_service::<Parent, ParentEvent, ParentRequest, WorkerMsg>(
            Parent {
                worker: None,
                worker_capacity: 8,
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

    let mut restarts = 0;
    for job in script {
        let restart_waiter = matches!(job, Job::Poison)
            .then(|| app.observe_child_restarted(parent.address()))
            .transpose()?;
        expect_parent_reply(
            app.call_blocking_request(
                parent.requests,
                ParentRequest::Process(job),
                CALL_TIMEOUT,
            )?,
            ParentReply::Accepted,
            "process job",
        )?;
        if let Some(waiter) = restart_waiter {
            waiter
                .wait(CALL_TIMEOUT)
                .map_err(|error| anyhow::anyhow!("supervisor restart: {error:?}"))?;
            restarts += 1;
        }
    }

    app.shutdown().drain().join_report().ensure_clean()?;

    Ok(Report {
        processed: work_count,
        poisoned: poison_count,
        restarts,
        exit_clean: true,
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
