//! `system_job_queue` v2 — bounded worker pool with synchronous `Submit`,
//! cancel-while-running, and one-shot worker respawn on crash.
//!
//! v2 collapses two parallel pending structures (`PendingReplies` for parked
//! callers + `PendingCallSet` for in-flight call handles) into one
//! [`PendingCancelableCallSet`], using `RequestCall::defer_cancelable(...)
//! .try_admit(...)` as the admission gate. Total admission cap is `workers`;
//! there is no separate queue layer. A queued layer would just delay the
//! `Busy` reply and was buying nothing real.
//!
//! What this specimen pulls on:
//!
//! - [`PendingCancelableCallSet`] for caller authority + cancel handle as
//!   one token.
//! - `RequestCall::defer_cancelable(call_cancelable_request(...)).try_admit(...)`
//!   for "admit first, then dispatch" with a typed `Full` recovery path.
//! - `PendingCancelableCall::cancel(translator)` for cancel-while-running:
//!   one effect closes the wait AND routes the parked request context into
//!   the cancel continuation.
//! - `spawn_observed(ChildDefinition::new(...)).then_service_event(...)` for
//!   typed child start results (and exact child-start failures).
//! - Host readiness and quiescence as parked typed requests (`AwaitReady`,
//!   `AwaitQuiescent`) rather than a readiness mutex or host spin loop.
//! - Runtime-owned `sleep` as the worker's only async surface.
//! - The `Worker` isolate uses the `event = .. request = ..` split-service
//!   macro form: `WorkerEvent::Wake` is fire-and-forget, while
//!   `WorkerRequest` carries typed `Process` and `Cancel` calls. The
//!   macro generates both rejection arms, so neither `handle_event` nor
//!   `handle_request` writes one by hand.
//! - `LocalSystem::register_split_service_with_bootstrap` registers the `Queue`
//!   isolate with its startup `Bootstrap` event prefilled atomically.

use std::fmt;
use std::sync::Barrier;
use std::thread;
use std::time::Duration;

use tina::{ChildDefinition, SpawnObservedError, prelude::*};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, PendingCancelableCallSet,
    PendingCancelableRemoveError, PendingCancelableTicket, RequestPendingCancelableInsertError,
    SleepReply, SplitServiceHandle, call_cancelable_request, call_request, sleep,
};

const MAX_WORKERS: usize = 256;
const MAX_MAILBOX_CAPACITY: usize = 65_536;
const MAX_JOB_SLEEP_MS: u64 = 60_000;
const MAX_CALL_TIMEOUT_MS: u64 = 120_000;
const OVERFLOW_EXTRA_CALLERS: usize = 3;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_CANCEL_RETRY_DELAY: Duration = Duration::from_millis(1);
const MAX_WORKER_CANCEL_RETRIES: u8 = 3;

/// Tunables for one specimen run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunConfig {
    pub workers: usize,
    pub queue_mailbox: usize,
    pub worker_mailbox: usize,
    pub job_sleep_ms: u64,
    pub call_timeout_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            queue_mailbox: 64,
            worker_mailbox: 8,
            job_sleep_ms: 80,
            call_timeout_ms: 5_000,
        }
    }
}

impl RunConfig {
    /// Validates every value that controls an allocation, caller thread, or
    /// wait before the runtime is constructed.
    pub fn validate(self) -> Result<Self, RunConfigError> {
        nonzero_bounded("workers", self.workers, MAX_WORKERS)?;
        nonzero_bounded("queue_mailbox", self.queue_mailbox, MAX_MAILBOX_CAPACITY)?;
        nonzero_bounded("worker_mailbox", self.worker_mailbox, MAX_MAILBOX_CAPACITY)?;
        nonzero_bounded_u64("job_sleep_ms", self.job_sleep_ms, MAX_JOB_SLEEP_MS)?;
        nonzero_bounded_u64("call_timeout_ms", self.call_timeout_ms, MAX_CALL_TIMEOUT_MS)?;

        let burst = self
            .workers
            .checked_add(OVERFLOW_EXTRA_CALLERS)
            .ok_or(RunConfigError::DerivedCountOverflow("overflow burst"))?;
        burst
            .checked_add(1)
            .ok_or(RunConfigError::DerivedCountOverflow(
                "overflow barrier participants",
            ))?;
        self.job_sleep_ms
            .checked_mul(4)
            .and_then(|value| value.checked_add(1_000))
            .ok_or(RunConfigError::DerivedDurationOverflow(
                "worker dispatch timeout",
            ))?;
        Ok(self)
    }

    fn overflow_shape(self) -> Result<(usize, usize), RunConfigError> {
        let burst = self
            .workers
            .checked_add(OVERFLOW_EXTRA_CALLERS)
            .ok_or(RunConfigError::DerivedCountOverflow("overflow burst"))?;
        let participants = burst
            .checked_add(1)
            .ok_or(RunConfigError::DerivedCountOverflow(
                "overflow barrier participants",
            ))?;
        Ok((burst, participants))
    }

    fn dispatch_timeout(self) -> Duration {
        let millis = self
            .job_sleep_ms
            .checked_mul(4)
            .and_then(|value| value.checked_add(1_000))
            .expect("RunConfig::validate checked the dispatch timeout");
        Duration::from_millis(millis)
    }
}

fn nonzero_bounded(field: &'static str, value: usize, max: usize) -> Result<(), RunConfigError> {
    if value == 0 {
        return Err(RunConfigError::Zero(field));
    }
    if value > max {
        return Err(RunConfigError::TooLarge { field, value, max });
    }
    Ok(())
}

fn nonzero_bounded_u64(field: &'static str, value: u64, max: u64) -> Result<(), RunConfigError> {
    if value == 0 {
        return Err(RunConfigError::Zero(field));
    }
    if value > max {
        return Err(RunConfigError::DurationTooLarge { field, value, max });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunConfigError {
    Zero(&'static str),
    TooLarge {
        field: &'static str,
        value: usize,
        max: usize,
    },
    DurationTooLarge {
        field: &'static str,
        value: u64,
        max: u64,
    },
    DerivedCountOverflow(&'static str),
    DerivedDurationOverflow(&'static str),
}

impl fmt::Display for RunConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero(field) => write!(formatter, "{field} must be greater than zero"),
            Self::TooLarge { field, value, max } => {
                write!(formatter, "{field}={value} exceeds maximum {max}")
            }
            Self::DurationTooLarge { field, value, max } => {
                write!(formatter, "{field}={value}ms exceeds maximum {max}ms")
            }
            Self::DerivedCountOverflow(field) => {
                write!(formatter, "{field} overflowed usize")
            }
            Self::DerivedDurationOverflow(field) => {
                write!(formatter, "{field} overflowed u64 milliseconds")
            }
        }
    }
}

impl std::error::Error for RunConfigError {}

/// Job identity. Monotonic per queue; never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobId(pub u64);

/// Per-job payload. `Poison` panics inside the worker so the queue can
/// observe `CallOutcome::Closed` and exercise respawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Work(u32),
    Poison,
}

/// What [`QueueRequest::Submit`] eventually replies with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Completed { id: JobId, value: u32 },
    Cancelled { id: JobId },
    Failed { id: JobId, reason: String },
}

