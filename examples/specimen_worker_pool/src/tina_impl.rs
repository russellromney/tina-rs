use std::convert::Infallible;
use std::time::Duration;

use tina::CallRejectedReason;
use tina::prelude::*;
use tina_runtime::{
    BoundedItems, CallOutcome, DefaultThreadedMailboxFactory, ParkError, PendingReplies,
    SleepReply, ThreadedRuntime, bounded_batch, call_request, request_effect_after_park, sleep,
};

use crate::{CLIENTS, MAX_PENDING, Report, WORKERS, expected_for};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);

// --- Worker -------------------------------------------------------------

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum WorkerRequest {
    Do(u64),
}

/// Internal event: the work-timer continuation.
#[derive(Debug)]
enum WorkerEvent {
    Done(RequestContext<WorkerReply>, SleepReply, u64),
}

#[derive(Debug, Clone, Copy)]
enum WorkerReply {
    Result(u64),
    TimerFailed,
}

struct Worker {
    id: u64,
    work: Duration,
}

#[tina_runtime::isolate(event = WorkerEvent, request = WorkerRequest, reply = WorkerReply)]
impl Worker {
    fn handle_event(
        &mut self,
        event: WorkerEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            WorkerEvent::Done(req, Ok(()), result) => reply_to(req, WorkerReply::Result(result)),
            WorkerEvent::Done(req, Err(_), _) => reply_to(req, WorkerReply::TimerFailed),
        }
    }

    fn handle_request(
        &mut self,
        request: WorkerRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            // Vary the wait by id so replies arrive out of dispatch order.
            WorkerRequest::Do(payload) => {
                let id = self.id;
                call.defer(sleep(self.work)).reply(move |req, reply| {
                    tina::ServiceMessage::Event(WorkerEvent::Done(req, reply, payload + id))
                })
            }
        }
    }
}

// --- Frontend -----------------------------------------------------------

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum FrontendRequest {
    Submit(u64),
}

/// Internal event: one worker's call outcome landing back at the frontend.
#[derive(Debug)]
enum FrontendEvent {
    WorkerDone(u64, CallOutcome<WorkerReply>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendReply {
    Result(u64),
    PendingFull,
    DuplicateRequest,
    WorkerTimerFailed,
    WorkerFull,
    WorkerClosed,
    WorkerTimeout,
    WorkerRejected(CallRejectedReason),
}

struct Frontend {
    workers: Vec<tina::ServiceRequestAddress<WorkerEvent, WorkerRequest, WorkerReply>>,
    pending: PendingReplies<u64, FrontendReply>,
    next_qid: u64,
    next_worker: usize,
}

#[tina_runtime::isolate(event = FrontendEvent, request = FrontendRequest, reply = FrontendReply)]
impl Frontend {
    fn handle_event(
        &mut self,
        event: FrontendEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            FrontendEvent::WorkerDone(qid, outcome) => self.on_worker_done(qid, outcome),
        }
    }

    fn handle_request(
        &mut self,
        request: FrontendRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            FrontendRequest::Submit(payload) => {
                let qid = self.next_qid;
                self.next_qid += 1;
                match self.pending.park_request(qid, call) {
                    Ok((_ticket, permit)) => {
                        let worker = self.workers[self.next_worker];
                        self.next_worker = (self.next_worker + 1) % self.workers.len();
                        let dispatch_effect = call_request(worker, WorkerRequest::Do(payload), CALL_TIMEOUT)
                            .then(move |outcome| {
                                tina::ServiceMessage::Event(FrontendEvent::WorkerDone(qid, outcome))
                            });
                        request_effect_after_park(permit, dispatch_effect)
                    }
                    Err(ParkError::Full { call, .. }) => call.reply(FrontendReply::PendingFull),
                    Err(ParkError::DuplicateKey { call, .. }) => {
                        call.reply(FrontendReply::DuplicateRequest)
                    }
                }
            }
        }
    }
}

impl Frontend {
    fn on_worker_done(&mut self, qid: u64, outcome: CallOutcome<WorkerReply>) -> Effect<Self> {
        let Some(slot) = self.pending.take(&qid) else {
            return noop();
        };
        reply_to(slot, frontend_reply_from_worker(outcome))
    }
}

fn frontend_reply_from_worker(outcome: CallOutcome<WorkerReply>) -> FrontendReply {
    match outcome {
        CallOutcome::Replied(WorkerReply::Result(value)) => FrontendReply::Result(value),
        CallOutcome::Replied(WorkerReply::TimerFailed) => FrontendReply::WorkerTimerFailed,
        CallOutcome::Full => FrontendReply::WorkerFull,
        CallOutcome::Closed => FrontendReply::WorkerClosed,
        CallOutcome::Timeout => FrontendReply::WorkerTimeout,
        CallOutcome::Rejected(reason) => FrontendReply::WorkerRejected(reason),
    }
}

