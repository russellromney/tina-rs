//! `system_job_queue` — bounded job queue with N supervised-style worker
//! children, sync `Submit` that parks the caller in `PendingReplies`,
//! `Cancel` that reaches both queued and in-flight jobs, and a retry budget
//! that re-dispatches a job after a worker crash.
//!
//! What this specimen pulls on:
//!
//! - [`PendingReplies`] for parked `Submit` callers waiting for completion.
//! - [`CallContext`] for `Submit`/`Cancel`/`Stats` caller authority.
//! - `spawn_observed(ChildDefinition::new(...))` for typed child refs.
//! - `send_observed(...).then(...)` to detect when a worker mailbox is dead
//!   without polling.
//! - `sleep(d).then(...)` for runtime-owned simulated job time.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::{CallContext, ChildDefinition, PendingCallSet, prelude::*};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, PendingReplies, SleepReply, ThreadedRuntime,
    call_cancelable, sleep,
};

/// Tunables for one specimen run.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub workers: usize,
    pub queue_capacity: usize,
    pub pending_capacity: usize,
    pub queue_mailbox: usize,
    pub worker_mailbox: usize,
    pub job_sleep_ms: u64,
    pub call_timeout_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            queue_capacity: 4,
            pending_capacity: 8,
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
/// observe a dead mailbox and exercise replacement + retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Work(u32),
    Poison,
}

/// What [`QueueMsg::Submit`] eventually replies with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// Worker returned a value.
    Completed { id: JobId, value: u32, attempts: u32 },
    /// A `Cancel` reached the job before it finished.
    Cancelled { id: JobId },
    /// Retry budget exhausted — every attempt either crashed the worker
    /// or returned an error before a clean reply.
    Failed { id: JobId, attempts: u32, reason: String },
}

/// Queue stats snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStats {
    pub workers: usize,
    pub workers_alive: usize,
    pub queued: usize,
    pub in_flight: usize,
    pub jobs_admitted: u64,
    pub jobs_busy_rejected: u64,
    pub jobs_completed: u64,
    pub jobs_cancelled: u64,
    pub jobs_failed: u64,
    pub worker_crashes: u64,
    pub worker_respawns: u64,
    pub retries_used: u64,
    pub pending_high_water: usize,
    pub pending_full_rejects: u64,
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

/// Messages routed into the queue isolate.
#[derive(Debug)]
pub enum QueueMsg {
    /// One-shot bootstrap: spawn the worker pool. Sent by the host after
    /// the queue is registered.
    Bootstrap,
    Submit { payload: Payload, max_retries: u32 },
    Cancel(JobId),
    Stats,
    WorkerStarted {
        slot: usize,
        result: tina::SpawnObservedResult<WorkerMsg, WorkerReply>,
    },
    WorkerCallReturned {
        slot: usize,
        id: JobId,
        outcome: CallOutcome<WorkerReply>,
    },
}

/// Worker's reply to a `Process` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerReply {
    Completed(u32),
    Cancelled,
}

/// Messages a worker accepts.
#[derive(Debug, Clone)]
pub enum WorkerMsg {
    Process { id: JobId, payload: Payload, sleep_ms: u64 },
    Cancel(JobId),
    /// Internal: the runtime-owned sleep finished.
    Wake { id: JobId, result: SleepReply },
}

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

