//! Extension smoke crate: a **fake bridge** — one bounded worker around
//! a blocking function — built with only public APIs and the public
//! [`tina_runtime::bridge`] vocabulary.
//!
//! A bridge glues Tina to a messy outside system. Tina can bound
//! admission, observe worker-terminal truth, and own its own deadlines.
//! Tina *cannot* always stop the outside work. This crate makes that
//! honesty concrete with a real OS worker thread and a bounded job queue.
//!
//! What it proves:
//!
//! - **Bounded setup.** The job queue is a bounded `sync_channel`; a
//!   submit past capacity is [`BridgeOutcomeClass::Retryable`]`(BridgeFull)`,
//!   never an unbounded buffer.
//! - **Closer.** [`FakeBridgeCloser`] implements [`BridgeCloser`]:
//!   idempotent `close()` plus visible `is_closed()`. After close, submit
//!   is [`BridgeOutcomeClass::Unavailable`]`(BridgeClosed)`.
//! - **Metrics / pressure.** [`FakeBridgeMetrics::pressure`] renders a
//!   [`BridgePressure`] with installed capacity, in-flight, high-water,
//!   and the rejection/late counters.
//! - **Shutdown.** [`FakeBridgeInstall::close_and_drain`] closes
//!   admission, drains the queue, joins the worker within a deadline, and
//!   returns a [`BridgeDrainReport`].
//! - **Worker-terminal vs caller-observed.** The worker records a
//!   [`BridgeTerminal`] when it finishes. That is separate from what the
//!   caller saw: when a caller's deadline fires first, the bridge replies
//!   [`BridgeCallerWarning::ExternalWorkMayContinue`] — it does **not**
//!   pretend the external work stopped. When the work later lands it is
//!   counted as a late terminal.
//!
//! ## Feeding a Tina isolate
//!
//! This crate captures completions through a result channel so the smoke
//! test stays deterministic. A bridge that feeds a live Tina service
//! delivers each completion as a message to an isolate instead, using
//! only public APIs:
//!
//! ```ignore
//! // From the worker thread, deliver the completion into the isolate
//! // that is waiting on it. `address` comes from
//! // `ThreadedRuntime::register_with_capacity`; the isolate then
//! // replies to the original caller with `reply_to(..)`.
//! runtime.try_send(address, Msg::Completed { id, output })?;
//! ```
//!
//! Nothing in that path is private. The bounded admission, the worker
//! terminal accounting, and the caller warning shown here are exactly
//! what such a bridge surfaces.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tina_runtime::bridge::{
    BridgeCallerWarning, BridgeCloser, BridgeDrainReport, BridgeInstall, BridgeOutcomeClass,
    BridgePressure, BridgeRetryable, BridgeTerminal, BridgeUnavailable,
};

#[derive(Default)]
struct State {
    current: AtomicU64,
    high_water: AtomicU64,
    full_count: AtomicU64,
    timeout_count: AtomicU64,
    closed_count: AtomicU64,
    late_result_count: AtomicU64,
    worker_terminal_count: AtomicU64,
    last_terminal: Mutex<Option<BridgeTerminal>>,
}

struct Job {
    input: u64,
    result_tx: SyncSender<u64>,
    abandoned: Arc<AtomicBool>,
}

/// Config for installing a fake bridge.
pub struct FakeBridgeConfig {
    /// Stable, validated surface name (e.g. `"fake.worker"`).
    pub name: String,
    /// Bounded admission capacity (the job queue depth).
    pub capacity: usize,
}

/// Idempotent admission closer.
#[derive(Clone)]
pub struct FakeBridgeCloser {
    closed: Arc<AtomicBool>,
}

impl BridgeCloser for FakeBridgeCloser {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// Metrics handle: renders the bridge's installed-capacity pressure.
#[derive(Clone)]
pub struct FakeBridgeMetrics {
    name: String,
    capacity: usize,
    state: Arc<State>,
}

impl FakeBridgeMetrics {
    /// Render the current pressure. `capacity` is the installed admission
    /// cap, not a fresh config value.
    pub fn pressure(&self) -> BridgePressure {
        BridgePressure::measured(
            self.name.clone(),
            self.capacity,
            self.state.current.load(Ordering::Acquire) as usize,
            self.state.high_water.load(Ordering::Acquire),
            self.state.full_count.load(Ordering::Acquire),
            self.state.timeout_count.load(Ordering::Acquire),
            self.state.closed_count.load(Ordering::Acquire),
            self.state.late_result_count.load(Ordering::Acquire),
            self.state.worker_terminal_count.load(Ordering::Acquire),
        )
        .expect("static bridge name is valid")
    }

