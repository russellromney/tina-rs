use std::convert::Infallible;
use std::time::Duration;

use tina::CallRejectedReason;
use tina::prelude::*;
use tina_runtime::{
    BoundedItems, CallError, CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, SleepReply,
    bounded_batch, call_request, sleep,
};

use crate::{CLIENTS, DRIVER_BURST_CAP, Report, WORKERS, expected_for};

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
    #[cfg(test)]
    Stop,
}

#[derive(Debug, Clone, Copy)]
enum WorkerReply {
    Result(u64),
    TimerFailed(CallError),
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
            WorkerEvent::Done(req, Err(error), _) => reply_to(req, WorkerReply::TimerFailed(error)),
            #[cfg(test)]
            WorkerEvent::Stop => stop(),
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
                call.defer(sleep(self.work))
                    .reply_service_event(move |req, reply| {
                        WorkerEvent::Done(req, reply, payload + id)
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
    WorkerDone(RequestContext<FrontendReply>, CallOutcome<WorkerReply>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendReply {
    Result(u64),
    WorkerTimerFailed(CallError),
    WorkerFull,
    WorkerClosed,
    WorkerTimeout,
    WorkerRejected(CallRejectedReason),
}

struct Frontend {
    workers: Vec<tina::ServiceRequestAddress<WorkerEvent, WorkerRequest, WorkerReply>>,
    next_worker: usize,
    call_timeout: Duration,
}

#[tina_runtime::isolate(event = FrontendEvent, request = FrontendRequest, reply = FrontendReply)]
impl Frontend {
    fn handle_event(
        &mut self,
        event: FrontendEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            FrontendEvent::WorkerDone(req, outcome) => {
                reply_to(req, frontend_reply_from_worker(outcome))
            }
        }
    }

    fn handle_request(
        &mut self,
        request: FrontendRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            FrontendRequest::Submit(payload) => {
                let worker = self.workers[self.next_worker];
                self.next_worker = (self.next_worker + 1) % self.workers.len();
                call.defer(call_request(
                    worker,
                    WorkerRequest::Do(payload),
                    self.call_timeout,
                ))
                .reply_service_event(FrontendEvent::WorkerDone)
            }
        }
    }
}

fn frontend_reply_from_worker(outcome: CallOutcome<WorkerReply>) -> FrontendReply {
    match outcome {
        CallOutcome::Replied(WorkerReply::Result(value)) => FrontendReply::Result(value),
        CallOutcome::Replied(WorkerReply::TimerFailed(error)) => {
            FrontendReply::WorkerTimerFailed(error)
        }
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
                    DRIVER_BURST_CAP,
                    self.payloads.iter().copied().enumerate(),
                )
                .expect("driver workload must fit the configured burst cap");
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
                record_driver_outcome(&mut self.outcome, want, outcome);
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

fn record_driver_outcome(
    outcome: &mut DriverOutcome,
    expected: u64,
    returned: CallOutcome<FrontendReply>,
) {
    match returned {
        CallOutcome::Replied(FrontendReply::Result(got)) if got == expected => {
            outcome.correct += 1;
        }
        CallOutcome::Replied(FrontendReply::Result(_)) => outcome.wrong += 1,
        CallOutcome::Replied(
            FrontendReply::WorkerTimerFailed(_)
            | FrontendReply::WorkerFull
            | FrontendReply::WorkerClosed
            | FrontendReply::WorkerTimeout
            | FrontendReply::WorkerRejected(_),
        )
        | CallOutcome::Full
        | CallOutcome::Closed
        | CallOutcome::Timeout
        | CallOutcome::Rejected(_) => outcome.failed += 1,
    }
}

pub fn run() -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;

    let mut workers = Vec::with_capacity(WORKERS);
    for w in 0..WORKERS as u64 {
        // Vary work time so replies are out of order.
        let work = Duration::from_millis(5 + (w * 7) % 20);
        workers.push(
            app.register_split_service::<Worker, WorkerEvent, WorkerRequest, Infallible>(
                Worker { id: w, work },
                16,
            )
            .map_err(|e| anyhow::anyhow!("register worker: {e:?}"))?
            .requests,
        );
    }

    let frontend = app
        .register_split_service::<Frontend, FrontendEvent, FrontendRequest, Infallible>(
            Frontend {
                workers,
                next_worker: 0,
                call_timeout: CALL_TIMEOUT,
            },
            32,
        )
        .map_err(|e| anyhow::anyhow!("register frontend: {e:?}"))?
        .requests;

    let payloads: Vec<u64> = (0..CLIENTS as u64)
        .map(|c| c.wrapping_mul(11) + 1)
        .collect();
    let driver = app
        .register_root::<_, Infallible>(
            Driver {
                frontend,
                payloads,
                remaining: CLIENTS,
                outcome: DriverOutcome::default(),
            },
            32,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = app
        .observe_result::<DriverOutcome, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;
    app.try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick driver: {e:?}"))?;
    let outcome = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("driver finishes: {e:?}"))?;

    app.shutdown().drain().join_report().ensure_clean()?;

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
    use tina_runtime::{
        CallKind, DeferredReplyRejectedReason, LocalSystemConfig, RuntimeEventKind,
    };

    type TestApp = LocalSystem<SingleShard, DefaultThreadedMailboxFactory>;

    fn test_app() -> TestApp {
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
            .try_build()
            .expect("start test app")
    }

    fn register_frontend(
        app: &TestApp,
        worker: tina::ServiceRequestAddress<WorkerEvent, WorkerRequest, WorkerReply>,
        call_timeout: Duration,
    ) -> tina::ServiceRequestAddress<FrontendEvent, FrontendRequest, FrontendReply> {
        app.register_split_service::<Frontend, FrontendEvent, FrontendRequest, Infallible>(
            Frontend {
                workers: vec![worker],
                next_worker: 0,
                call_timeout,
            },
            8,
        )
        .expect("register frontend")
        .requests
    }

    fn call_frontend(
        app: &TestApp,
        frontend: tina::ServiceRequestAddress<FrontendEvent, FrontendRequest, FrontendReply>,
    ) -> CallOutcome<FrontendReply> {
        app.call_blocking_request(frontend, FrontendRequest::Submit(7), Duration::from_secs(1))
            .expect("frontend host call")
    }

    fn shutdown_clean(app: TestApp) {
        app.shutdown()
            .drain()
            .join_report()
            .ensure_clean()
            .expect("clean test shutdown");
    }

    struct RejectWorker;

    #[tina_runtime::isolate(event = WorkerEvent, request = WorkerRequest, reply = WorkerReply)]
    impl RejectWorker {
        fn handle_event(
            &mut self,
            event: WorkerEvent,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            match event {
                WorkerEvent::Done(req, _, _) => {
                    reply_to(req, WorkerReply::TimerFailed(CallError::InvariantViolation))
                }
                WorkerEvent::Stop => stop(),
            }
        }

        fn handle_request(
            &mut self,
            _request: WorkerRequest,
            call: RequestCall<'_, Self>,
        ) -> RequestEffect<Self> {
            call.reject(CallRejectedReason::UnsupportedMessage)
        }
    }

    #[derive(Debug)]
    enum TimerHolderMsg {
        Start,
        Done(SleepReply),
    }

    struct TimerHolder;

    #[tina_runtime::isolate(message = TimerHolderMsg)]
    impl TimerHolder {
        fn handle(
            &mut self,
            message: TimerHolderMsg,
            _ctx: &mut Context<'_, SingleShard>,
        ) -> Effect<Self> {
            match message {
                TimerHolderMsg::Start => sleep(Duration::from_secs(5)).then(TimerHolderMsg::Done),
                TimerHolderMsg::Done(_outcome) => noop(),
            }
        }
    }

    fn wait_for_timer_dispatch(app: &TestApp) {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            let trace = app.complete_trace().expect("read complete trace");
            if trace.iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallDispatchAttempted {
                        call_kind: CallKind::Sleep,
                        ..
                    }
                )
            }) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timer holder did not arm"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn worker_terminal_outcomes_remain_distinct_frontend_replies() {
        let expected = tina::SystemIncarnation::new(1);
        let actual = tina::SystemIncarnation::new(2);
        assert_eq!(
            frontend_reply_from_worker(CallOutcome::Replied(WorkerReply::Result(42))),
            FrontendReply::Result(42)
        );
        assert_eq!(
            frontend_reply_from_worker(CallOutcome::Replied(WorkerReply::TimerFailed(
                CallError::TimerFull,
            ))),
            FrontendReply::WorkerTimerFailed(CallError::TimerFull)
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
            CallRejectedReason::ForeignSystem { expected, actual },
            CallRejectedReason::ReplyAbandoned,
            CallRejectedReason::HandlerPanicked,
            CallRejectedReason::UnsupportedMessage,
        ] {
            assert_eq!(
                frontend_reply_from_worker(CallOutcome::Rejected(reason)),
                FrontendReply::WorkerRejected(reason)
            );
        }
    }

    #[test]
    fn deferred_frontend_preserves_live_worker_full_closed_timeout_and_rejected() {
        let app = test_app();
        let full_worker = app
            .register_split_service::<Worker, WorkerEvent, WorkerRequest, Infallible>(
                Worker {
                    id: 0,
                    work: Duration::ZERO,
                },
                0,
            )
            .expect("register zero-capacity worker");
        let frontend = register_frontend(&app, full_worker.requests, Duration::from_secs(1));
        assert_eq!(
            call_frontend(&app, frontend),
            CallOutcome::Replied(FrontendReply::WorkerFull)
        );
        shutdown_clean(app);

        let app = test_app();
        let closed_worker = app
            .register_split_service::<Worker, WorkerEvent, WorkerRequest, Infallible>(
                Worker {
                    id: 0,
                    work: Duration::ZERO,
                },
                4,
            )
            .expect("register closeable worker");
        let stopped = app
            .observe_isolate_complete(closed_worker.requests.address().address())
            .expect("register worker stop observer");
        app.try_send_event(closed_worker.events, WorkerEvent::Stop)
            .expect("admit worker stop");
        stopped.wait(Duration::from_secs(1)).expect("worker stops");
        let frontend = register_frontend(&app, closed_worker.requests, Duration::from_secs(1));
        assert_eq!(
            call_frontend(&app, frontend),
            CallOutcome::Replied(FrontendReply::WorkerClosed)
        );
        shutdown_clean(app);

        let app = test_app();
        let slow_worker = app
            .register_split_service::<Worker, WorkerEvent, WorkerRequest, Infallible>(
                Worker {
                    id: 0,
                    work: Duration::from_millis(80),
                },
                4,
            )
            .expect("register slow worker");
        let frontend = register_frontend(&app, slow_worker.requests, Duration::from_millis(10));
        assert_eq!(
            call_frontend(&app, frontend),
            CallOutcome::Replied(FrontendReply::WorkerTimeout)
        );
        std::thread::sleep(Duration::from_millis(100));
        let late_replies = app
            .complete_trace()
            .expect("read timeout trace")
            .iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::DeferredReplyRejected {
                        reason: DeferredReplyRejectedReason::CallerTimedOut,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(late_replies, 1);
        shutdown_clean(app);

        let app = test_app();
        let rejected_worker = app
            .register_split_service::<RejectWorker, WorkerEvent, WorkerRequest, Infallible>(
                RejectWorker,
                4,
            )
            .expect("register rejecting worker");
        let frontend = register_frontend(&app, rejected_worker.requests, Duration::from_secs(1));
        assert_eq!(
            call_frontend(&app, frontend),
            CallOutcome::Replied(FrontendReply::WorkerRejected(
                CallRejectedReason::UnsupportedMessage
            ))
        );
        shutdown_clean(app);
    }

    #[test]
    fn deferred_frontend_reports_timer_full_without_collapsing_call_error() {
        let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
            .config(LocalSystemConfig {
                timer_capacity: 1,
                ..LocalSystemConfig::default()
            })
            .try_build()
            .expect("start one-timer app");
        let holder = app
            .register_root::<TimerHolder, Infallible>(TimerHolder, 4)
            .expect("register timer holder");
        app.try_send(holder, TimerHolderMsg::Start)
            .expect("start held timer");
        wait_for_timer_dispatch(&app);

        let worker = app
            .register_split_service::<Worker, WorkerEvent, WorkerRequest, Infallible>(
                Worker {
                    id: 0,
                    work: Duration::from_millis(1),
                },
                4,
            )
            .expect("register timer-full worker");
        let frontend = register_frontend(&app, worker.requests, Duration::from_secs(1));
        assert_eq!(
            call_frontend(&app, frontend),
            CallOutcome::Replied(FrontendReply::WorkerTimerFailed(CallError::TimerFull))
        );
        shutdown_clean(app);
    }

    #[test]
    fn caller_timeout_settles_deferred_authority_once_without_late_reply() {
        let app = test_app();
        let worker = app
            .register_split_service::<Worker, WorkerEvent, WorkerRequest, Infallible>(
                Worker {
                    id: 0,
                    work: Duration::from_millis(60),
                },
                4,
            )
            .expect("register slow worker");
        let frontend = register_frontend(&app, worker.requests, Duration::from_secs(1));
        assert_eq!(
            app.call_blocking_request(
                frontend,
                FrontendRequest::Submit(7),
                Duration::from_millis(10),
            )
            .expect("frontend host call"),
            CallOutcome::Timeout
        );
        std::thread::sleep(Duration::from_millis(100));

        let trace = app.complete_trace().expect("read caller timeout trace");
        let caller_timeouts = trace
            .iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::DeferredReplyRejected {
                        reason: DeferredReplyRejectedReason::CallerTimedOut,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(caller_timeouts, 1);
        shutdown_clean(app);
    }

    #[test]
    fn driver_maps_out_of_order_completions_by_captured_input_index() {
        let payloads: Vec<u64> = (0..CLIENTS as u64)
            .map(|client| client.wrapping_mul(11) + 1)
            .collect();
        let mut outcome = DriverOutcome::default();
        for i in (0..CLIENTS).rev() {
            let worker_id = (i % WORKERS) as u64;
            let expected = expected_for(payloads[i], worker_id);
            record_driver_outcome(
                &mut outcome,
                expected,
                CallOutcome::Replied(FrontendReply::Result(expected)),
            );
        }
        assert_eq!(outcome.correct, CLIENTS);
        assert_eq!(outcome.wrong, 0);
        assert_eq!(outcome.failed, 0);
    }

    #[test]
    fn driver_rejects_over_cap_input_before_effect_construction() {
        const { assert!(CLIENTS <= DRIVER_BURST_CAP) };
        assert!(
            BoundedItems::try_from_iter(DRIVER_BURST_CAP, 0..DRIVER_BURST_CAP.saturating_add(1))
                .is_err()
        );
    }
}