// --- Driver -------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct DriverOutcome {
    correct: usize,
    wrong: usize,
    failed: usize,
}

#[derive(Debug)]
enum DriverMsg {
    Begin,
    Returned(usize, CallOutcome<FrontendReply>),
}

struct Driver {
    frontend: tina::ServiceRequestAddress<FrontendEvent, FrontendRequest, FrontendReply>,
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
                let frontend = self.frontend;
                let payloads = BoundedItems::try_from_iter(
                    MAX_PENDING,
                    self.payloads.iter().copied().enumerate(),
                )
                .expect("driver workload must fit the frontend pending bound");
                let calls = payloads.map_effects(|(i, payload)| {
                    call_request(frontend, FrontendRequest::Submit(payload), CALL_TIMEOUT)
                        .then(move |outcome| DriverMsg::Returned(i, outcome))
                });
                bounded_batch(calls)
            }
            DriverMsg::Returned(i, outcome) => {
                let payload = self.payloads[i];
                // Round-robin worker assignment is deterministic.
                let worker_id = (i % WORKERS) as u64;
                let want = expected_for(payload, worker_id);
                match outcome {
                    CallOutcome::Replied(FrontendReply::Result(got)) if got == want => {
                        self.outcome.correct += 1;
                    }
                    CallOutcome::Replied(FrontendReply::Result(_)) => self.outcome.wrong += 1,
                    CallOutcome::Replied(
                        FrontendReply::PendingFull
                        | FrontendReply::DuplicateRequest
                        | FrontendReply::WorkerTimerFailed
                        | FrontendReply::WorkerFull
                        | FrontendReply::WorkerClosed
                        | FrontendReply::WorkerTimeout
                        | FrontendReply::WorkerRejected(_),
                    )
                    | CallOutcome::Full
                    | CallOutcome::Closed
                    | CallOutcome::Timeout
                    | CallOutcome::Rejected(_) => self.outcome.failed += 1,
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

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let mut workers = Vec::with_capacity(WORKERS);
    for w in 0..WORKERS as u64 {
        // Vary work time so replies are out of order.
        let work = Duration::from_millis(5 + (w * 7) % 20);
        workers.push(
            runtime
                .register_split_service::<Worker, WorkerEvent, WorkerRequest, Infallible>(
                    Worker { id: w, work },
                    16,
                )
                .map_err(|e| anyhow::anyhow!("register worker: {e:?}"))?
                .requests,
        );
    }

    let frontend = runtime
        .register_split_service::<Frontend, FrontendEvent, FrontendRequest, Infallible>(
            Frontend {
                workers,
                pending: PendingReplies::with_capacity(MAX_PENDING),
                next_qid: 1,
                next_worker: 0,
            },
            32,
        )
        .map_err(|e| anyhow::anyhow!("register frontend: {e:?}"))?
        .requests;

    let payloads: Vec<u64> = (0..CLIENTS as u64)
        .map(|c| c.wrapping_mul(11) + 1)
        .collect();
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                frontend,
                payloads,
                remaining: CLIENTS,
                outcome: DriverOutcome::default(),
            },
            32,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = runtime
        .observe_result::<DriverOutcome, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;
    runtime
        .try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick driver: {e:?}"))?;
    let outcome = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("driver finishes: {e:?}"))?;

    runtime
        .shutdown()
        .map_err(|e| anyhow::anyhow!("runtime shutdown: {e}"))?;

    Ok(Report {
        clients: CLIENTS,
        correct_replies: outcome.correct,
        wrong_replies: outcome.wrong,
        failed: outcome.failed,
        exit_clean: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_terminal_outcomes_remain_distinct_frontend_replies() {
        assert_eq!(
            frontend_reply_from_worker(CallOutcome::Replied(WorkerReply::Result(42))),
            FrontendReply::Result(42)
        );
        assert_eq!(
            frontend_reply_from_worker(CallOutcome::Replied(WorkerReply::TimerFailed)),
            FrontendReply::WorkerTimerFailed
        );
        assert_eq!(
            frontend_reply_from_worker(CallOutcome::Full),
            FrontendReply::WorkerFull
        );
        assert_eq!(
            frontend_reply_from_worker(CallOutcome::Closed),
            FrontendReply::WorkerClosed
        );
        assert_eq!(
            frontend_reply_from_worker(CallOutcome::Timeout),
            FrontendReply::WorkerTimeout
        );
        for reason in [
            CallRejectedReason::ReplyAbandoned,
            CallRejectedReason::HandlerPanicked,
            CallRejectedReason::UnsupportedMessage,
        ] {
            assert_eq!(
                frontend_reply_from_worker(CallOutcome::Rejected(reason)),
                FrontendReply::WorkerRejected(reason)
            );
        }
        assert_ne!(FrontendReply::PendingFull, FrontendReply::WorkerFull);
    }
}
