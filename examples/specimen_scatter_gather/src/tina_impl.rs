//! Tina: a `Coordinator` isolate that captures one
//! [`tina::DeferredReply`] per inbound client query, dispatches a
//! runtime call to each worker, gathers per-target replies, and
//! answers the client through the captured slot. The pending box
//! has a named cap (`MAX_IN_FLIGHT`) and visible counters.
//!
//! Driver: one `Driver` isolate batches `CLIENTS` parallel calls
//! against the coordinator, accumulates outcomes, and `stop_with`s
//! a typed result. The host reads the result via
//! `runtime.observe_result`.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ParkError, PendingReplies, ThreadedRuntime,
    call_request, request_effect_after_park,
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

/// Internal event: one worker's per-query reply, never caller authority.
#[derive(Debug)]
enum CoordEvent {
    WorkerDone(u64, CallOutcome<WorkerReply>),
}

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum CoordRequest {
    Query(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AggregateReply {
    /// Sum of per-worker contributions for this query.
    Ok(u64),
    /// Coordinator's pending box was full; query was not admitted.
    Full,
}

#[derive(Debug)]
struct PartialQuery {
    qid: u64,
    sum: u64,
    remaining: usize,
}

struct Coordinator {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    pending: PendingReplies<u64, AggregateReply>,
    next_qid: u64,
    partials: Vec<PartialQuery>,
}

#[tina_runtime::isolate(event = CoordEvent, request = CoordRequest, reply = AggregateReply)]
impl Coordinator {
    fn handle_event(
        &mut self,
        event: CoordEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            CoordEvent::WorkerDone(qid, outcome) => {
                let Some(idx) = self.partials.iter().position(|p| p.qid == qid) else {
                    return noop();
                };
                let partial = &mut self.partials[idx];
                if let CallOutcome::Replied(WorkerReply(v)) = outcome {
                    partial.sum = partial.sum.wrapping_add(v);
                }
                partial.remaining -= 1;
                if partial.remaining == 0 {
                    let done = self.partials.remove(idx);
                    if let Some(slot) = self.pending.take(&qid) {
                        return reply_to(slot, AggregateReply::Ok(done.sum));
                    }
                }
                noop()
            }
        }
    }

    fn handle_request(
        &mut self,
        request: CoordRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            CoordRequest::Query(payload) => {
                let qid = self.next_qid;
                self.next_qid += 1;
                match self.pending.park_request(qid, call) {
                    Ok((_ticket, permit)) => {
                        self.partials.push(PartialQuery {
                            qid,
                            sum: 0,
                            remaining: self.workers.len(),
                        });
                        let calls: Vec<_> = self
                            .workers
                            .iter()
                            .map(|w| {
                                tina_runtime::call(*w, WorkerMsg::Do(payload), QUERY_TIMEOUT)
                                    .then_service_event(move |outcome| {
                                        CoordEvent::WorkerDone(qid, outcome)
                                    })
                            })
                            .collect();
                        request_effect_after_park(permit, Effect::Batch(calls))
                    }
                    // Pending box full: reply Full to the caller
                    // without capturing. The caller observes a typed
                    // overload distinct from a successful aggregate.
                    Err(ParkError::Full { call, .. }) => call.reply(AggregateReply::Full),
                    Err(ParkError::DuplicateKey { call, .. }) => call.reply(AggregateReply::Full),
                }
            }
        }
    }
}

// --- Driver ---------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct DriverOutcome {
    correct: usize,
    wrong: usize,
    failed: usize,
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
                    CallOutcome::Replied(AggregateReply::Ok(sum))
                        if sum == expected_aggregate(payload) =>
                    {
                        self.outcome.correct += 1;
                    }
                    CallOutcome::Replied(AggregateReply::Ok(_)) => self.outcome.wrong += 1,
                    CallOutcome::Replied(AggregateReply::Full) => self.outcome.failed += 1,
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

// --- Run ------------------------------------------------------------------

pub fn run() -> anyhow::Result<Report> {
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
                pending: PendingReplies::with_capacity(MAX_IN_FLIGHT),
                next_qid: 1,
                partials: Vec::with_capacity(MAX_IN_FLIGHT),
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

    Ok(Report {
        clients: CLIENTS,
        workers: WORKERS,
        aggregates_correct: outcome.correct,
        aggregates_wrong: outcome.wrong,
        failed: outcome.failed,
        exit_clean: true,
    })
}
