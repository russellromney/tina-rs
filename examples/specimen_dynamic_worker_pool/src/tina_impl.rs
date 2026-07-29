//! Tina side. The coordinator observes each dynamic spawn, then makes one
//! typed request to the child. The request outcome is the join: a child that
//! panics before replying settles as `Rejected(HandlerPanicked)` instead of
//! leaving a missing parent message.

use std::convert::Infallible;
use std::time::Duration;

use tina::{
    CallRejectedReason, ChildDefinition, ServiceMessage, SpawnObservedError, SpawnObservedResult,
    prelude::*,
};
use tina_runtime::{
    BoundedItems, CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, SplitServiceHandle,
    bounded_batch, call_request,
};

use crate::{Report, WORK_VALUES, WORKER_COUNT};

const CALL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
enum WorkerRequest {
    Compute,
}

#[derive(Debug, Clone, Copy)]
enum WorkerReply {
    Partial(u64),
}

struct Worker {
    chunk: Vec<u64>,
    panic_on_compute: bool,
}

#[tina_runtime::isolate(request = WorkerRequest, reply = WorkerReply)]
impl Worker {
    fn handle_request(
        &mut self,
        request: WorkerRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            WorkerRequest::Compute => {
                assert!(!self.panic_on_compute, "injected worker failure");
                let partial = self.chunk.iter().copied().sum();
                call.reply_and(WorkerReply::Partial(partial), vec![stop()])
            }
        }
    }
}

#[derive(Debug)]
enum CoordMsg {
    Start,
    WorkerStarted(SpawnObservedResult<ServiceMessage<Infallible, WorkerRequest>, WorkerReply>),
    WorkerDone(CallOutcome<WorkerReply>),
}

struct Coordinator {
    expected: u32,
    settled: u32,
    chunks: Vec<Vec<u64>>,
    panic_worker: Option<u32>,
    report: Report,
}

#[tina_runtime::isolate(
    message = CoordMsg,
    spawn_observed = tina::SpawnObserved<
        ChildDefinition<Worker>,
        CoordMsg,
        ServiceMessage<Infallible, WorkerRequest>,
        WorkerReply
    >,
)]
impl Coordinator {
    fn handle(&mut self, msg: CoordMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            CoordMsg::Start => {
                let panic_worker = self.panic_worker;
                let chunks = BoundedItems::try_from_iter(
                    self.expected as usize,
                    self.chunks.drain(..).enumerate(),
                )
                .expect("worker chunks are capped by WORKER_COUNT");
                bounded_batch(chunks.map_effects(move |(index, chunk)| {
                    spawn_observed(ChildDefinition::new(
                        Worker {
                            chunk,
                            panic_on_compute: panic_worker == Some(index as u32),
                        },
                        1,
                    ))
                    .then(CoordMsg::WorkerStarted)
                }))
            }
            CoordMsg::WorkerStarted(Ok(child)) => {
                let worker = SplitServiceHandle::from_address(child.address).requests;
                call_request(worker, WorkerRequest::Compute, CALL_TIMEOUT)
                    .then(CoordMsg::WorkerDone)
            }
            CoordMsg::WorkerStarted(Err(error)) => {
                record_spawn_error(&mut self.report, error);
                self.settle()
            }
            CoordMsg::WorkerDone(outcome) => {
                record_worker_outcome(&mut self.report, outcome);
                self.settle()
            }
        }
    }
}

fn record_spawn_error(report: &mut Report, error: SpawnObservedError) {
    match error {
        SpawnObservedError::ZeroMailboxCapacity => report.spawn_zero_capacity += 1,
        SpawnObservedError::DestinationUnavailable => report.spawn_destination_unavailable += 1,
        SpawnObservedError::FactoryPanicked => report.spawn_factory_panicked += 1,
        SpawnObservedError::ParentMailboxFull => report.spawn_parent_mailbox_full += 1,
        SpawnObservedError::ParentMailboxClosed => report.spawn_parent_mailbox_closed += 1,
        SpawnObservedError::ParentMailboxReservationsUnsupported => {
            report.spawn_parent_mailbox_reservations_unsupported += 1;
        }
        // `SpawnObservedError` is `#[non_exhaustive]`; only variants added
        // after this specimen land in `spawn_other`.
        other => report.record_future_spawn(other),
    }
}

impl Report {
    fn record_future_spawn(&mut self, _error: SpawnObservedError) {
        self.spawn_other += 1;
    }
}

