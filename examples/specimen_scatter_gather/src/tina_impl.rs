//! Tina: a `Coordinator` isolate owns a bounded collection of typed
//! scatter/gather operations. Each operation owns its original caller,
//! child cancellation authority, ordered target outcomes, and aggregate
//! deadline. The application supplies targets and folds the completed report.
//!
//! Driver: one `Driver` isolate batches `CLIENTS` parallel calls
//! against the coordinator, accumulates outcomes, and `stop_with`s
//! a typed result. The host reads the result via
//! `runtime.observe_result`.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::sharded::{ScatterGatherConfig, ScatterGatherReport, ScatterGatherTargetOutcome};
use tina_runtime::{
    BoundedItems, CallOutcome, DefaultThreadedMailboxFactory, ScatterGatherEvent,
    ScatterGatherOperations, ScatterGatherOperationsStart, ScatterGatherStartError,
    ThreadedRuntime, bounded_batch, call_cancelable, call_request,
};

use crate::{
    CLIENTS, IncorrectAggregate, MAX_IN_FLIGHT, Report, TargetOutcome, TargetResult,
    TinaTerminalReport, WORKERS, expected_aggregate,
};

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

// --- Worker ---------------------------------------------------------------

#[derive(Debug)]
enum WorkerMsg {
    Do(u64),
}

#[derive(Debug, Clone, Copy)]
struct WorkerReply {
    worker: usize,
    value: u64,
}

struct Worker {
    id: u64,
}

#[tina::isolate(message = WorkerMsg, reply = WorkerReply)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.apply(msg))
    }

    fn handle_call(&mut self, msg: WorkerMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.apply(msg))
    }
}

impl Worker {
    fn apply(&self, msg: WorkerMsg) -> WorkerReply {
        match msg {
            WorkerMsg::Do(payload) => WorkerReply {
                worker: self.id as usize,
                value: payload.wrapping_add(self.id),
            },
        }
    }
}

// --- Coordinator ----------------------------------------------------------

#[derive(Debug)]
enum CoordEvent {
    Scatter(ScatterGatherEvent<usize, WorkerReply>),
}

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum CoordRequest {
    Query(u64),
}

#[derive(Debug)]
enum AggregateReply {
    Complete(ScatterGatherReport<WorkerReply, usize>),
    Full,
    StartRejected(ScatterGatherStartError<usize>),
}

struct Coordinator {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    operations: ScatterGatherOperations<usize, WorkerReply, AggregateReply>,
}

#[tina_runtime::isolate(event = CoordEvent, request = CoordRequest, reply = AggregateReply)]
impl Coordinator {
    fn handle_event(
        &mut self,
        event: CoordEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        let CoordEvent::Scatter(event) = event;
        let Some(advance) = self
            .operations
            .advance_service::<Self, _, _, _>(event, CoordEvent::Scatter)
            .unwrap_or_else(|error| panic!("scatter continuation violated authority: {error:?}"))
        else {
            return noop();
        };
        match advance.completed {
            Some(completed) => Effect::Batch(vec![
                advance.effect,
                reply_to(
                    completed.request,
                    AggregateReply::Complete(completed.report),
                ),
            ]),
            None => advance.effect,
        }
    }

    fn handle_request(
        &mut self,
        request: CoordRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            CoordRequest::Query(payload) => call.capture(|request| {
                let config = ScatterGatherConfig {
                    max_targets: self.workers.len(),
                    collector_capacity: self.workers.len(),
                    per_target_timeout: QUERY_TIMEOUT,
                    aggregate_timeout: QUERY_TIMEOUT,
                };
                let targets = BoundedItems::try_from_iter(
                    config.max_targets,
                    self.workers
                        .iter()
                        .copied()
                        .enumerate()
                        .map(|(key, worker)| (key, Some(worker))),
                )
                .expect("worker list defines the scatter target cap");
                match self.operations.start_service::<Self, _, _, _, _, _, _>(
                    request,
                    config,
                    targets,
                    move |worker, timeout| call_cancelable(worker, WorkerMsg::Do(payload), timeout),
                    CoordEvent::Scatter,
                ) {
                    Ok(ScatterGatherOperationsStart::Running(effect)) => effect,
                    Ok(ScatterGatherOperationsStart::Ready(completed)) => reply_to(
                        completed.request,
                        AggregateReply::Complete(completed.report),
                    ),
                    Err(failure) => match failure.error {
                        ScatterGatherStartError::OperationsFull { .. } => {
                            reply_to(failure.request, AggregateReply::Full)
                        }
                        error => reply_to(failure.request, AggregateReply::StartRejected(error)),
                    },
                }
            }),
        }
    }
}

