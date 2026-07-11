//! `system_job_queue` v2 — bounded worker pool with synchronous `Submit`,
//! cancel-while-running, and one-shot worker respawn on crash.
//!
//! v2 collapses two parallel pending structures (`PendingReplies` for parked
//! callers + `PendingCallSet` for in-flight call handles) into one
//! [`PendingCancelableCallSet`], using `CallContext::defer_cancelable(...)
//! .try_admit(...)` as the admission gate. Total admission cap is `workers`;
//! there is no separate queue layer. A queued layer would just delay the
//! `Busy` reply and was buying nothing real.
//!
//! What this specimen pulls on:
//!
//! - [`PendingCancelableCallSet`] for caller authority + cancel handle as
//!   one token.
//! - `CallContext::defer_cancelable(call_cancelable(...)).try_admit(...)`
//!   for "admit first, then dispatch" with a typed `Full` recovery path.
//! - `PendingCancelableCall::cancel(translator)` for cancel-while-running:
//!   one effect closes the wait AND routes the parked request context into
//!   the cancel continuation.
//! - `spawn_observed(ChildDefinition::new(...))` for typed child refs.
//! - Runtime-owned `sleep` as the worker's only async surface.
//! - The `Worker` isolate uses the `event = .. request = ..` split-service
//!   macro form: `WorkerEvent` (`Cancel`, `Wake`) is fire-and-forget,
//!   `WorkerRequest` (`Process`) is the one caller-authority message. The
//!   macro generates both rejection arms, so neither `handle_event` nor
//!   `handle_request` writes one by hand.
//! - `register_with_capacity_and_bootstrap` registers the `Queue` isolate
//!   with its startup `Bootstrap` message prefilled atomically, instead of
//!   a separate post-register `try_send`.

use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::{ChildDefinition, prelude::*};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, PendingCancelableCallSet,
    PendingCancelableRemoveError, PendingCancelableTicket, RequestPendingCancelableInsertError,
    SleepReply, ThreadedRuntime, call_cancelable_request, sleep,
};

/// Tunables for one specimen run.
#[derive(Debug, Clone, Copy)]
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
    pub jobs_admitted: u64,
    pub jobs_busy_rejected: u64,
    pub jobs_completed: u64,
    pub jobs_cancelled: u64,
    pub jobs_failed: u64,
    pub worker_crashes: u64,
    pub worker_respawns: u64,
}

/// Replies the queue produces to host callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueReply {
    Done(JobOutcome),
    Busy,
    Cancelled(JobId),
    NotFound,
    Stats(QueueStats),
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
    /// Continuation from `PendingCancelableCall::cancel`. Carries the parked
    /// caller's request context so we can answer them after the cancel lands.
    ParkedCallerCancelled {
        id: JobId,
        req: tina::RequestContext<QueueReply>,
    },
}

/// Caller-authority requests the host can ask the queue.
#[derive(Debug)]
pub enum QueueRequest {
    Submit(Payload),
    Cancel(JobId),
    Stats,
}

/// Split-service envelope for [`Queue`]. The `event = .. request = ..`
/// isolate macro form generates the wrong-lane rejection arms, so neither
/// `handle_event` nor `handle_request` writes one by hand.
pub type QueueMsg = tina::ServiceMessage<QueueEvent, QueueRequest>;

/// Worker's reply to a `Process` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerReply {
    Completed(u32),
    Cancelled,
}

/// Fire-and-forget facts a worker accepts: cancel-while-running and the
/// runtime-owned sleep wake-up. Neither carries caller authority.
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    Cancel(JobId),
    /// Internal: the runtime-owned sleep finished.
    Wake { id: JobId, result: SleepReply },
}

/// The one caller-authority request a worker accepts.
#[derive(Debug, Clone)]
pub enum WorkerRequest {
    Process { id: JobId, payload: Payload, sleep_ms: u64 },
}

/// Split-service envelope for [`Worker`]. The `event = .. request = ..`
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
    cancelled: bool,
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
            WorkerEvent::Cancel(id) => {
                if let Some(current) = self.current.as_mut() {
                    if current.id == id {
                        current.cancelled = true;
                    }
                }
                noop()
            }
            WorkerEvent::Wake { id, result: _ } => {
                let Some(current) = self.current.take() else { return noop(); };
                if current.id != id {
                    return noop();
                }
                if current.cancelled {
                    return reply_to::<Self>(current.slot, WorkerReply::Cancelled);
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
            WorkerRequest::Process { id, payload, sleep_ms } => {
                if self.current.is_some() {
                    return call.reject(tina::CallRejectedReason::ReplyAbandoned);
                }
                call.capture(move |req| {
                    let slot = req.into_deferred();
                    self.current = Some(WorkerCurrent { id, payload, cancelled: false, slot });
                    sleep(Duration::from_millis(sleep_ms)).then(move |result| {
                        tina::ServiceMessage::Event(WorkerEvent::Wake { id, result })
                    })
                })
            }
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
    spawned_workers: usize,
    ready_signal: Arc<ReadyGate>,
}

#[tina_runtime::isolate(
    event = QueueEvent,
    request = QueueRequest,
    reply = QueueReply,
    send = tina::Outbound<WorkerMsg>,
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
            QueueEvent::ParkedCallerCancelled { id, req } => {
                self.stats.jobs_cancelled += 1;
                reply_to::<Self>(req, QueueReply::Done(JobOutcome::Cancelled { id }))
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
        }
    }
}