    /// The last worker-terminal outcome the bridge observed, if any.
    /// This is worker-terminal truth, distinct from what a caller saw.
    pub fn last_terminal(&self) -> Option<BridgeTerminal> {
        self.state.last_terminal.lock().unwrap().clone()
    }
}

/// Installed fake bridge: owns the worker thread and the job queue.
pub struct FakeBridgeInstall {
    closer: FakeBridgeCloser,
    metrics: FakeBridgeMetrics,
    tx: Option<SyncSender<Job>>,
    worker: Option<JoinHandle<()>>,
}

impl BridgeInstall for FakeBridgeInstall {
    type Closer = FakeBridgeCloser;
    type Metrics = FakeBridgeMetrics;

    fn closer(&self) -> &FakeBridgeCloser {
        &self.closer
    }

    fn metrics(&self) -> &FakeBridgeMetrics {
        &self.metrics
    }
}

/// Outcome of submitting a job to the bridge.
pub enum SubmitOutcome {
    /// Admitted: hold the ticket and wait for the result with a deadline.
    Admitted(CallTicket),
    /// Rejected before any work was dispatched, with the typed class.
    Rejected(BridgeOutcomeClass),
}

/// What the caller observed while waiting for one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallObserved {
    /// The bridge replied before the caller's deadline.
    Completed {
        /// The worker's output.
        output: u64,
        /// Always [`BridgeCallerWarning::None`] — the work is fully
        /// accounted for.
        warning: BridgeCallerWarning,
    },
    /// The caller's deadline fired first. The outside work may still be
    /// running; `warning` is [`BridgeCallerWarning::ExternalWorkMayContinue`].
    TimedOut {
        /// Honest warning to attach to the caller's reply.
        warning: BridgeCallerWarning,
    },
    /// The worker thread vanished before replying.
    WorkerAborted,
}

/// A handle to wait for one admitted job's result.
pub struct CallTicket {
    result_rx: Receiver<u64>,
    abandoned: Arc<AtomicBool>,
    state: Arc<State>,
}

impl CallTicket {
    /// Wait up to `timeout` for the bridge's reply.
    ///
    /// On timeout the caller stops waiting, but the worker keeps running:
    /// the returned warning is
    /// [`BridgeCallerWarning::ExternalWorkMayContinue`], never a claim
    /// that the work stopped. The eventual terminal is counted as a late
    /// result.
    pub fn wait(self, timeout: Duration) -> CallObserved {
        match self.result_rx.recv_timeout(timeout) {
            Ok(output) => CallObserved::Completed {
                output,
                warning: BridgeCallerWarning::None,
            },
            Err(RecvTimeoutError::Timeout) => {
                self.abandoned.store(true, Ordering::Release);
                self.state.timeout_count.fetch_add(1, Ordering::AcqRel);
                CallObserved::TimedOut {
                    // Project the timeout outcome into the honest warning.
                    warning: BridgeCallerWarning::from_outcome(&BridgeOutcomeClass::Retryable(
                        BridgeRetryable::CallerTimeout,
                    )),
                }
            }
            Err(RecvTimeoutError::Disconnected) => CallObserved::WorkerAborted,
        }
    }
}

/// Install a fake bridge that runs `work` on a bounded worker thread.
pub fn install<F>(config: FakeBridgeConfig, work: F) -> FakeBridgeInstall
where
    F: Fn(u64) -> u64 + Send + 'static,
{
    let state = Arc::new(State::default());
    let closed = Arc::new(AtomicBool::new(false));
    let (tx, rx) = sync_channel::<Job>(config.capacity);
    let worker_state = Arc::clone(&state);
    let worker = thread::spawn(move || worker_loop(rx, work, worker_state));

    FakeBridgeInstall {
        closer: FakeBridgeCloser { closed },
        metrics: FakeBridgeMetrics {
            name: config.name,
            capacity: config.capacity,
            state,
        },
        tx: Some(tx),
        worker: Some(worker),
    }
}

fn worker_loop<F: Fn(u64) -> u64>(rx: Receiver<Job>, work: F, state: Arc<State>) {
    while let Ok(job) = rx.recv() {
        let output = work(job.input);
        // Worker-terminal truth: the worker reached a terminal outcome.
        state.worker_terminal_count.fetch_add(1, Ordering::AcqRel);
        *state.last_terminal.lock().unwrap() =
            Some(BridgeTerminal::Reached(BridgeOutcomeClass::Succeeded));
        state.current.fetch_sub(1, Ordering::AcqRel);
        if job.abandoned.load(Ordering::Acquire) {
            // The caller had already given up: this is a late result, not
            // a fresh reply. Tina stopped waiting; the work did not.
            state.late_result_count.fetch_add(1, Ordering::AcqRel);
        }
        // Deliver if the caller is still listening; ignore otherwise.
        let _ = job.result_tx.send(output);
    }
}