/// Queue stats snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStats {
    pub workers: usize,
    pub workers_alive: usize,
    pub in_flight: usize,
    pub pending_callers: usize,
    pub jobs_admitted: u64,
    pub jobs_busy_rejected: u64,
    pub jobs_completed: u64,
    pub jobs_cancelled: u64,
    pub jobs_failed: u64,
    pub worker_crashes: u64,
    pub worker_respawns: u64,
    pub cancel_reconciliation_failures: u64,
}

/// Replies the queue produces to host callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueReply {
    Done(JobOutcome),
    Busy,
    Cancelled(JobId),
    NotFound,
    Stats(QueueStats),
    Ready,
    Quiescent,
    StartupFailed(SpawnObservedError),
}

/// Fire-and-forget facts the queue accepts: bootstrap, spawn/call
/// continuations, and cancel acks. None of these carry caller authority
/// from an outside host.
#[derive(Debug)]
pub enum QueueEvent {
    Bootstrap,
    WorkerStarted {
        slot: usize,
        result: tina::SpawnObservedResult<WorkerMsg, WorkerReply>,
    },
    WorkerCallReturned {
        slot: usize,
        id: JobId,
        ticket: PendingCancelableTicket,
        outcome: CallOutcome<WorkerReply>,
    },
    WorkerCancelReturned {
        slot: usize,
        id: JobId,
        attempt: u8,
        outcome: CallOutcome<WorkerReply>,
    },
    RetryWorkerCancel {
        slot: usize,
        id: JobId,
        attempt: u8,
        result: SleepReply,
    },
    /// Continuation from `PendingCancelableCall::cancel`. Carries the parked
    /// caller's request context so we can answer them after the cancel lands.
    ParkedCallerCancelled {
        id: JobId,
        req: tina::RequestContext<QueueReply>,
        outcome: tina::CancelOutcome,
    },
}

/// Caller-authority requests the host can ask the queue.
#[derive(Debug)]
pub enum QueueRequest {
    Submit(Payload),
    Cancel(JobId),
    Stats,
    /// Parks until every worker slot has a live child, or returns the first
    /// exact child-start failure seen during bootstrap/replacement.
    AwaitReady,
    /// Parks until no job is in flight and no caller is parked.
    AwaitQuiescent,
}

/// Private split-service envelope type used only in isolate attributes.
type QueueMsg = tina::ServiceMessage<QueueEvent, QueueRequest>;

/// Worker's reply to a `Process` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerReply {
    Completed(u32),
    Cancelled,
    Failed(String),
    CancelAcknowledged { id: JobId, released: bool },
}

/// Fire-and-forget facts a worker accepts.
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    /// Internal: the runtime-owned sleep finished.
    Wake { id: JobId, result: SleepReply },
}

/// The one caller-authority request a worker accepts.
#[derive(Debug, Clone)]
pub enum WorkerRequest {
    Process {
        id: JobId,
        payload: Payload,
        sleep_ms: u64,
    },
    Cancel(JobId),
}

/// Split-service envelope for `Worker`. The `event = .. request = ..`
/// isolate macro form generates the caller-authority rejection arms, so
/// neither `handle_event` nor `handle_request` writes one by hand.
type WorkerMsg = tina::ServiceMessage<WorkerEvent, WorkerRequest>;

// ---------- Worker isolate ----------

struct Worker {
    current: Option<WorkerCurrent>,
}

struct WorkerCurrent {
    id: JobId,
    payload: Payload,
    slot: tina::DeferredReply<WorkerReply>,
}

#[tina_runtime::isolate(event = WorkerEvent, request = WorkerRequest, reply = WorkerReply)]
impl Worker {
    fn handle_event(
        &mut self,
        event: WorkerEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            WorkerEvent::Wake { id, result } => {
                let Some(current) = self.current.take() else {
                    return noop();
                };
                if current.id != id {
                    self.current = Some(current);
                    return noop();
                }
                if let Err(error) = result {
                    return reply_to::<Self>(
                        current.slot,
                        WorkerReply::Failed(format!("worker sleep failed: {error:?}")),
                    );
                }
                match current.payload {
                    Payload::Poison => panic!("worker poisoned by job {id:?}"),
                    Payload::Work(n) => {
                        reply_to::<Self>(current.slot, WorkerReply::Completed(n.wrapping_mul(2)))
                    }
                }
            }
        }
    }

    fn handle_request(
        &mut self,
        request: WorkerRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            WorkerRequest::Process {
                id,
                payload,
                sleep_ms,
            } => {
                if self.current.is_some() {
                    return call.reject(tina::CallRejectedReason::ReplyAbandoned);
                }
                call.capture(move |req| {
                    let slot = req.into_deferred();
                    self.current = Some(WorkerCurrent { id, payload, slot });
                    sleep(Duration::from_millis(sleep_ms))
                        .then_service_event(move |result| WorkerEvent::Wake { id, result })
                })
            }
            WorkerRequest::Cancel(id) => match self.current.take() {
                Some(current) if current.id == id => call.reply_and(
                    WorkerReply::CancelAcknowledged { id, released: true },
                    vec![reply_to::<Self>(current.slot, WorkerReply::Cancelled)],
                ),
                Some(current) => {
                    self.current = Some(current);
                    call.reply(WorkerReply::CancelAcknowledged {
                        id,
                        released: false,
                    })
                }
                None => call.reply(WorkerReply::CancelAcknowledged {
                    id,
                    released: false,
                }),
            },
        }
    }
}

// ---------- Queue isolate ----------

struct Queue {
    config: RunConfig,
    pending: PendingCancelableCallSet<JobId, QueueReply, WorkerReply>,
    workers: Vec<Option<tina_runtime::SplitServiceHandle<WorkerEvent, WorkerRequest, WorkerReply>>>,
    worker_busy: Vec<Option<JobId>>,
    next_id: u64,
    stats: QueueStats,
    /// Host parked on [`QueueRequest::AwaitReady`].
    ready_waiter: Option<tina::RequestContext<QueueReply>>,
    /// Host parked on [`QueueRequest::AwaitQuiescent`].
    quiescent_waiter: Option<tina::RequestContext<QueueReply>>,
    /// First exact child-start failure observed during bootstrap/replacement.
    startup_error: Option<SpawnObservedError>,
}