fn record_worker_outcome(report: &mut Report, outcome: CallOutcome<WorkerReply>) {
    match outcome {
        CallOutcome::Replied(WorkerReply::Partial(partial)) => {
            report.results_collected += 1;
            report.total_sum += partial;
        }
        CallOutcome::Full => report.call_full += 1,
        CallOutcome::Closed => report.call_closed += 1,
        CallOutcome::Timeout => report.call_timeout += 1,
        CallOutcome::Rejected(CallRejectedReason::ForeignSystem { .. }) => {
            report.rejected_foreign_system += 1;
        }
        CallOutcome::Rejected(CallRejectedReason::ReplyAbandoned) => {
            report.rejected_reply_abandoned += 1;
        }
        CallOutcome::Rejected(CallRejectedReason::HandlerPanicked) => {
            report.rejected_handler_panicked += 1;
        }
        CallOutcome::Rejected(CallRejectedReason::UnsupportedMessage) => {
            report.rejected_unsupported_message += 1;
        }
    }
}

impl Coordinator {
    fn settle(&mut self) -> Effect<Self> {
        self.settled += 1;
        if self.settled == self.expected {
            self.report.exit_clean = true;
            stop_with(self.report)
        } else {
            noop()
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    run_with_failure(None)
}

/// Runs the fixed workload with one optional child panic for failure-path
/// regression coverage.
pub fn run_with_failure(panic_worker: Option<u32>) -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;

    let chunk_size = WORK_VALUES.len() / WORKER_COUNT as usize;
    let chunks: Vec<Vec<u64>> = (0..WORKER_COUNT as usize)
        .map(|i| WORK_VALUES[i * chunk_size..(i + 1) * chunk_size].to_vec())
        .collect();

    Ok(
        app.run_to_shutdown_reported(Duration::from_secs(5), move |app| {
            let coord_addr = app
                .register_root::<_, Infallible>(
                    Coordinator {
                        expected: WORKER_COUNT,
                        settled: 0,
                        chunks,
                        panic_worker,
                        report: Report::default(),
                    },
                    (WORKER_COUNT + 4) as usize,
                )
                .map_err(|e| anyhow::anyhow!("register coordinator: {e:?}"))?;

            let waiter = app
                .observe_result::<Report, _, _>(coord_addr)
                .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

            app.try_send(coord_addr, CoordMsg::Start)
                .map_err(|e| anyhow::anyhow!("send Start: {e:?}"))?;

            waiter
                .wait(Duration::from_secs(5))
                .map_err(|e| anyhow::anyhow!("coord did not finish: {e:?}"))
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_worker_call_outcome_has_an_independent_bucket() {
        let expected = tina::SystemIncarnation::new(1);
        let actual = tina::SystemIncarnation::new(2);
        let mut report = Report::default();
        for outcome in [
            CallOutcome::Replied(WorkerReply::Partial(7)),
            CallOutcome::Full,
            CallOutcome::Closed,
            CallOutcome::Timeout,
            CallOutcome::Rejected(CallRejectedReason::ForeignSystem { expected, actual }),
            CallOutcome::Rejected(CallRejectedReason::ReplyAbandoned),
            CallOutcome::Rejected(CallRejectedReason::HandlerPanicked),
            CallOutcome::Rejected(CallRejectedReason::UnsupportedMessage),
        ] {
            record_worker_outcome(&mut report, outcome);
        }
        assert_eq!(report.results_collected, 1);
        assert_eq!(report.total_sum, 7);
        assert_eq!(report.call_full, 1);
        assert_eq!(report.call_closed, 1);
        assert_eq!(report.call_timeout, 1);
        assert_eq!(report.rejected_foreign_system, 1);
        assert_eq!(report.rejected_reply_abandoned, 1);
        assert_eq!(report.rejected_handler_panicked, 1);
        assert_eq!(report.rejected_unsupported_message, 1);
    }

    #[test]
    fn spawn_rejections_have_independent_buckets() {
        let mut report = Report::default();
        record_spawn_error(&mut report, SpawnObservedError::ZeroMailboxCapacity);
        record_spawn_error(&mut report, SpawnObservedError::DestinationUnavailable);
        record_spawn_error(&mut report, SpawnObservedError::FactoryPanicked);
        record_spawn_error(&mut report, SpawnObservedError::ParentMailboxFull);
        record_spawn_error(&mut report, SpawnObservedError::ParentMailboxClosed);
        record_spawn_error(
            &mut report,
            SpawnObservedError::ParentMailboxReservationsUnsupported,
        );
        assert_eq!(report.spawn_zero_capacity, 1);
        assert_eq!(report.spawn_destination_unavailable, 1);
        assert_eq!(report.spawn_factory_panicked, 1);
        assert_eq!(report.spawn_parent_mailbox_full, 1);
        assert_eq!(report.spawn_parent_mailbox_closed, 1);
        assert_eq!(report.spawn_parent_mailbox_reservations_unsupported, 1);
        assert_eq!(report.spawn_other, 0);
    }
}