impl Queue {
    fn new(config: RunConfig, ready: Arc<ReadyGate>) -> Self {
        Self {
            config,
            pending: PendingCancelableCallSet::with_capacity(config.workers.max(1)),
            workers: vec![None; config.workers],
            worker_busy: vec![None; config.workers],
            next_id: 1,
            stats: QueueStats {
                workers: config.workers,
                workers_alive: 0,
                in_flight: 0,
                jobs_admitted: 0,
                jobs_busy_rejected: 0,
                jobs_completed: 0,
                jobs_cancelled: 0,
                jobs_failed: 0,
                worker_crashes: 0,
                worker_respawns: 0,
            },
            spawned_workers: 0,
            ready_signal: ready,
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
        spawn_observed(ChildDefinition::new(Worker { current: None }, cap)).then(move |result| {
            QueueMsg::Event(QueueEvent::WorkerStarted { slot, result })
        })
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
                self.stats.workers_alive += 1;
                self.spawned_workers += 1;
                if self.spawned_workers == self.workers.len() {
                    self.ready_signal.signal();
                }
                noop()
            }
            Err(_) => {
                self.workers[slot] = None;
                noop()
            }
        }
    }

    fn submit(
        &mut self,
        payload: Payload,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        let Some(slot) = self.idle_slot() else {
            self.stats.jobs_busy_rejected += 1;
            return call.reply(QueueReply::Busy);
        };
        let Some(worker) = self.workers[slot] else {
            self.stats.jobs_busy_rejected += 1;
            return call.reply(QueueReply::Busy);
        };
        let id = JobId(self.next_id);
        self.next_id += 1;
        let sleep_ms = self.config.job_sleep_ms;
        let dispatch_timeout = Duration::from_millis(sleep_ms.saturating_mul(4) + 1_000);

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
            .try_admit(
                &mut self.pending,
                id,
                move |key, ticket, outcome| {
                    QueueMsg::Event(QueueEvent::WorkerCallReturned {
                        slot,
                        id: key,
                        ticket,
                        outcome,
                    })
                },
            );
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

        // Free the worker slot. The late `WorkerCallReturned` will see the
        // missing pending entry and only tick the late-reply counter.
        let mut worker_addr = None;
        for (slot, busy) in self.worker_busy.iter_mut().enumerate() {
            if *busy == Some(id) {
                *busy = None;
                worker_addr = self.workers[slot];
                break;
            }
        }
        self.stats.in_flight = self.in_flight_count();

        // Order in this batch matters. Replying the cancel API caller
        // first appears to keep the cancel-call translator's later message
        // delivery clean; with the cancel-ack effect last, the
        // `ParkedCallerCancelled` continuation never lands and the parked
        // submit caller gets `Closed` (reply-abandoned). Worth reporting as
        // a Finding — the user shouldn't have to discover this empirically.
        let mut follow_up: Vec<Effect<Self>> = Vec::with_capacity(2);
        follow_up.push(token.cancel(|key, req, _outcome| {
            QueueMsg::Event(QueueEvent::ParkedCallerCancelled { id: key, req })
        }));
        if let Some(worker) = worker_addr {
            // Wake the worker early so it stops sleeping; the worker's reply
            // will be rejected by the runtime since `cancel_call` already
            // closed our wait. This send is opportunistic.
            follow_up.push(tina::send_event::<Self, _, _>(
                worker.events,
                WorkerEvent::Cancel(id),
            ));
        }
        call.reply_and(QueueReply::Cancelled(id), follow_up)
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

        if self.worker_busy[slot] == Some(id) {
            self.worker_busy[slot] = None;
            self.stats.in_flight = self.in_flight_count();
        }

        let req = pending.into_request_context();
        let final_outcome = match outcome {
            CallOutcome::Replied(WorkerReply::Completed(value)) => {
                self.stats.jobs_completed += 1;
                JobOutcome::Completed { id, value }
            }
            CallOutcome::Replied(WorkerReply::Cancelled) => {
                self.stats.jobs_cancelled += 1;
                JobOutcome::Cancelled { id }
            }
            CallOutcome::Closed | CallOutcome::Rejected(_) => {
                self.stats.jobs_failed += 1;
                self.stats.worker_crashes += 1;
                if self.workers[slot].is_some() {
                    self.stats.workers_alive = self.stats.workers_alive.saturating_sub(1);
                }
                self.workers[slot] = None;
                JobOutcome::Failed { id, reason: format!("worker call returned {outcome:?}") }
            }
            CallOutcome::Timeout | CallOutcome::Full => {
                self.stats.jobs_failed += 1;
                JobOutcome::Failed { id, reason: format!("worker call returned {outcome:?}") }
            }
        };

        let respawn_effect = if self.workers[slot].is_none() {
            self.stats.worker_respawns += 1;
            Some(self.spawn_worker(slot))
        } else {
            None
        };

        let reply = reply_to::<Self>(req, QueueReply::Done(final_outcome));
        match respawn_effect {
            Some(spawn) => batch(vec![reply, spawn]),
            None => reply,
        }
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
        s.workers_alive = self.workers.iter().filter(|w| w.is_some()).count();
        s
    }
}