#[tina_runtime::isolate(
    event = QueueEvent,
    request = QueueRequest,
    reply = QueueReply,
    io = tina_runtime::RuntimeCall<QueueMsg>,
    spawn_observed = tina::SpawnObserved<ChildDefinition<Worker>, QueueMsg, WorkerMsg, WorkerReply>,
)]
impl Queue {
    fn handle_event(
        &mut self,
        event: QueueEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            QueueEvent::Bootstrap => self.spawn_all_workers(),
            QueueEvent::WorkerStarted { slot, result } => self.on_worker_started(slot, result),
            QueueEvent::WorkerCallReturned {
                slot,
                id,
                ticket,
                outcome,
            } => self.on_worker_call_returned(slot, id, ticket, outcome),
            QueueEvent::WorkerCancelReturned {
                slot,
                id,
                attempt,
                outcome,
            } => self.on_worker_cancel_returned(slot, id, attempt, outcome),
            QueueEvent::RetryWorkerCancel {
                slot,
                id,
                attempt,
                result,
            } => self.retry_worker_cancel(slot, id, attempt, result),
            QueueEvent::ParkedCallerCancelled { id, req, outcome } => {
                self.on_parked_caller_cancelled(id, req, outcome)
            }
        }
    }

    fn handle_request(
        &mut self,
        request: QueueRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            QueueRequest::Submit(payload) => self.submit(payload, call),
            QueueRequest::Cancel(id) => self.cancel(id, call),
            QueueRequest::Stats => call.reply(QueueReply::Stats(self.snapshot())),
            QueueRequest::AwaitReady => self.await_ready(call),
            QueueRequest::AwaitQuiescent => self.await_quiescent(call),
        }
    }
}

impl Queue {
    fn new(config: RunConfig) -> Self {
        Self {
            config,
            pending: PendingCancelableCallSet::with_capacity(config.workers),
            workers: vec![None; config.workers],
            worker_busy: vec![None; config.workers],
            next_id: 1,
            stats: QueueStats {
                workers: config.workers,
                workers_alive: 0,
                in_flight: 0,
                pending_callers: 0,
                jobs_admitted: 0,
                jobs_busy_rejected: 0,
                jobs_completed: 0,
                jobs_cancelled: 0,
                jobs_failed: 0,
                worker_crashes: 0,
                worker_respawns: 0,
                cancel_reconciliation_failures: 0,
            },
            ready_waiter: None,
            quiescent_waiter: None,
            startup_error: None,
        }
    }

    fn spawn_all_workers(&mut self) -> Effect<Self> {
        let mut effects = Vec::with_capacity(self.workers.len());
        for slot in 0..self.workers.len() {
            effects.push(self.spawn_worker(slot));
        }
        batch(effects)
    }

    fn spawn_worker(&self, slot: usize) -> Effect<Self> {
        let cap = self.config.worker_mailbox;
        spawn_observed(ChildDefinition::new(Worker { current: None }, cap))
            .then_service_event(move |result| QueueEvent::WorkerStarted { slot, result })
    }

    fn on_worker_started(
        &mut self,
        slot: usize,
        result: tina::SpawnObservedResult<WorkerMsg, WorkerReply>,
    ) -> Effect<Self> {
        match result {
            Ok(child) => {
                self.workers[slot] = Some(tina_runtime::SplitServiceHandle::from_address(
                    child.address,
                ));
                self.stats.workers_alive = self.workers.iter().filter(|w| w.is_some()).count();
                self.settle_ready_waiter()
            }
            Err(error) => {
                self.workers[slot] = None;
                self.stats.workers_alive = self.workers.iter().filter(|w| w.is_some()).count();
                self.startup_error = Some(error);
                self.settle_ready_waiter()
            }
        }
    }

    fn all_workers_ready(&self) -> bool {
        self.workers.iter().all(|worker| worker.is_some())
    }

    fn is_quiescent(&self) -> bool {
        self.in_flight_count() == 0 && self.pending.is_empty()
    }

    fn settle_ready_waiter(&mut self) -> Effect<Self> {
        self.discard_closed_ready_waiter();
        if let Some(error) = self.startup_error {
            if let Some(req) = self.ready_waiter.take() {
                return reply_to::<Self>(req, QueueReply::StartupFailed(error));
            }
            return noop();
        }
        if self.all_workers_ready() {
            if let Some(req) = self.ready_waiter.take() {
                return reply_to::<Self>(req, QueueReply::Ready);
            }
        }
        noop()
    }

    fn settle_quiescent_waiter(&mut self) -> Effect<Self> {
        self.discard_closed_quiescent_waiter();
        if self.is_quiescent() {
            if let Some(req) = self.quiescent_waiter.take() {
                return reply_to::<Self>(req, QueueReply::Quiescent);
            }
        }
        noop()
    }

    fn await_ready(&mut self, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        self.discard_closed_ready_waiter();
        if let Some(error) = self.startup_error {
            return call.reply(QueueReply::StartupFailed(error));
        }
        if self.all_workers_ready() {
            return call.reply(QueueReply::Ready);
        }
        if self.ready_waiter.is_some() {
            return call.reply(QueueReply::Busy);
        }
        call.capture(|req| {
            self.ready_waiter = Some(req);
            noop()
        })
    }

    fn await_quiescent(&mut self, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        self.discard_closed_quiescent_waiter();
        if self.is_quiescent() {
            return call.reply(QueueReply::Quiescent);
        }
        if self.quiescent_waiter.is_some() {
            return call.reply(QueueReply::Busy);
        }
        call.capture(|req| {
            self.quiescent_waiter = Some(req);
            noop()
        })
    }

    fn discard_closed_ready_waiter(&mut self) {
        Self::discard_closed_waiter(&mut self.ready_waiter);
    }

    fn discard_closed_quiescent_waiter(&mut self) {
        Self::discard_closed_waiter(&mut self.quiescent_waiter);
    }

    fn discard_closed_waiter(waiter: &mut Option<tina::RequestContext<QueueReply>>) {
        if waiter.as_ref().is_some_and(|waiter| !waiter.is_open()) {
            *waiter = None;
        }
    }

    fn submit(&mut self, payload: Payload, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        let Some(slot) = self.idle_slot() else {
            self.stats.jobs_busy_rejected += 1;
            return call.reply(QueueReply::Busy);
        };
        let Some(worker) = self.workers[slot] else {
            self.stats.jobs_busy_rejected += 1;
            return call.reply(QueueReply::Busy);
        };
        let id = JobId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("monotonic job id space exhausted");
        let sleep_ms = self.config.job_sleep_ms;
        let dispatch_timeout = self.config.dispatch_timeout();

        let admission = call
            .defer_cancelable(call_cancelable_request(
                worker.requests,
                WorkerRequest::Process {
                    id,
                    payload,
                    sleep_ms,
                },
                dispatch_timeout,
            ))
            .try_admit_service_event(&mut self.pending, id, move |key, ticket, outcome| {
                QueueEvent::WorkerCallReturned {
                    slot,
                    id: key,
                    ticket,
                    outcome,
                }
            });
        match admission {
            Ok(effect) => {
                self.worker_busy[slot] = Some(id);
                self.stats.jobs_admitted += 1;
                self.stats.in_flight = self.in_flight_count();
                effect
            }
            // pending cap = workers, so a full pending set while an idle
            // worker slot exists should never happen in normal accounting.
            // If it ever does, answer the caller typed instead of stranding
            // the already-captured request authority behind a panic.
            Err(error @ RequestPendingCancelableInsertError::Full { .. }) => {
                self.stats.jobs_busy_rejected += 1;
                error.reply(QueueReply::Busy)
            }
            // Monotonic `next_id` never repeats, so a duplicate key is a
            // real accounting bug worth failing loudly on.
            Err(RequestPendingCancelableInsertError::DuplicateKey { .. }) => {
                panic!("duplicate job id {id:?} — queue accounting bug")
            }
        }
    }