#[tina_runtime::isolate(message = WorkerMsg, reply = WorkerReply)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
        match msg {
            // Caller-authority variants land in handle_call.
            WorkerMsg::Process { .. } => noop(),
            WorkerMsg::Cancel(id) => {
                if let Some(current) = self.current.as_mut() {
                    if current.id == id {
                        current.cancelled = true;
                    }
                }
                noop()
            }
            WorkerMsg::Wake { id, result: _ } => {
                let Some(current) = self.current.take() else { return noop(); };
                if current.id != id {
                    return noop();
                }
                if current.cancelled {
                    return reply_to::<Self>(current.slot, WorkerReply::Cancelled);
                }
                match current.payload {
                    Payload::Poison => {
                        // Panic the worker mailbox. The queue's outstanding
                        // call resolves as `CallOutcome::Closed`, which is
                        // the visible signal we use to drive retry/respawn.
                        panic!("worker poisoned by job {id:?}");
                    }
                    Payload::Work(n) => reply_to::<Self>(current.slot, WorkerReply::Completed(n.wrapping_mul(2))),
                }
            }
        }
    }

    fn handle_call(&mut self, msg: WorkerMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkerMsg::Process { id, payload, sleep_ms } => {
                if self.current.is_some() {
                    return call.reject(tina::CallRejectedReason::ReplyAbandoned);
                }
                let slot = call.into_request_context().into_deferred();
                self.current = Some(WorkerCurrent { id, payload, cancelled: false, slot });
                sleep(Duration::from_millis(sleep_ms))
                    .then(move |result| WorkerMsg::Wake { id, result })
            }
            WorkerMsg::Cancel(_) | WorkerMsg::Wake { .. } => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

// ---------- Queue isolate ----------

#[derive(Debug)]
struct JobRecord {
    payload: Payload,
    sleep_ms: u64,
    max_retries: u32,
    attempts: u32,
    state: JobState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JobState {
    Queued,
    /// Dispatched to worker `slot`, awaiting the worker's report.
    Running { slot: usize, cancel_requested: bool },
}

struct Queue {
    #[allow(dead_code)]
    self_addr: Address<QueueMsg, QueueReply>,
    config: RunConfig,
    pending: PendingReplies<JobId, QueueReply>,
    in_flight_calls: PendingCallSet<JobId, WorkerReply>,
    workers: Vec<Option<Address<WorkerMsg, WorkerReply>>>,
    worker_busy: Vec<Option<JobId>>,
    queue: VecDeque<JobId>,
    jobs: HashMap<JobId, JobRecord>,
    next_id: u64,
    stats: QueueStats,
    spawned_workers: usize,
    ready_signal: Arc<ReadyGate>,
}

#[tina_runtime::isolate(
    message = QueueMsg,
    reply = QueueReply,
    send = tina::Outbound<WorkerMsg>,
    call = tina_runtime::RuntimeCall<QueueMsg>,
    spawn_observed = tina::SpawnObserved<ChildDefinition<Worker>, QueueMsg, WorkerMsg, WorkerReply>,
)]
impl Queue {
    fn handle(&mut self, msg: QueueMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
        match msg {
            QueueMsg::Bootstrap => self.spawn_all_workers(),
            // request/reply variants land in `handle_call`; if a host ever
            // mis-routes them through plain `try_send`, swallow them.
            QueueMsg::Submit { .. } | QueueMsg::Cancel(_) | QueueMsg::Stats => noop(),
            QueueMsg::WorkerStarted { slot, result } => self.on_worker_started(slot, result),
            QueueMsg::WorkerCallReturned { slot, id, outcome } => {
                self.on_worker_call_returned(slot, id, outcome)
            }
        }
    }