// ---------- Host-visible entry points ----------

/// Bootstrap signal. Each finished spawn ticks the gate; the host waits on
/// it before sending traffic.
#[derive(Debug, Default)]
pub struct ReadyGate {
    inner: Mutex<bool>,
}

impl ReadyGate {
    pub fn signal(&self) {
        *self.inner.lock().expect("ready gate") = true;
    }
    fn ready(&self) -> bool {
        *self.inner.lock().expect("ready gate")
    }
}

/// Aggregate report for a smoke run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub overflow: OverflowReport,
    pub cancel_in_flight: CancelInFlightReport,
    pub poison_crash: PoisonCrashReport,
    pub respawn_then_admit: RespawnThenAdmitReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverflowReport {
    pub completed: usize,
    pub busy: usize,
    pub stats: QueueStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelInFlightReport {
    pub submit_outcome: JobOutcome,
    pub cancel_reply: QueueReply,
    pub stats: QueueStats,
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
    Ok(RunReport {
        overflow: run_overflow(config)?,
        cancel_in_flight: run_cancel_in_flight(config)?,
        poison_crash: run_poison_crash(config)?,
        respawn_then_admit: run_respawn_then_admit(config)?,
    })
}

pub fn run_overflow(config: RunConfig) -> anyhow::Result<OverflowReport> {
    let runtime = Arc::new(ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory));
    let queue = register_queue(&runtime, config)?;
    // Admission cap is `workers`. Burst beyond it.
    let cap = config.workers;
    let burst = cap + 3;
    let timeout = Duration::from_millis(config.call_timeout_ms);

    let barrier = Arc::new(Barrier::new(burst + 1));
    let outcomes = Arc::new(Mutex::new(Vec::with_capacity(burst)));
    let mut threads = Vec::with_capacity(burst);
    for _ in 0..burst {
        let rt = Arc::clone(&runtime);
        let gate = Arc::clone(&barrier);
        let out = Arc::clone(&outcomes);
        threads.push(thread::spawn(move || {
            gate.wait();
            let outcome = rt.call_blocking(queue, QueueMsg::Request(QueueRequest::Submit(Payload::Work(7))), timeout);
            out.lock().expect("outcomes lock").push(outcome);
        }));
    }
    barrier.wait();
    for t in threads {
        t.join().expect("submit thread panicked");
    }

    let mut completed = 0;
    let mut busy = 0;
    for outcome in outcomes.lock().expect("outcomes lock").iter() {
        match outcome {
            Ok(CallOutcome::Replied(QueueReply::Done(JobOutcome::Completed { .. }))) => {
                completed += 1
            }
            Ok(CallOutcome::Replied(QueueReply::Busy)) => busy += 1,
            other => anyhow::bail!("unexpected submit outcome: {other:?}"),
        }
    }

    let stats = stats(&runtime, queue)?;
    shutdown(runtime);
    Ok(OverflowReport { completed, busy, stats })
}