impl FakeBridgeInstall {
    /// Submit one job. Bounded: a full queue is `Retryable(BridgeFull)`;
    /// a closed bridge is `Unavailable(BridgeClosed)`.
    pub fn submit(&self, input: u64) -> SubmitOutcome {
        if self.closer.is_closed() {
            self.metrics
                .state
                .closed_count
                .fetch_add(1, Ordering::AcqRel);
            return SubmitOutcome::Rejected(BridgeOutcomeClass::Unavailable(
                BridgeUnavailable::BridgeClosed,
            ));
        }
        let abandoned = Arc::new(AtomicBool::new(false));
        let (result_tx, result_rx) = sync_channel::<u64>(1);
        let job = Job {
            input,
            result_tx,
            abandoned: Arc::clone(&abandoned),
        };
        let tx = self.tx.as_ref().expect("bridge open");
        match tx.try_send(job) {
            Ok(()) => {
                let now = self.metrics.state.current.fetch_add(1, Ordering::AcqRel) + 1;
                self.metrics
                    .state
                    .high_water
                    .fetch_max(now, Ordering::AcqRel);
                SubmitOutcome::Admitted(CallTicket {
                    result_rx,
                    abandoned,
                    state: Arc::clone(&self.metrics.state),
                })
            }
            Err(TrySendError::Full(_)) => {
                self.metrics.state.full_count.fetch_add(1, Ordering::AcqRel);
                SubmitOutcome::Rejected(BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull))
            }
            Err(TrySendError::Disconnected(_)) => SubmitOutcome::Rejected(
                BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
            ),
        }
    }

    /// Borrow the metrics handle.
    pub fn metrics(&self) -> &FakeBridgeMetrics {
        &self.metrics
    }

    /// Borrow the closer.
    pub fn closer(&self) -> &FakeBridgeCloser {
        &self.closer
    }

    /// Current pressure snapshot.
    pub fn pressure(&self) -> BridgePressure {
        self.metrics.pressure()
    }

    /// Close admission without draining. In-flight work keeps running.
    pub fn close(&self) {
        self.closer.close();
    }

    /// Close admission, drain the queue, and join the worker within
    /// `deadline`. Returns a [`BridgeDrainReport`].
    pub fn close_and_drain(mut self, deadline: Duration) -> BridgeDrainReport {
        self.closer.close();
        // Dropping the sender lets the worker finish its queue and exit.
        drop(self.tx.take());
        let worker = self.worker.take().expect("worker present");
        let start = Instant::now();
        while !worker.is_finished() && start.elapsed() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let drained = worker.is_finished();
        let joined = !drained || worker.join().is_ok();
        // Dropping an unfinished JoinHandle detaches the thread.
        let remaining = self.metrics.state.current.load(Ordering::Acquire);
        BridgeDrainReport::new(
            true,
            drained && joined && remaining == 0,
            remaining,
            vec![("compute", remaining)],
            start.elapsed(),
        )
    }
}

impl Drop for FakeBridgeInstall {
    fn drop(&mut self) {
        // Closing the channel lets the worker drain its queue and exit.
        // Detach rather than block in Drop.
        drop(self.tx.take());
    }
}

/// What the smoke run observed across the four scenarios.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Happy-path completions (3 submitted, 3 replied).
    pub happy_completed: u64,
    /// Happy-path drain reached zero in-flight.
    pub happy_drained: bool,
    /// The timed-out caller saw `ExternalWorkMayContinue`.
    pub caller_saw_external_may_continue: bool,
    /// Late terminal recorded after the caller gave up.
    pub late_result_count: u64,
    /// Worker-terminal observations in the timeout scenario.
    pub worker_terminal_count: u64,
    /// A submit past capacity returned `Retryable(BridgeFull)`.
    pub saw_full: bool,
    /// A submit after close returned `Unavailable(BridgeClosed)`.
    pub saw_closed: bool,
}

/// Drive the four scenarios and report what was observed.
pub fn run() -> Report {
    let happy = run_happy_path();
    let (caller_warn, late, terminals) = run_timeout_and_late_result();
    let saw_full = run_full();
    let saw_closed = run_closed();

    Report {
        happy_completed: happy.0,
        happy_drained: happy.1,
        caller_saw_external_may_continue: caller_warn,
        late_result_count: late,
        worker_terminal_count: terminals,
        saw_full,
        saw_closed,
    }
}

fn run_happy_path() -> (u64, bool) {
    let bridge = install(
        FakeBridgeConfig {
            name: "fake.happy".to_string(),
            capacity: 4,
        },
        |x| x + 1,
    );
    let mut completed = 0;
    for i in 0..3 {
        if let SubmitOutcome::Admitted(ticket) = bridge.submit(i) {
            if let CallObserved::Completed { .. } = ticket.wait(Duration::from_secs(2)) {
                completed += 1;
            }
        }
    }
    let drain = bridge.close_and_drain(Duration::from_secs(2));
    (completed, drain.drained())
}

