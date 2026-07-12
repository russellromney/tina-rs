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
    ThreadedRuntime, call_cancelable, call_request,
};

use crate::{CLIENTS, MAX_IN_FLIGHT, Report, WORKERS, expected_aggregate};

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

// --- Worker ---------------------------------------------------------------

#[derive(Debug)]
enum WorkerMsg {
    Do(u64),
}

#[derive(Debug, Clone, Copy)]
struct WorkerReply(u64);

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
            WorkerMsg::Do(payload) => WorkerReply(payload.wrapping_add(self.id)),
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

#[derive(Debug, Default, Clone, Copy)]
struct DriverOutcome {
    correct: usize,
    wrong: usize,
    failed: usize,
    coordinator_full: usize,
}

#[derive(Debug)]
enum DriverMsg {
    Begin,
    Returned(usize, CallOutcome<AggregateReply>),
}

struct Driver {
    coord: tina::ServiceRequestAddress<CoordEvent, CoordRequest, AggregateReply>,
    payloads: Vec<u64>,
    remaining: usize,
    outcome: DriverOutcome,
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
                let calls: Vec<_> = self
                    .payloads
                    .iter()
                    .copied()
                    .enumerate()
                    .map(|(i, payload)| {
                        call_request(coord, CoordRequest::Query(payload), QUERY_TIMEOUT)
                            .then(move |outcome| DriverMsg::Returned(i, outcome))
                    })
                    .collect();
                Effect::Batch(calls)
            }
            DriverMsg::Returned(i, outcome) => {
                let payload = self.payloads[i];
                match outcome {
                    CallOutcome::Replied(AggregateReply::Complete(report))
                        if aggregate_sum(&report) == Some(expected_aggregate(payload)) =>
                    {
                        self.outcome.correct += 1;
                    }
                    CallOutcome::Replied(AggregateReply::Complete(_)) => self.outcome.wrong += 1,
                    CallOutcome::Replied(AggregateReply::Full) => {
                        self.outcome.coordinator_full += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Replied(AggregateReply::StartRejected(error)) => {
                        let _ = error;
                        self.outcome.failed += 1;
                    }
                    _ => self.outcome.failed += 1,
                }
                self.remaining -= 1;
                if self.remaining == 0 {
                    stop_with(self.outcome)
                } else {
                    noop()
                }
            }
        }
    }
}

fn aggregate_sum(report: &ScatterGatherReport<WorkerReply, usize>) -> Option<u64> {
    report
        .outcomes
        .iter()
        .try_fold(0u64, |sum, (_, outcome)| match outcome {
            ScatterGatherTargetOutcome::Replied(WorkerReply(value)) => {
                Some(sum.wrapping_add(*value))
            }
            ScatterGatherTargetOutcome::Full
            | ScatterGatherTargetOutcome::Closed
            | ScatterGatherTargetOutcome::Timeout
            | ScatterGatherTargetOutcome::Rejected(_)
            | ScatterGatherTargetOutcome::AggregateTimeout
            | ScatterGatherTargetOutcome::MissingShard => None,
        })
}

// --- Run ------------------------------------------------------------------

pub fn run() -> anyhow::Result<Report> {
    run_with_max_in_flight(MAX_IN_FLIGHT).map(|(report, _)| report)
}

fn run_with_max_in_flight(max_in_flight: usize) -> anyhow::Result<(Report, DriverOutcome)> {
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

    let payloads: Vec<u64> = (0..CLIENTS as u64)
        .map(|c| c.wrapping_mul(7).wrapping_add(11))
        .collect();
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                coord,
                payloads,
                remaining: CLIENTS,
                outcome: DriverOutcome::default(),
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
        },
        outcome,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_capacity_returns_typed_full_without_stranding_callers() {
        let (report, outcome) = run_with_max_in_flight(1).expect("capacity-one run completed");
        assert_eq!(report.aggregates_correct, 1);
        assert_eq!(report.aggregates_wrong, 0);
        assert_eq!(report.failed, CLIENTS - 1);
        assert_eq!(outcome.coordinator_full, CLIENTS - 1);
        assert!(report.exit_clean);
    }
}