    fn cancel(&mut self, id: JobId, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        let Some(ticket) = self.pending.ticket(&id) else {
            return call.reply(QueueReply::NotFound);
        };
        let token = match self.pending.remove(&id, ticket) {
            Ok(token) => token,
            Err(PendingCancelableRemoveError::MissingKey)
            | Err(PendingCancelableRemoveError::StaleTicket) => {
                return call.reply(QueueReply::NotFound);
            }
        };

        // Keep the worker slot charged until the worker acknowledges that it
        // released the cancelled process's deferred reply authority.
        let mut worker = None;
        for (slot, busy) in self.worker_busy.iter_mut().enumerate() {
            if *busy == Some(id) {
                worker = self.workers[slot].map(|worker| (slot, worker));
                break;
            }
        }

        // `reply_and` guarantees that the cancel caller is settled before
        // these follow-up effects run. The cancel continuation still owns and
        // explicitly settles the parked submit caller.
        let mut follow_up: Vec<Effect<Self>> = Vec::with_capacity(2);
        follow_up.push(token.cancel_service_event(|key, req, outcome| {
            QueueEvent::ParkedCallerCancelled {
                id: key,
                req,
                outcome,
            }
        }));
        if let Some((slot, worker)) = worker {
            follow_up.push(self.cancel_worker(slot, id, 0, worker));
        } else {
            self.stats.cancel_reconciliation_failures += 1;
            follow_up.push(stop());
        }
        call.reply_and(QueueReply::Cancelled(id), follow_up)
    }

    fn on_parked_caller_cancelled(
        &mut self,
        id: JobId,
        req: tina::RequestContext<QueueReply>,
        outcome: tina::CancelOutcome,
    ) -> Effect<Self> {
        match outcome {
            tina::CancelOutcome::Cancelled | tina::CancelOutcome::AlreadyCancelled => {
                self.stats.jobs_cancelled += 1;
                let reply = reply_to::<Self>(req, QueueReply::Done(JobOutcome::Cancelled { id }));
                match self.settle_quiescent_waiter() {
                    Effect::Noop => reply,
                    other => batch(vec![reply, other]),
                }
            }
            tina::CancelOutcome::AlreadyCompleted => {
                self.stats.jobs_failed += 1;
                let reply = reply_to::<Self>(
                    req,
                    QueueReply::Done(JobOutcome::Failed {
                        id,
                        reason: "cancel raced with an already completed worker call".into(),
                    }),
                );
                match self.settle_quiescent_waiter() {
                    Effect::Noop => reply,
                    other => batch(vec![reply, other]),
                }
            }
            tina::CancelOutcome::NotAdmitted | tina::CancelOutcome::WrongShard => {
                self.stats.jobs_failed += 1;
                self.stats.cancel_reconciliation_failures += 1;
                batch(vec![
                    reply_to::<Self>(
                        req,
                        QueueReply::Done(JobOutcome::Failed {
                            id,
                            reason: format!("worker-call cancellation returned {outcome:?}"),
                        }),
                    ),
                    stop(),
                ])
            }
        }
    }

    fn cancel_worker(
        &self,
        slot: usize,
        id: JobId,
        attempt: u8,
        worker: SplitServiceHandle<WorkerEvent, WorkerRequest, WorkerReply>,
    ) -> Effect<Self> {
        call_request(
            worker.requests,
            WorkerRequest::Cancel(id),
            self.config.dispatch_timeout(),
        )
        .then_service_event(move |outcome| QueueEvent::WorkerCancelReturned {
            slot,
            id,
            attempt,
            outcome,
        })
    }

    fn on_worker_cancel_returned(
        &mut self,
        slot: usize,
        id: JobId,
        attempt: u8,
        outcome: CallOutcome<WorkerReply>,
    ) -> Effect<Self> {
        if self.worker_busy[slot] != Some(id) {
            return noop();
        }

        match classify_worker_cancel_outcome(id, &outcome) {
            WorkerCancelDisposition::Released => {
                self.worker_busy[slot] = None;
                self.after_slot_or_pending_change()
            }
            WorkerCancelDisposition::Retry if attempt < MAX_WORKER_CANCEL_RETRIES => {
                let next_attempt = attempt + 1;
                sleep(WORKER_CANCEL_RETRY_DELAY).then_service_event(move |result| {
                    QueueEvent::RetryWorkerCancel {
                        slot,
                        id,
                        attempt: next_attempt,
                        result,
                    }
                })
            }
            WorkerCancelDisposition::Retry => self.fail_cancel_reconciliation(),
            WorkerCancelDisposition::Closed => {
                self.worker_busy[slot] = None;
                if self.workers[slot].take().is_some() {
                    self.stats.workers_alive = self.workers.iter().filter(|w| w.is_some()).count();
                }
                self.stats.in_flight = self.in_flight_count();
                self.stats.worker_crashes += 1;
                self.stats.worker_respawns += 1;
                let spawn = self.spawn_worker(slot);
                match self.settle_quiescent_waiter() {
                    Effect::Noop => spawn,
                    other => batch(vec![other, spawn]),
                }
            }
            WorkerCancelDisposition::Fatal => self.fail_cancel_reconciliation(),
        }
    }

    fn retry_worker_cancel(
        &mut self,
        slot: usize,
        id: JobId,
        attempt: u8,
        result: SleepReply,
    ) -> Effect<Self> {
        if result.is_err() {
            return self.fail_cancel_reconciliation();
        }
        if self.worker_busy[slot] != Some(id) {
            return noop();
        }
        let Some(worker) = self.workers[slot] else {
            return self.fail_cancel_reconciliation();
        };
        self.cancel_worker(slot, id, attempt, worker)
    }

    fn fail_cancel_reconciliation(&mut self) -> Effect<Self> {
        self.stats.cancel_reconciliation_failures += 1;
        // Stopping the owner also stops its children, so no uncertain worker
        // can be reused and no replacement can make the topology unbounded.
        stop()
    }