/// A gate the worker blocks on so the caller-timeout race is deterministic.
struct Gate {
    lock: Mutex<bool>,
    cv: std::sync::Condvar,
}

fn run_timeout_and_late_result() -> (bool, u64, u64) {
    let gate = Arc::new(Gate {
        lock: Mutex::new(false),
        cv: std::sync::Condvar::new(),
    });
    let gate_w = Arc::clone(&gate);
    let bridge = install(
        FakeBridgeConfig {
            name: "fake.timeout".to_string(),
            capacity: 2,
        },
        move |x| {
            // Block until released, so the caller deadline fires first.
            let mut released = gate_w.lock.lock().unwrap();
            while !*released {
                released = gate_w.cv.wait(released).unwrap();
            }
            x * 2
        },
    );

    let warning = match bridge.submit(7) {
        SubmitOutcome::Admitted(ticket) => match ticket.wait(Duration::from_millis(20)) {
            CallObserved::TimedOut { warning } => {
                warning == BridgeCallerWarning::ExternalWorkMayContinue
            }
            _ => false,
        },
        SubmitOutcome::Rejected(_) => false,
    };

    // Release the worker; the abandoned job now lands as a late terminal.
    {
        let mut released = gate.lock.lock().unwrap();
        *released = true;
        gate.cv.notify_all();
    }

    let pressure = {
        // Drain so the worker has certainly finished before we read.
        let _ = bridge.metrics().pressure();
        // close_and_drain consumes the install; read pressure first via a
        // clone of the metrics handle.
        let metrics = bridge.metrics().clone();
        let drain = bridge.close_and_drain(Duration::from_secs(2));
        assert!(drain.drained(), "late-result bridge must drain");
        metrics.pressure()
    };

    (
        warning,
        pressure.late_result_count(),
        pressure.worker_terminal_count(),
    )
}

fn run_full() -> bool {
    let gate = Arc::new(Gate {
        lock: Mutex::new(false),
        cv: std::sync::Condvar::new(),
    });
    let gate_w = Arc::clone(&gate);
    let bridge = install(
        FakeBridgeConfig {
            name: "fake.full".to_string(),
            capacity: 1,
        },
        move |x| {
            let mut released = gate_w.lock.lock().unwrap();
            while !*released {
                released = gate_w.cv.wait(released).unwrap();
            }
            x
        },
    );

    // A: worker picks it up and blocks. B: fills the cap-1 queue. C: Full.
    let _a = bridge.submit(1);
    let _b = bridge.submit(2);
    let saw_full = matches!(
        bridge.submit(3),
        SubmitOutcome::Rejected(BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull))
    );

    {
        let mut released = gate.lock.lock().unwrap();
        *released = true;
        gate.cv.notify_all();
    }
    let drain = bridge.close_and_drain(Duration::from_secs(2));
    saw_full && drain.drained()
}

fn run_closed() -> bool {
    let bridge = install(
        FakeBridgeConfig {
            name: "fake.closed".to_string(),
            capacity: 2,
        },
        |x| x,
    );
    bridge.close();
    let saw_closed = matches!(
        bridge.submit(1),
        SubmitOutcome::Rejected(BridgeOutcomeClass::Unavailable(
            BridgeUnavailable::BridgeClosed
        ))
    );
    let drain = bridge.close_and_drain(Duration::from_secs(2));
    saw_closed && drain.drained()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_bridge_proves_lifecycle_and_caller_honesty() {
        let report = run();
        assert_eq!(report.happy_completed, 3, "all happy jobs completed");
        assert!(report.happy_drained, "happy drain reached zero in-flight");
        assert!(
            report.caller_saw_external_may_continue,
            "a timed-out caller must be told external work may continue"
        );
        assert_eq!(
            report.late_result_count, 1,
            "the abandoned job lands as exactly one late terminal"
        );
        assert_eq!(report.worker_terminal_count, 1);
        assert!(
            report.saw_full,
            "submit past capacity is Retryable(BridgeFull)"
        );
        assert!(
            report.saw_closed,
            "submit after close is Unavailable(BridgeClosed)"
        );
    }

    #[test]
    fn closer_is_idempotent_and_visible() {
        let bridge = install(
            FakeBridgeConfig {
                name: "fake.closer".to_string(),
                capacity: 1,
            },
            |x| x,
        );
        assert!(!bridge.closer().is_closed());
        bridge.closer().close();
        bridge.closer().close(); // idempotent
        assert!(bridge.closer().is_closed());
        assert!(bridge.close_and_drain(Duration::from_secs(1)).drained());
    }
}