pub fn run_cancel_in_flight(config: RunConfig) -> anyhow::Result<CancelInFlightReport> {
    // Long-running job so cancel reliably lands first.
    let mut config = config;
    config.job_sleep_ms = config.job_sleep_ms.max(150);
    let runtime = Arc::new(ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory));
    let queue = register_queue(&runtime, config)?;
    let timeout = Duration::from_millis(config.call_timeout_ms);

    let rt = Arc::clone(&runtime);
    let submit = thread::spawn(move || {
        rt.call_blocking(queue, QueueMsg::Request(QueueRequest::Submit(Payload::Work(99))), timeout)
    });

    // Let the submit reach the worker before we cancel.
    thread::sleep(Duration::from_millis(config.job_sleep_ms / 4 + 5));
    let cancel_outcome =
        runtime.call_blocking(queue, QueueMsg::Request(QueueRequest::Cancel(JobId(1))), timeout)?;
    let cancel_reply = match cancel_outcome {
        CallOutcome::Replied(reply) => reply,
        other => anyhow::bail!("unexpected cancel outcome: {other:?}"),
    };

    let submit_outcome = match submit.join().expect("submit thread panicked")? {
        CallOutcome::Replied(QueueReply::Done(o)) => o,
        other => anyhow::bail!("unexpected submit outcome: {other:?}"),
    };

    // Give the late worker reply time to land so the late-reply counter
    // ticks before we read stats.
    thread::sleep(Duration::from_millis(config.job_sleep_ms + 30));
    let stats = stats(&runtime, queue)?;
    shutdown(runtime);

    Ok(CancelInFlightReport { submit_outcome, cancel_reply, stats })
}

pub fn run_poison_crash(config: RunConfig) -> anyhow::Result<PoisonCrashReport> {
    let runtime = Arc::new(ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory));
    let queue = register_queue(&runtime, config)?;
    let timeout = Duration::from_millis(config.call_timeout_ms);

    let outcome = runtime.call_blocking(queue, QueueMsg::Request(QueueRequest::Submit(Payload::Poison)), timeout)?;
    let failed_outcome = match outcome {
        CallOutcome::Replied(QueueReply::Done(o)) => o,
        other => anyhow::bail!("unexpected poison outcome: {other:?}"),
    };

    // Poll briefly so the respawn finishes before we read stats.
    wait_until(Duration::from_secs(2), "respawn", || {
        let s = stats(&runtime, queue).unwrap_or_else(|_| panic!("stats failed"));
        s.workers_alive == config.workers
    })?;

    let stats = stats(&runtime, queue)?;
    shutdown(runtime);
    Ok(PoisonCrashReport { failed_outcome, stats })
}

pub fn run_respawn_then_admit(config: RunConfig) -> anyhow::Result<RespawnThenAdmitReport> {
    let runtime = Arc::new(ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory));
    let queue = register_queue(&runtime, config)?;
    let timeout = Duration::from_millis(config.call_timeout_ms);

    let poison = runtime.call_blocking(queue, QueueMsg::Request(QueueRequest::Submit(Payload::Poison)), timeout)?;
    let poison_outcome = match poison {
        CallOutcome::Replied(QueueReply::Done(o)) => o,
        other => anyhow::bail!("unexpected poison outcome: {other:?}"),
    };

    // Wait for the respawn to land before sending the follow-up.
    wait_until(Duration::from_secs(2), "respawn", || {
        stats(&runtime, queue).map(|s| s.workers_alive == config.workers).unwrap_or(false)
    })?;

    let follow_up = runtime.call_blocking(queue, QueueMsg::Request(QueueRequest::Submit(Payload::Work(21))), timeout)?;
    let follow_up_outcome = match follow_up {
        CallOutcome::Replied(QueueReply::Done(o)) => o,
        other => anyhow::bail!("unexpected follow-up outcome: {other:?}"),
    };

    let stats = stats(&runtime, queue)?;
    shutdown(runtime);
    Ok(RespawnThenAdmitReport { poison_outcome, follow_up_outcome, stats })
}

// ---------- Helpers ----------

fn register_queue(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    config: RunConfig,
) -> anyhow::Result<Address<QueueMsg, QueueReply>> {
    let ready = Arc::new(ReadyGate::default());

    let address = runtime
        .register_with_capacity_and_bootstrap::<Queue, WorkerMsg>(
            Queue::new(config, Arc::clone(&ready)),
            config.queue_mailbox,
            QueueMsg::Event(QueueEvent::Bootstrap),
        )
        .map_err(|e| anyhow::anyhow!("register queue: {e:?}"))?;

    wait_until(Duration::from_secs(2), "all workers ready", || ready.ready())?;
    Ok(address)
}

fn wait_until<F>(timeout: Duration, label: &str, mut predicate: F) -> anyhow::Result<()>
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate() {
        if Instant::now() > deadline {
            anyhow::bail!("wait_until({label}) timed out");
        }
        thread::yield_now();
    }
    Ok(())
}

fn stats(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    queue: Address<QueueMsg, QueueReply>,
) -> anyhow::Result<QueueStats> {
    match runtime.call_blocking(queue, QueueMsg::Request(QueueRequest::Stats), Duration::from_secs(2))? {
        CallOutcome::Replied(QueueReply::Stats(s)) => Ok(s),
        other => anyhow::bail!("stats call failed: {other:?}"),
    }
}

fn shutdown(runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>) {
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