    fn on_worker_call_returned(
        &mut self,
        slot: usize,
        id: JobId,
        ticket: PendingCancelableTicket,
        outcome: CallOutcome<WorkerReply>,
    ) -> Effect<Self> {
        let pending = match self.pending.remove(&id, ticket) {
            Ok(token) => token,
            Err(PendingCancelableRemoveError::MissingKey)
            | Err(PendingCancelableRemoveError::StaleTicket) => {
                // Cancel removed the entry first. With `cancel_call` that
                // closed the queue's wait, a worker reply for this id should
                // not reach us at all — so this branch is mostly defensive.
                return noop();
            }
        };

        let disposition = classify_worker_call_outcome(&outcome);
        let req = pending.into_request_context();
        let final_outcome = match outcome {
            CallOutcome::Replied(WorkerReply::Completed(value)) => {
                self.release_worker_slot(slot, id);
                self.stats.jobs_completed += 1;
                JobOutcome::Completed { id, value }
            }
            CallOutcome::Replied(WorkerReply::Cancelled) => {
                self.release_worker_slot(slot, id);
                self.stats.jobs_cancelled += 1;
                JobOutcome::Cancelled { id }
            }
            CallOutcome::Replied(WorkerReply::Failed(reason)) => {
                self.release_worker_slot(slot, id);
                self.stats.jobs_failed += 1;
                JobOutcome::Failed { id, reason }
            }
            CallOutcome::Full => {
                self.release_worker_slot(slot, id);
                self.stats.jobs_failed += 1;
                JobOutcome::Failed {
                    id,
                    reason: "worker process call was not admitted: Full".into(),
                }
            }
            CallOutcome::Closed => {
                self.stats.worker_crashes += 1;
                self.stats.jobs_failed += 1;
                JobOutcome::Failed {
                    id,
                    reason: "worker process call returned Closed".into(),
                }
            }
            CallOutcome::Timeout => {
                self.stats.jobs_failed += 1;
                JobOutcome::Failed {
                    id,
                    reason: "worker process call timed out; cancellation reconciliation started"
                        .into(),
                }
            }
            CallOutcome::Rejected(reason) => {
                self.stats.jobs_failed += 1;
                JobOutcome::Failed {
                    id,
                    reason: format!("worker process call was rejected: {reason:?}"),
                }
            }
            CallOutcome::Replied(WorkerReply::CancelAcknowledged { .. }) => {
                self.stats.jobs_failed += 1;
                JobOutcome::Failed {
                    id,
                    reason: "process call returned a cancel acknowledgement".into(),
                }
            }
        };

        let follow_up = match disposition {
            WorkerCallDisposition::Released => Some(self.settle_quiescent_waiter()),
            WorkerCallDisposition::Reconcile => match self.workers[slot] {
                Some(worker) => Some(self.cancel_worker(slot, id, 0, worker)),
                None => Some(stop()),
            },
            WorkerCallDisposition::Replace => {
                self.worker_busy[slot] = None;
                if self.workers[slot].take().is_some() {
                    self.stats.workers_alive = self.workers.iter().filter(|w| w.is_some()).count();
                }
                self.stats.in_flight = self.in_flight_count();
                self.stats.worker_respawns += 1;
                let spawn = self.spawn_worker(slot);
                match self.settle_quiescent_waiter() {
                    Effect::Noop => Some(spawn),
                    other => Some(batch(vec![other, spawn])),
                }
            }
            WorkerCallDisposition::Fatal => {
                self.stats.cancel_reconciliation_failures += 1;
                Some(stop())
            }
        };

        let reply = reply_to::<Self>(req, QueueReply::Done(final_outcome));
        match follow_up {
            Some(Effect::Noop) => reply,
            Some(effect) => batch(vec![reply, effect]),
            None => reply,
        }
    }

    fn release_worker_slot(&mut self, slot: usize, id: JobId) {
        if self.worker_busy[slot] == Some(id) {
            self.worker_busy[slot] = None;
            self.stats.in_flight = self.in_flight_count();
        }
    }

    fn after_slot_or_pending_change(&mut self) -> Effect<Self> {
        self.stats.in_flight = self.in_flight_count();
        self.settle_quiescent_waiter()
    }

    fn idle_slot(&self) -> Option<usize> {
        self.workers
            .iter()
            .enumerate()
            .find(|(slot, addr)| addr.is_some() && self.worker_busy[*slot].is_none())
            .map(|(slot, _)| slot)
    }

    fn in_flight_count(&self) -> usize {
        self.worker_busy.iter().filter(|s| s.is_some()).count()
    }