    fn handle_call(&mut self, msg: QueueMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            QueueMsg::Submit { payload, max_retries } => self.submit(payload, max_retries, call),
            QueueMsg::Cancel(id) => self.cancel(id, call),
            QueueMsg::Stats => call.reply(QueueReply::Stats(self.snapshot())),
            QueueMsg::Bootstrap
            | QueueMsg::WorkerStarted { .. }
            | QueueMsg::WorkerCallReturned { .. } => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl Queue {
    fn new(self_addr: Address<QueueMsg, QueueReply>, config: RunConfig, ready: Arc<ReadyGate>) -> Self {
        Self {
            self_addr,
            config,
            pending: PendingReplies::with_capacity(config.pending_capacity)
                .named("system_job_queue.pending"),
            in_flight_calls: PendingCallSet::with_capacity(config.workers.max(1)),
            workers: vec![None; config.workers],
            worker_busy: vec![None; config.workers],
            queue: VecDeque::with_capacity(config.queue_capacity),
            jobs: HashMap::new(),
            next_id: 1,
            stats: QueueStats {
                workers: config.workers,
                workers_alive: 0,
                queued: 0,
                in_flight: 0,
                jobs_admitted: 0,
                jobs_busy_rejected: 0,
                jobs_completed: 0,
                jobs_cancelled: 0,
                jobs_failed: 0,
                worker_crashes: 0,
                worker_respawns: 0,
                retries_used: 0,
                pending_high_water: 0,
                pending_full_rejects: 0,
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
        spawn_observed(ChildDefinition::new(Worker { current: None }, cap))
            .then(move |result| QueueMsg::WorkerStarted { slot, result })
    }

    fn on_worker_started(
        &mut self,
        slot: usize,
        result: tina::SpawnObservedResult<WorkerMsg, WorkerReply>,
    ) -> Effect<Self> {
        match result {
            Ok(child) => {
                self.workers[slot] = Some(child.address);
                self.stats.workers_alive += 1;
                self.spawned_workers += 1;
                if self.spawned_workers == self.workers.len() {
                    self.ready_signal.signal();
                }
                self.dispatch_next()
            }
            Err(_) => {
                // Spawn rejected at construction (e.g. zero capacity). This
                // is a configuration bug. Mark slot dead and move on so the
                // pool runs degraded instead of stalling silently.
                self.workers[slot] = None;
                noop()
            }
        }
    }

    fn submit(&mut self, payload: Payload, max_retries: u32, call: CallContext<'_, Self>) -> Effect<Self> {
        if self.queue.len() >= self.config.queue_capacity && self.idle_slot().is_none() {
            self.stats.jobs_busy_rejected += 1;
            return call.reply(QueueReply::Busy);
        }
        let id = JobId(self.next_id);
        self.next_id += 1;
        let slot = call.into_request_context().into_deferred();
        if let Err(error) = self.pending.try_insert(id, slot) {
            return match error {
                tina_runtime::PendingRepliesInsertError::Full(_, slot) => {
                    self.stats.pending_full_rejects = self.pending.full_rejects();
                    self.stats.jobs_busy_rejected += 1;
                    reply_to(slot, QueueReply::Busy)
                }
                tina_runtime::PendingRepliesInsertError::DuplicateKey(_, slot) => {
                    reply_to(slot, QueueReply::Done(JobOutcome::Failed {
                        id,
                        attempts: 0,
                        reason: "duplicate job id (queue bug)".into(),
                    }))
                }
            };
        }
        self.stats.pending_high_water = self.pending.high_water();
        self.stats.jobs_admitted += 1;
        self.jobs.insert(
            id,
            JobRecord {
                payload,
                sleep_ms: self.config.job_sleep_ms,
                max_retries,
                attempts: 0,
                state: JobState::Queued,
            },
        );
        self.queue.push_back(id);
        self.stats.queued = self.queue.len();
        self.dispatch_next()
    }

    fn cancel(&mut self, id: JobId, call: CallContext<'_, Self>) -> Effect<Self> {
        let Some(record) = self.jobs.get_mut(&id) else {
            return call.reply(QueueReply::NotFound);
        };
        match record.state.clone() {
            JobState::Queued => {
                self.queue.retain(|q| *q != id);
                self.stats.queued = self.queue.len();
                self.jobs.remove(&id);
                let mut effects = Vec::with_capacity(2);
                if let Some(slot) = self.pending.take(&id) {
                    self.stats.jobs_cancelled += 1;
                    effects.push(reply_to::<Self>(
                        slot,
                        QueueReply::Done(JobOutcome::Cancelled { id }),
                    ));
                }
                effects.push(call.reply(QueueReply::Cancelled(id)));
                batch(effects)
            }
            JobState::Running { slot, .. } => {
                record.state = JobState::Running { slot, cancel_requested: true };
                let cancel_send = match self.workers[slot] {
                    Some(addr) => send(addr, WorkerMsg::Cancel(id)),
                    None => noop(),
                };
                batch(vec![cancel_send, call.reply(QueueReply::Cancelled(id))])
                // Note: we keep the in-flight call handle alive so the
                // worker's eventual `WorkerReply::Cancelled` reply still
                // routes through `on_worker_call_returned`, which is where
                // the parked submit caller is replied to.
            }
        }
    }

    fn dispatch_next(&mut self) -> Effect<Self> {
        let mut effects = Vec::new();
        loop {
            if self.queue.is_empty() {
                break;
            }
            let Some(slot) = self.idle_slot() else { break };
            let Some(worker) = self.workers[slot] else { break };
            let id = self.queue.pop_front().expect("queue not empty");
            self.stats.queued = self.queue.len();
            let (payload, sleep_ms) = match self.jobs.get_mut(&id) {
                Some(record) => {
                    record.attempts += 1;
                    if record.attempts > 1 {
                        self.stats.retries_used += 1;
                    }
                    record.state = JobState::Running { slot, cancel_requested: false };
                    (record.payload.clone(), record.sleep_ms)
                }
                None => continue,
            };
            self.worker_busy[slot] = Some(id);
            self.stats.in_flight = self.in_flight_count();
            // The dispatch timeout is the worker job time plus generous slack.
            // A worker that panics mid-sleep produces `CallOutcome::Closed`
            // before the timeout; the timeout only matters if a worker
            // wedges entirely.
            let dispatch_timeout = Duration::from_millis(sleep_ms.saturating_mul(4) + 1_000);
            let (effect, handle) = call_cancelable(
                worker,
                WorkerMsg::Process { id, payload, sleep_ms },
                dispatch_timeout,
            )
            .then(move |outcome| QueueMsg::WorkerCallReturned { slot, id, outcome });
            // Stash the handle so a `Cancel` for an in-flight job can
            // close our wait via `cancel_call`. A duplicate id here would
            // be a queue accounting bug — let it surface loudly.
            if let Err(err) = self.in_flight_calls.insert(id, handle) {
                effects.push(self.finish_job(
                    id,
                    JobOutcome::Failed {
                        id,
                        attempts: 1,
                        reason: format!("in_flight_calls insert: {err:?}"),
                    },
                ));
                continue;
            }
            effects.push(effect);
        }
        if effects.is_empty() {
            noop()
        } else {
            batch(effects)
        }
    }

    fn on_worker_call_returned(
        &mut self,
        slot: usize,
        id: JobId,
        outcome: CallOutcome<WorkerReply>,
    ) -> Effect<Self> {
        // Stale call returning after we've moved on (e.g., job was cancelled
        // and removed). Free the slot anyway and dispatch.
        let was_running_here = self.worker_busy[slot] == Some(id);
        if was_running_here {
            self.worker_busy[slot] = None;
            self.stats.in_flight = self.in_flight_count();
        }
        let _ = self.in_flight_calls.remove(&id);

        let Some(record) = self.jobs.get_mut(&id) else { return self.dispatch_next() };
        let attempts = record.attempts;
        let cancel_requested = matches!(record.state, JobState::Running { cancel_requested: true, .. });
        let max_retries = record.max_retries;

        let final_outcome = match outcome {
            CallOutcome::Replied(WorkerReply::Completed(value)) if !cancel_requested => {
                Some(JobOutcome::Completed { id, value, attempts })
            }
            CallOutcome::Replied(_) => Some(JobOutcome::Cancelled { id }),
            // Worker stopped (panicked) or rejected the call. Both count as
            // a crash for retry-budget purposes.
            CallOutcome::Closed | CallOutcome::Rejected(_) => {
                self.stats.worker_crashes += 1;
                if self.workers[slot].is_some() {
                    self.stats.workers_alive = self.stats.workers_alive.saturating_sub(1);
                }
                self.workers[slot] = None;
                self.retry_or_fail(id, attempts, max_retries, "worker crashed")
            }
            CallOutcome::Timeout => self.retry_or_fail(id, attempts, max_retries, "worker call timed out"),
            CallOutcome::Full => self.retry_or_fail(id, attempts, max_retries, "worker mailbox full"),
        };

        let respawn_effect = if self.workers[slot].is_none() {
            self.stats.worker_respawns += 1;
            Some(self.spawn_worker(slot))
        } else {
            None
        };

        let mut effects: Vec<Effect<Self>> = Vec::new();
        if let Some(outcome) = final_outcome {
            effects.push(self.finish_job(id, outcome));
        }
        if let Some(eff) = respawn_effect {
            effects.push(eff);
        }
        effects.push(self.dispatch_next());
        batch(effects)
    }

    fn retry_or_fail(
        &mut self,
        id: JobId,
        attempts: u32,
        max_retries: u32,
        reason: &str,
    ) -> Option<JobOutcome> {
        if attempts <= max_retries {
            if let Some(record) = self.jobs.get_mut(&id) {
                record.state = JobState::Queued;
            }
            self.queue.push_back(id);
            self.stats.queued = self.queue.len();
            None
        } else {
            Some(JobOutcome::Failed { id, attempts, reason: reason.into() })
        }
    }

    fn finish_job(&mut self, id: JobId, outcome: JobOutcome) -> Effect<Self> {
        match &outcome {
            JobOutcome::Completed { .. } => self.stats.jobs_completed += 1,
            JobOutcome::Cancelled { .. } => self.stats.jobs_cancelled += 1,
            JobOutcome::Failed { .. } => self.stats.jobs_failed += 1,
        }
        self.jobs.remove(&id);
        match self.pending.take(&id) {
            Some(slot) => reply_to::<Self>(slot, QueueReply::Done(outcome)),
            None => noop(),
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
        s.queued = self.queue.len();
        s.in_flight = self.in_flight_count();
        s.workers_alive = self.workers.iter().filter(|w| w.is_some()).count();
        s.pending_high_water = self.pending.high_water();
        s.pending_full_rejects = self.pending.full_rejects();
        s
    }
}

// ---------- Host-visible entry points ----------

/// Bootstrap signal so the host can wait for every worker child to be live
/// before it starts submitting jobs. Each spawn outcome ticks the gate.
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
    pub cancel_queued: CancelQueuedReport,
    pub poison_retry: PoisonRetryReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverflowReport {
    /// How many `Submit` calls returned `Done(Completed)` after the burst
    /// exceeded the queue capacity.
    pub completed: usize,
    /// How many `Submit` calls were rejected with `Busy`.
    pub busy: usize,
    pub stats: QueueStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelQueuedReport {
    pub cancelled_jobs: usize,
    pub completed_jobs: usize,
    pub stats: QueueStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoisonRetryReport {
    pub failed_outcome: JobOutcome,
    pub stats: QueueStats,
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    Ok(RunReport {
        overflow: run_overflow(config)?,
        cancel_queued: run_cancel_queued(config)?,
        poison_retry: run_poison_retry(config)?,
    })
}

pub fn run_overflow(config: RunConfig) -> anyhow::Result<OverflowReport> {
    let runtime = Arc::new(ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory));
    let queue = register_queue(&runtime, config)?;

    // The total admission cap is queue_capacity + workers (queued + in-flight).
    let cap = config.queue_capacity + config.workers;
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
            let outcome = rt.call_blocking(
                queue,
                QueueMsg::Submit { payload: Payload::Work(7), max_retries: 0 },
                timeout,
            );
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

pub fn run_cancel_queued(config: RunConfig) -> anyhow::Result<CancelQueuedReport> {
    // Use a longer-running job so cancels reliably land before completion.
    let mut config = config;
    config.job_sleep_ms = config.job_sleep_ms.max(150);
    let runtime = Arc::new(ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory));
    let queue = register_queue(&runtime, config)?;
    let timeout = Duration::from_millis(config.call_timeout_ms);

    // Submit (workers + queue_capacity) jobs concurrently. The first
    // `workers` jobs go in-flight; the rest sit queued.
    let total = config.workers + config.queue_capacity;
    let barrier = Arc::new(Barrier::new(total + 1));
    let outcomes = Arc::new(Mutex::new(Vec::with_capacity(total)));
    let mut threads = Vec::with_capacity(total);
    for _ in 0..total {
        let rt = Arc::clone(&runtime);
        let gate = Arc::clone(&barrier);
        let out = Arc::clone(&outcomes);
        threads.push(thread::spawn(move || {
            gate.wait();
            let outcome = rt.call_blocking(
                queue,
                QueueMsg::Submit { payload: Payload::Work(11), max_retries: 0 },
                timeout,
            );
            out.lock().expect("outcomes lock").push(outcome);
        }));
    }
    barrier.wait();

    // Wait long enough for all workers to be busy and the rest to be queued.
    let settle = Duration::from_millis(config.job_sleep_ms / 4 + 10);
    thread::sleep(settle);

    // Cancel every queued JobId (we know they are 1..=total but ones in
    // flight are the lower-numbered ids assigned first). Walk the high end
    // since later submissions land in the queue.
    let mut cancelled = 0;
    for raw in (1..=total as u64).rev().take(config.queue_capacity) {
        let reply = runtime.call_blocking(queue, QueueMsg::Cancel(JobId(raw)), timeout)?;
        if matches!(reply, CallOutcome::Replied(QueueReply::Cancelled(_))) {
            cancelled += 1;
        }
    }

    for t in threads {
        t.join().expect("submit thread panicked");
    }

    let mut completed = 0;
    let mut cancelled_outcomes = 0;
    for outcome in outcomes.lock().expect("outcomes lock").iter() {
        match outcome {
            Ok(CallOutcome::Replied(QueueReply::Done(JobOutcome::Completed { .. }))) => {
                completed += 1
            }
            Ok(CallOutcome::Replied(QueueReply::Done(JobOutcome::Cancelled { .. }))) => {
                cancelled_outcomes += 1
            }
            other => anyhow::bail!("unexpected submit outcome: {other:?}"),
        }
    }
    let _ = cancelled; // cancel calls returned Cancelled; report uses the parked outcome

    let stats = stats(&runtime, queue)?;
    shutdown(runtime);
    Ok(CancelQueuedReport {
        cancelled_jobs: cancelled_outcomes,
        completed_jobs: completed,
        stats,
    })
}

pub fn run_poison_retry(config: RunConfig) -> anyhow::Result<PoisonRetryReport> {
    let runtime = Arc::new(ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory));
    let queue = register_queue(&runtime, config)?;
    let timeout = Duration::from_millis(config.call_timeout_ms);

    let outcome = runtime.call_blocking(
        queue,
        QueueMsg::Submit { payload: Payload::Poison, max_retries: 2 },
        timeout,
    )?;
    let final_outcome = match outcome {
        CallOutcome::Replied(QueueReply::Done(o)) => o,
        other => anyhow::bail!("unexpected poison outcome: {other:?}"),
    };

    let stats = stats(&runtime, queue)?;
    shutdown(runtime);
    Ok(PoisonRetryReport { failed_outcome: final_outcome, stats })
}

// ---------- Helpers ----------

fn register_queue(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    config: RunConfig,
) -> anyhow::Result<Address<QueueMsg, QueueReply>> {
    let ready = Arc::new(ReadyGate::default());
    let ready_for_isolate = Arc::clone(&ready);

    let address = runtime
        .register_with_capacity_using::<_, WorkerMsg, _>(config.queue_mailbox, move |self_addr| {
            Queue::new(self_addr, config, ready_for_isolate)
        })
        .map_err(|e| anyhow::anyhow!("register queue: {e:?}"))?;

    runtime
        .try_send(address, QueueMsg::Bootstrap)
        .map_err(|e| anyhow::anyhow!("send bootstrap: {e:?}"))?;

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
    match runtime.call_blocking(queue, QueueMsg::Stats, Duration::from_secs(2))? {
        CallOutcome::Replied(QueueReply::Stats(s)) => Ok(s),
        other => anyhow::bail!("stats call failed: {other:?}"),
    }
}

fn shutdown(runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>) {
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