// --- Driver ---------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct DriverOutcome {
    correct: usize,
    wrong: usize,
    failed: usize,
    coordinator_full: usize,
    coordinator_mailbox_full: usize,
    coordinator_closed: usize,
    coordinator_timeout: usize,
    coordinator_rejected: Vec<tina::CallRejectedReason>,
    start_rejected: Vec<ScatterGatherStartError<usize>>,
    incorrect_aggregates: Vec<IncorrectAggregate>,
    refill_correct: usize,
}

#[derive(Debug)]
enum DriverMsg {
    Begin,
    Returned(usize, CallOutcome<AggregateReply>),
    RefillReturned(u64, CallOutcome<AggregateReply>),
}

struct Driver {
    coord: tina::ServiceRequestAddress<CoordEvent, CoordRequest, AggregateReply>,
    payloads: BoundedItems<(usize, u64)>,
    remaining: usize,
    outcome: DriverOutcome,
    refill_payload: Option<u64>,
}

#[tina_runtime::isolate(message = DriverMsg)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin => {
                let coord = self.coord;
                let payloads = self.payloads.clone();
                bounded_batch(payloads.map_effects(|(i, payload)| {
                    call_request(coord, CoordRequest::Query(payload), QUERY_TIMEOUT)
                        .then(move |outcome| DriverMsg::Returned(i, outcome))
                }))
            }
            DriverMsg::Returned(i, outcome) => {
                let payload = self.payloads.as_slice()[i].1;
                match outcome {
                    CallOutcome::Replied(AggregateReply::Complete(report))
                        if aggregate_sum(&report, payload) == Some(expected_aggregate(payload)) =>
                    {
                        self.outcome.correct += 1;
                    }
                    CallOutcome::Replied(AggregateReply::Complete(report)) => {
                        self.outcome.wrong += 1;
                        self.outcome
                            .incorrect_aggregates
                            .push(incorrect_aggregate(&report, payload));
                    }
                    CallOutcome::Replied(AggregateReply::Full) => {
                        self.outcome.coordinator_full += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Replied(AggregateReply::StartRejected(error)) => {
                        self.outcome.start_rejected.push(error);
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Full => {
                        self.outcome.coordinator_mailbox_full += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Closed => {
                        self.outcome.coordinator_closed += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Timeout => {
                        self.outcome.coordinator_timeout += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Rejected(reason) => {
                        self.outcome.coordinator_rejected.push(reason);
                        self.outcome.failed += 1;
                    }
                }
                self.remaining -= 1;
                if self.remaining == 0 {
                    if let Some(payload) = self.refill_payload.take() {
                        call_request(self.coord, CoordRequest::Query(payload), QUERY_TIMEOUT)
                            .then(move |outcome| DriverMsg::RefillReturned(payload, outcome))
                    } else {
                        stop_with(std::mem::take(&mut self.outcome))
                    }
                } else {
                    noop()
                }
            }
            DriverMsg::RefillReturned(payload, outcome) => {
                match outcome {
                    CallOutcome::Replied(AggregateReply::Complete(report))
                        if aggregate_sum(&report, payload) == Some(expected_aggregate(payload)) =>
                    {
                        self.outcome.refill_correct += 1;
                    }
                    CallOutcome::Replied(AggregateReply::Complete(report)) => {
                        self.outcome.wrong += 1;
                        self.outcome
                            .incorrect_aggregates
                            .push(incorrect_aggregate(&report, payload));
                    }
                    CallOutcome::Replied(AggregateReply::Full) => {
                        self.outcome.coordinator_full += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Replied(AggregateReply::StartRejected(error)) => {
                        self.outcome.start_rejected.push(error);
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Full => {
                        self.outcome.coordinator_mailbox_full += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Closed => {
                        self.outcome.coordinator_closed += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Timeout => {
                        self.outcome.coordinator_timeout += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Rejected(reason) => {
                        self.outcome.coordinator_rejected.push(reason);
                        self.outcome.failed += 1;
                    }
                }
                stop_with(std::mem::take(&mut self.outcome))
            }
        }
    }
}

fn aggregate_sum(report: &ScatterGatherReport<WorkerReply, usize>, payload: u64) -> Option<u64> {
    if report.outcomes.len() != WORKERS {
        return None;
    }
    report.outcomes.iter().enumerate().try_fold(
        0u64,
        |sum, (expected_worker, (target, outcome))| match outcome {
            ScatterGatherTargetOutcome::Replied(reply)
                if *target == expected_worker
                    && reply.worker == expected_worker
                    && reply.value == payload.wrapping_add(expected_worker as u64) =>
            {
                Some(sum.wrapping_add(reply.value))
            }
            ScatterGatherTargetOutcome::Replied(_)
            | ScatterGatherTargetOutcome::Full
            | ScatterGatherTargetOutcome::Closed
            | ScatterGatherTargetOutcome::Timeout
            | ScatterGatherTargetOutcome::Rejected(_)
            | ScatterGatherTargetOutcome::AggregateTimeout
            | ScatterGatherTargetOutcome::MissingShard => None,
        },
    )
}

fn incorrect_aggregate(
    report: &ScatterGatherReport<WorkerReply, usize>,
    payload: u64,
) -> IncorrectAggregate {
    IncorrectAggregate {
        expected_targets: WORKERS,
        targets: report
            .outcomes
            .iter()
            .enumerate()
            .map(|(expected_target, (actual_target, outcome))| TargetResult {
                expected_target,
                actual_target: *actual_target,
                outcome: match outcome {
                    ScatterGatherTargetOutcome::Replied(reply) => TargetOutcome::Replied {
                        worker: reply.worker,
                        value: reply.value,
                        expected_value: payload.wrapping_add(expected_target as u64),
                    },
                    ScatterGatherTargetOutcome::Full => TargetOutcome::Full,
                    ScatterGatherTargetOutcome::Closed => TargetOutcome::Closed,
                    ScatterGatherTargetOutcome::Timeout => TargetOutcome::Timeout,
                    ScatterGatherTargetOutcome::Rejected(reason) => {
                        TargetOutcome::Rejected(*reason)
                    }
                    ScatterGatherTargetOutcome::AggregateTimeout => TargetOutcome::AggregateTimeout,
                    ScatterGatherTargetOutcome::MissingShard => TargetOutcome::MissingShard,
                },
            })
            .collect(),
    }
}

// --- Run ------------------------------------------------------------------

pub fn run() -> anyhow::Result<Report> {
    run_with_max_in_flight(MAX_IN_FLIGHT, false).map(|(report, _)| report)
}

fn run_with_max_in_flight(
    max_in_flight: usize,
    prove_refill: bool,
) -> anyhow::Result<(Report, DriverOutcome)> {
    let runtime = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)?;

    let mut workers = Vec::with_capacity(WORKERS);
    for w in 0..WORKERS as u64 {
        let addr = runtime
            .register_with_capacity::<_, Infallible>(Worker { id: w }, 16)
            .map_err(|e| anyhow::anyhow!("register worker {w}: {e:?}"))?;
        workers.push(addr);
    }

    let coord = runtime
        .register_split_service::<Coordinator, CoordEvent, CoordRequest, Infallible>(
            Coordinator {
                workers,
                operations: ScatterGatherOperations::with_capacity(max_in_flight),
            },
            32,
        )
        .map_err(|e| anyhow::anyhow!("register coordinator: {e:?}"))?
        .requests;

    let payloads = BoundedItems::try_from_iter(
        CLIENTS,
        (0..CLIENTS as u64)
            .map(|c| c.wrapping_mul(7).wrapping_add(11))
            .enumerate(),
    )
    .expect("CLIENTS is the driver-owned request bound");
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                coord,
                payloads,
                remaining: CLIENTS,
                outcome: DriverOutcome::default(),
                refill_payload: prove_refill.then_some(CLIENTS as u64 * 7 + 11),
            },
            32,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = runtime
        .observe_result::<DriverOutcome, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("register result waiter: {e:?}"))?;
    runtime
        .try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick driver: {e:?}"))?;
    let outcome = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("driver finishes: {e:?}"))?;

    runtime.shutdown_report().ensure_clean()?;

    Ok((
        Report {
            clients: CLIENTS,
            workers: WORKERS,
            aggregates_correct: outcome.correct,
            aggregates_wrong: outcome.wrong,
            failed: outcome.failed,
            exit_clean: true,
            tina_terminals: TinaTerminalReport {
                coordinator_full: outcome.coordinator_full,
                coordinator_mailbox_full: outcome.coordinator_mailbox_full,
                coordinator_closed: outcome.coordinator_closed,
                coordinator_timeout: outcome.coordinator_timeout,
                coordinator_rejected: outcome.coordinator_rejected.clone(),
                start_rejected: outcome.start_rejected.clone(),
                incorrect_aggregates: outcome.incorrect_aggregates.clone(),
            },
        },
        outcome,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_report(payload: u64) -> ScatterGatherReport<WorkerReply, usize> {
        ScatterGatherReport {
            config: ScatterGatherConfig {
                max_targets: WORKERS,
                collector_capacity: WORKERS,
                per_target_timeout: QUERY_TIMEOUT,
                aggregate_timeout: QUERY_TIMEOUT,
            },
            outcomes: (0..WORKERS)
                .map(|worker| {
                    (
                        worker,
                        ScatterGatherTargetOutcome::Replied(WorkerReply {
                            worker,
                            value: payload + worker as u64,
                        }),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn aggregate_validation_rejects_reordering_misrouting_and_wrong_values() {
        let payload = 11;
        let report = complete_report(payload);
        assert_eq!(
            aggregate_sum(&report, payload),
            Some(expected_aggregate(payload))
        );

        let mut reordered = complete_report(payload);
        reordered.outcomes.swap(0, 1);
        assert_eq!(aggregate_sum(&reordered, payload), None);

        let mut misrouted = complete_report(payload);
        let ScatterGatherTargetOutcome::Replied(reply) = &mut misrouted.outcomes[0].1 else {
            unreachable!()
        };
        reply.worker = 1;
        assert_eq!(aggregate_sum(&misrouted, payload), None);

        let mut wrong_value = complete_report(payload);
        let ScatterGatherTargetOutcome::Replied(reply) = &mut wrong_value.outcomes[0].1 else {
            unreachable!()
        };
        reply.value += 1;
        assert_eq!(aggregate_sum(&wrong_value, payload), None);

        let mut partial = complete_report(payload);
        partial.outcomes[0].1 = ScatterGatherTargetOutcome::Timeout;
        assert_eq!(aggregate_sum(&partial, payload), None);
        let incorrect = incorrect_aggregate(&partial, payload);
        assert_eq!(incorrect.expected_targets, WORKERS);
        assert_eq!(incorrect.targets[0].outcome, TargetOutcome::Timeout);
    }

    #[test]
    fn operation_capacity_returns_typed_full_without_stranding_callers() {
        let (report, outcome) =
            run_with_max_in_flight(1, true).expect("capacity-one run completed");
        assert_eq!(report.aggregates_correct, 1);
        assert_eq!(report.aggregates_wrong, 0);
        assert_eq!(report.failed, CLIENTS - 1);
        assert_eq!(outcome.coordinator_full, CLIENTS - 1);
        assert_eq!(outcome.refill_correct, 1);
        assert_eq!(outcome.coordinator_mailbox_full, 0);
        assert_eq!(outcome.coordinator_closed, 0);
        assert_eq!(outcome.coordinator_timeout, 0);
        assert!(outcome.coordinator_rejected.is_empty());
        assert!(outcome.start_rejected.is_empty());
        assert_eq!(outcome.correct + outcome.wrong + outcome.failed, CLIENTS);
        assert!(report.exit_clean);
    }
}