    fn snapshot(&self) -> QueueStats {
        let mut s = self.stats.clone();
        s.in_flight = self.in_flight_count();
        s.pending_callers = self.pending.len();
        s.workers_alive = self.workers.iter().filter(|w| w.is_some()).count();
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerCancelDisposition {
    Released,
    Retry,
    Closed,
    Fatal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerCallDisposition {
    Released,
    Reconcile,
    Replace,
    Fatal,
}

fn classify_worker_call_outcome(outcome: &CallOutcome<WorkerReply>) -> WorkerCallDisposition {
    match outcome {
        CallOutcome::Replied(WorkerReply::Completed(_))
        | CallOutcome::Replied(WorkerReply::Cancelled)
        | CallOutcome::Replied(WorkerReply::Failed(_))
        | CallOutcome::Full => WorkerCallDisposition::Released,
        CallOutcome::Timeout => WorkerCallDisposition::Reconcile,
        CallOutcome::Closed => WorkerCallDisposition::Replace,
        CallOutcome::Rejected(_) | CallOutcome::Replied(WorkerReply::CancelAcknowledged { .. }) => {
            WorkerCallDisposition::Fatal
        }
    }
}

fn classify_worker_cancel_outcome(
    id: JobId,
    outcome: &CallOutcome<WorkerReply>,
) -> WorkerCancelDisposition {
    match outcome {
        CallOutcome::Replied(WorkerReply::CancelAcknowledged {
            id: acknowledged, ..
        }) if *acknowledged == id => WorkerCancelDisposition::Released,
        CallOutcome::Full | CallOutcome::Timeout => WorkerCancelDisposition::Retry,
        CallOutcome::Closed => WorkerCancelDisposition::Closed,
        CallOutcome::Rejected(_)
        | CallOutcome::Replied(WorkerReply::Completed(_))
        | CallOutcome::Replied(WorkerReply::Cancelled)
        | CallOutcome::Replied(WorkerReply::Failed(_))
        | CallOutcome::Replied(WorkerReply::CancelAcknowledged { .. }) => {
            WorkerCancelDisposition::Fatal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_context(open: bool, slot_id: u64) -> tina::RequestContext<QueueReply> {
        use std::any::TypeId;
        use std::sync::Arc;

        let shared = Arc::new(tina::DeferredSlotShared::new(
            slot_id,
            TypeId::of::<QueueReply>(),
        ));
        if !open {
            shared.set_state(tina::DeferredSlotState::Closed);
        }
        let deferred = tina::runtime_internal::deferred_from_handle(
            tina::runtime_internal::handle_from_shared(shared),
        );
        tina::runtime_internal::request_context_from_deferred(deferred)
    }

    #[test]
    fn ready_and_quiescent_waiter_slots_reclaim_only_closed_callers() {
        let mut queue = cancel_queue(JobId(1));
        queue.ready_waiter = Some(request_context(false, 1));
        queue.quiescent_waiter = Some(request_context(false, 2));
        queue.discard_closed_ready_waiter();
        queue.discard_closed_quiescent_waiter();
        assert!(queue.ready_waiter.is_none());
        assert!(queue.quiescent_waiter.is_none());

        queue.ready_waiter = Some(request_context(true, 3));
        queue.quiescent_waiter = Some(request_context(true, 4));
        queue.discard_closed_ready_waiter();
        queue.discard_closed_quiescent_waiter();
        assert!(queue.ready_waiter.as_ref().is_some_and(|waiter| waiter.is_open()));
        assert!(
            queue
                .quiescent_waiter
                .as_ref()
                .is_some_and(|waiter| waiter.is_open())
        );
    }

    #[test]
    fn terminal_settlement_never_replies_through_abandoned_waiter_slots() {
        let mut queue = cancel_queue(JobId(1));
        queue.ready_waiter = Some(request_context(false, 1));
        queue.startup_error = Some(SpawnObservedError::ParentMailboxClosed);
        assert!(matches!(queue.settle_ready_waiter(), Effect::Noop));
        assert!(queue.ready_waiter.is_none());

        queue.quiescent_waiter = Some(request_context(false, 2));
        queue.worker_busy[0] = None;
        queue.stats.in_flight = 0;
        assert!(matches!(queue.settle_quiescent_waiter(), Effect::Noop));
        assert!(queue.quiescent_waiter.is_none());
    }

    #[test]
    fn worker_cancel_classification_is_exhaustive_and_id_sensitive() {
        let id = JobId(7);
        for released in [false, true] {
            assert_eq!(
                classify_worker_cancel_outcome(
                    id,
                    &CallOutcome::Replied(WorkerReply::CancelAcknowledged { id, released }),
                ),
                WorkerCancelDisposition::Released,
            );
        }
        for outcome in [CallOutcome::Full, CallOutcome::Timeout] {
            assert_eq!(
                classify_worker_cancel_outcome(id, &outcome),
                WorkerCancelDisposition::Retry,
            );
        }
        assert_eq!(
            classify_worker_cancel_outcome(id, &CallOutcome::Closed),
            WorkerCancelDisposition::Closed,
        );
        for outcome in [
            CallOutcome::Rejected(tina::CallRejectedReason::UnsupportedMessage),
            CallOutcome::Replied(WorkerReply::Completed(14)),
            CallOutcome::Replied(WorkerReply::Cancelled),
            CallOutcome::Replied(WorkerReply::Failed("sleep".into())),
            CallOutcome::Replied(WorkerReply::CancelAcknowledged {
                id: JobId(8),
                released: true,
            }),
        ] {
            assert_eq!(
                classify_worker_cancel_outcome(id, &outcome),
                WorkerCancelDisposition::Fatal,
            );
        }
    }

    #[test]
    fn worker_process_classification_never_reuses_an_uncertain_worker() {
        for outcome in [
            CallOutcome::Replied(WorkerReply::Completed(14)),
            CallOutcome::Replied(WorkerReply::Cancelled),
            CallOutcome::Replied(WorkerReply::Failed("sleep".into())),
            CallOutcome::Full,
        ] {
            assert_eq!(
                classify_worker_call_outcome(&outcome),
                WorkerCallDisposition::Released,
            );
        }
        assert_eq!(
            classify_worker_call_outcome(&CallOutcome::Timeout),
            WorkerCallDisposition::Reconcile,
        );
        assert_eq!(
            classify_worker_call_outcome(&CallOutcome::Closed),
            WorkerCallDisposition::Replace,
        );
        for outcome in [
            CallOutcome::Rejected(tina::CallRejectedReason::UnsupportedMessage),
            CallOutcome::Replied(WorkerReply::CancelAcknowledged {
                id: JobId(7),
                released: true,
            }),
        ] {
            assert_eq!(
                classify_worker_call_outcome(&outcome),
                WorkerCallDisposition::Fatal,
            );
        }
    }

    fn cancel_queue(id: JobId) -> Queue {
        let config = RunConfig {
            workers: 1,
            queue_mailbox: 4,
            worker_mailbox: 4,
            job_sleep_ms: 10,
            call_timeout_ms: 100,
        };
        let mut queue = Queue::new(config);
        queue.worker_busy[0] = Some(id);
        queue.stats.in_flight = 1;
        queue
    }

    #[test]
    fn worker_cancel_state_machine_retries_replaces_and_fails_closed() {
        let id = JobId(7);

        let mut retry = cancel_queue(id);
        assert!(matches!(
            retry.on_worker_cancel_returned(0, id, 0, CallOutcome::Full),
            Effect::Io(_)
        ));
        assert_eq!(retry.worker_busy[0], Some(id));
        assert_eq!(retry.stats.cancel_reconciliation_failures, 0);

        let mut exhausted = cancel_queue(id);
        assert!(matches!(
            exhausted.on_worker_cancel_returned(
                0,
                id,
                MAX_WORKER_CANCEL_RETRIES,
                CallOutcome::Timeout,
            ),
            Effect::Stop
        ));
        assert_eq!(exhausted.worker_busy[0], Some(id));
        assert_eq!(exhausted.stats.cancel_reconciliation_failures, 1);

        let mut closed = cancel_queue(id);
        assert!(matches!(
            closed.on_worker_cancel_returned(0, id, 0, CallOutcome::Closed),
            Effect::SpawnObserved(_)
        ));
        assert_eq!(closed.worker_busy[0], None);
        assert_eq!(closed.stats.in_flight, 0);
        assert_eq!(closed.stats.worker_crashes, 1);
        assert_eq!(closed.stats.worker_respawns, 1);

        let mut rejected = cancel_queue(id);
        assert!(matches!(
            rejected.on_worker_cancel_returned(
                0,
                id,
                0,
                CallOutcome::Rejected(tina::CallRejectedReason::UnsupportedMessage),
            ),
            Effect::Stop
        ));
        assert_eq!(rejected.stats.cancel_reconciliation_failures, 1);
    }

    #[test]
    fn worker_cancel_state_machine_releases_only_the_matching_job() {
        let id = JobId(7);
        let mut queue = cancel_queue(id);
        assert!(matches!(
            queue.on_worker_cancel_returned(
                0,
                JobId(8),
                0,
                CallOutcome::Replied(WorkerReply::CancelAcknowledged {
                    id: JobId(8),
                    released: true,
                }),
            ),
            Effect::Noop
        ));
        assert_eq!(queue.worker_busy[0], Some(id));

        assert!(matches!(
            queue.on_worker_cancel_returned(
                0,
                id,
                0,
                CallOutcome::Replied(WorkerReply::CancelAcknowledged {
                    id,
                    released: false,
                }),
            ),
            Effect::Noop
        ));
        assert_eq!(queue.worker_busy[0], None);
        assert_eq!(queue.stats.in_flight, 0);
    }
}

// ---------- Host-visible entry points ----------

/// Aggregate report for a smoke run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub overflow: OverflowReport,
    pub cancel_in_flight: CancelInFlightReport,
    pub caller_gone: CallerGoneReport,
    pub poison_crash: PoisonCrashReport,
    pub respawn_then_admit: RespawnThenAdmitReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverflowReport {
    pub completed: usize,
    pub busy: usize,
    pub full: usize,
    pub closed: usize,
    pub timeout: usize,
    pub rejected: usize,
    pub rejection_reasons: Vec<tina::CallRejectedReason>,
    pub stats: QueueStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelInFlightReport {
    pub submit_outcome: JobOutcome,
    pub cancel_reply: QueueReply,
    pub refill_outcome: JobOutcome,
    pub stats: QueueStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerGoneReport {
    pub submit_outcome: CallOutcome<QueueReply>,
    pub stats: QueueStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaiterReplacementReport {
    pub abandoned_waiter: CallOutcome<QueueReply>,
    pub replacement_waiter: CallOutcome<QueueReply>,
    pub submit_outcome: JobOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoisonCrashReport {
    pub failed_outcome: JobOutcome,
    pub stats: QueueStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RespawnThenAdmitReport {
    pub poison_outcome: JobOutcome,
    pub follow_up_outcome: JobOutcome,
    pub stats: QueueStats,
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let config = config.validate()?;
    Ok(RunReport {
        overflow: run_overflow(config)?,
        cancel_in_flight: run_cancel_in_flight(config)?,
        caller_gone: run_caller_gone(config)?,
        poison_crash: run_poison_crash(config)?,
        respawn_then_admit: run_respawn_then_admit(config)?,
    })
}

pub fn run_overflow(config: RunConfig) -> anyhow::Result<OverflowReport> {
    let config = config.validate()?;
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    app.run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |app| -> anyhow::Result<OverflowReport> {
        let queue = register_queue(app, config)?;
        let (burst, barrier_participants) = config.overflow_shape()?;
        let timeout = Duration::from_millis(config.call_timeout_ms);
        let barrier = Barrier::new(barrier_participants);
        let outcomes = thread::scope(|scope| {
            let mut threads = Vec::with_capacity(burst);
            for _ in 0..burst {
                threads.push(scope.spawn(|| {
                    barrier.wait();
                    app.call_blocking_request(
                        queue.requests,
                        QueueRequest::Submit(Payload::Work(7)),
                        timeout,
                    )
                }));
            }
            barrier.wait();
            threads
                .into_iter()
                .map(|thread| {
                    thread
                        .join()
                        .map_err(|_| anyhow::anyhow!("submit thread panicked"))
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })?;

        let mut completed = 0;
        let mut busy = 0;
        let mut full = 0;
        let mut closed = 0;
        let mut timeout_count = 0;
        let mut rejected = 0;
        let mut rejection_reasons = Vec::new();
        for outcome in outcomes {
            match outcome? {
                CallOutcome::Replied(QueueReply::Done(JobOutcome::Completed { .. })) => {
                    completed += 1;
                }
                CallOutcome::Replied(QueueReply::Busy) => busy += 1,
                CallOutcome::Full => full += 1,
                CallOutcome::Closed => closed += 1,
                CallOutcome::Timeout => timeout_count += 1,
                CallOutcome::Rejected(reason) => {
                    rejected += 1;
                    rejection_reasons.push(reason);
                }
                CallOutcome::Replied(other) => {
                    anyhow::bail!("unexpected submit reply: {other:?}");
                }
            }
        }

        Ok(OverflowReport {
            completed,
            busy,
            full,
            closed,
            timeout: timeout_count,
            rejected,
            rejection_reasons,
            stats: stats(app, queue.requests)?,
        })
    })
    .map_err(anyhow::Error::from)
}

pub fn run_cancel_in_flight(config: RunConfig) -> anyhow::Result<CancelInFlightReport> {
    // Long-running job so cancel reliably lands first.
    let mut config = config.validate()?;
    config.job_sleep_ms = config.job_sleep_ms.max(150);
    let config = config.validate()?;
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    app.run_to_shutdown_reported(
        SHUTDOWN_TIMEOUT,
        |app| -> anyhow::Result<CancelInFlightReport> {
            let queue = register_queue(app, config)?;
            let timeout = Duration::from_millis(config.call_timeout_ms);
            let (submit_outcome, cancel_reply) = thread::scope(|scope| -> anyhow::Result<_> {
                let submit = scope.spawn(|| {
                    app.call_blocking_request(
                        queue.requests,
                        QueueRequest::Submit(Payload::Work(99)),
                        timeout,
                    )
                });

                thread::sleep(Duration::from_millis(config.job_sleep_ms / 4 + 5));
                let cancel_reply = match app.call_blocking_request(
                    queue.requests,
                    QueueRequest::Cancel(JobId(1)),
                    timeout,
                )? {
                    CallOutcome::Replied(reply) => reply,
                    other => anyhow::bail!("unexpected cancel outcome: {other:?}"),
                };
                let submit_outcome = match submit
                    .join()
                    .map_err(|_| anyhow::anyhow!("submit thread panicked"))??
                {
                    CallOutcome::Replied(QueueReply::Done(outcome)) => outcome,
                    other => anyhow::bail!("unexpected submit outcome: {other:?}"),
                };
                Ok((submit_outcome, cancel_reply))
            })?;

            await_quiescent(app, queue.requests, timeout)?;
            let refill_outcome = match app.call_blocking_request(
                queue.requests,
                QueueRequest::Submit(Payload::Work(21)),
                timeout,
            )? {
                CallOutcome::Replied(QueueReply::Done(outcome)) => outcome,
                other => anyhow::bail!("unexpected refill outcome: {other:?}"),
            };

            await_quiescent(app, queue.requests, timeout)?;
            Ok(CancelInFlightReport {
                submit_outcome,
                cancel_reply,
                refill_outcome,
                stats: stats(app, queue.requests)?,
            })
        },
    )
    .map_err(anyhow::Error::from)
}

pub fn run_caller_gone(config: RunConfig) -> anyhow::Result<CallerGoneReport> {
    let mut config = config.validate()?;
    config.job_sleep_ms = config.job_sleep_ms.max(100);
    let config = config.validate()?;
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    app.run_to_shutdown_reported(
        SHUTDOWN_TIMEOUT,
        |app| -> anyhow::Result<CallerGoneReport> {
            let queue = register_queue(app, config)?;
            let submit_outcome = app.call_blocking_request(
                queue.requests,
                QueueRequest::Submit(Payload::Work(11)),
                Duration::from_millis(10),
            )?;
            if !matches!(submit_outcome, CallOutcome::Timeout) {
                anyhow::bail!("caller-gone probe expected Timeout, got {submit_outcome:?}");
            }

            await_quiescent(
                app,
                queue.requests,
                Duration::from_millis(config.call_timeout_ms),
            )?;
            Ok(CallerGoneReport {
                submit_outcome,
                stats: stats(app, queue.requests)?,
            })
        },
    )
    .map_err(anyhow::Error::from)
}

/// Proves that a timed-out parked waiter releases its admission slot.
pub fn run_quiescent_waiter_replacement(
    config: RunConfig,
) -> anyhow::Result<WaiterReplacementReport> {
    let mut config = config.validate()?;
    config.job_sleep_ms = config.job_sleep_ms.max(150);
    config.call_timeout_ms = config
        .call_timeout_ms
        .max(config.job_sleep_ms.saturating_add(1_000));
    let config = config.validate()?;
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    app.run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |app| {
        let queue = register_queue(app, config)?;
        thread::scope(|scope| -> anyhow::Result<WaiterReplacementReport> {
            let submit = scope.spawn(|| {
                app.call_blocking_request(
                    queue.requests,
                    QueueRequest::Submit(Payload::Work(21)),
                    Duration::from_millis(config.call_timeout_ms),
                )
            });

            let observation_deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                if stats(app, queue.requests)?.in_flight == 1 {
                    break;
                }
                if std::time::Instant::now() >= observation_deadline {
                    anyhow::bail!("submitted job was not admitted before proof deadline");
                }
                thread::yield_now();
            }

            let abandoned_waiter = app.call_blocking_request(
                queue.requests,
                QueueRequest::AwaitQuiescent,
                Duration::from_millis(1),
            )?;
            if !matches!(abandoned_waiter, CallOutcome::Timeout) {
                anyhow::bail!("first waiter must time out, got {abandoned_waiter:?}");
            }

            let replacement_waiter = app.call_blocking_request(
                queue.requests,
                QueueRequest::AwaitQuiescent,
                Duration::from_millis(config.call_timeout_ms),
            )?;
            let submit_outcome = match submit
                .join()
                .map_err(|_| anyhow::anyhow!("submit thread panicked"))??
            {
                CallOutcome::Replied(QueueReply::Done(outcome)) => outcome,
                other => anyhow::bail!("unexpected submit outcome: {other:?}"),
            };

            Ok(WaiterReplacementReport {
                abandoned_waiter,
                replacement_waiter,
                submit_outcome,
            })
        })
    })
    .map_err(anyhow::Error::from)
}

pub fn run_poison_crash(config: RunConfig) -> anyhow::Result<PoisonCrashReport> {
    let config = config.validate()?;
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    app.run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |app| {
        let queue = register_queue(app, config)?;
        let timeout = Duration::from_millis(config.call_timeout_ms);
        let failed_outcome = match app.call_blocking_request(
            queue.requests,
            QueueRequest::Submit(Payload::Poison),
            timeout,
        )? {
            CallOutcome::Replied(QueueReply::Done(outcome)) => outcome,
            other => anyhow::bail!("unexpected poison outcome: {other:?}"),
        };

        await_ready(app, queue.requests, timeout)?;

        Ok(PoisonCrashReport {
            failed_outcome,
            stats: stats(app, queue.requests)?,
        })
    })
    .map_err(anyhow::Error::from)
}

pub fn run_respawn_then_admit(config: RunConfig) -> anyhow::Result<RespawnThenAdmitReport> {
    let config = config.validate()?;
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    app.run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |app| {
        let queue = register_queue(app, config)?;
        let timeout = Duration::from_millis(config.call_timeout_ms);
        let poison_outcome = match app.call_blocking_request(
            queue.requests,
            QueueRequest::Submit(Payload::Poison),
            timeout,
        )? {
            CallOutcome::Replied(QueueReply::Done(outcome)) => outcome,
            other => anyhow::bail!("unexpected poison outcome: {other:?}"),
        };

        await_ready(app, queue.requests, timeout)?;

        let follow_up_outcome = match app.call_blocking_request(
            queue.requests,
            QueueRequest::Submit(Payload::Work(21)),
            timeout,
        )? {
            CallOutcome::Replied(QueueReply::Done(outcome)) => outcome,
            other => anyhow::bail!("unexpected follow-up outcome: {other:?}"),
        };

        Ok(RespawnThenAdmitReport {
            poison_outcome,
            follow_up_outcome,
            stats: stats(app, queue.requests)?,
        })
    })
    .map_err(anyhow::Error::from)
}

// ---------- Helpers ----------

fn register_queue(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    config: RunConfig,
) -> anyhow::Result<SplitServiceHandle<QueueEvent, QueueRequest, QueueReply>> {
    let service = app
        .register_split_service_with_bootstrap::<
            Queue,
            QueueEvent,
            QueueRequest,
            std::convert::Infallible,
        >(
            Queue::new(config),
            config.queue_mailbox,
            QueueEvent::Bootstrap,
        )
        .map_err(|e| anyhow::anyhow!("register queue: {e:?}"))?;

    await_ready(
        app,
        service.requests,
        Duration::from_millis(config.call_timeout_ms),
    )?;
    Ok(service)
}

fn await_ready(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    queue: tina::ServiceRequestAddress<QueueEvent, QueueRequest, QueueReply>,
    timeout: Duration,
) -> anyhow::Result<()> {
    match app.call_blocking_request(queue, QueueRequest::AwaitReady, timeout)? {
        CallOutcome::Replied(QueueReply::Ready) => Ok(()),
        CallOutcome::Replied(QueueReply::StartupFailed(error)) => {
            anyhow::bail!("queue startup failed: {error:?}")
        }
        other => anyhow::bail!("await ready failed: {other:?}"),
    }
}

fn await_quiescent(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    queue: tina::ServiceRequestAddress<QueueEvent, QueueRequest, QueueReply>,
    timeout: Duration,
) -> anyhow::Result<()> {
    match app.call_blocking_request(queue, QueueRequest::AwaitQuiescent, timeout)? {
        CallOutcome::Replied(QueueReply::Quiescent) => Ok(()),
        other => anyhow::bail!("await quiescent failed: {other:?}"),
    }
}

fn stats(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    queue: tina::ServiceRequestAddress<QueueEvent, QueueRequest, QueueReply>,
) -> anyhow::Result<QueueStats> {
    match app.call_blocking_request(queue, QueueRequest::Stats, Duration::from_secs(2))? {
        CallOutcome::Replied(QueueReply::Stats(s)) => Ok(s),
        other => anyhow::bail!("stats call failed: {other:?}"),
    }
}
