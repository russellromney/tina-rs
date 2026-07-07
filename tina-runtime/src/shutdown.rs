//! Non-consuming shutdown shape for threaded runtimes.
//!
//! Both [`crate::ThreadedRuntime`] and [`crate::ThreadedMultiShardRuntime`]
//! own a [`SharedShutdownState`] behind an `Arc`. The runtime owner controls
//! lifetime as before; [`ThreadedShutdownHandle`] hands out cloneable
//! request/wait access without requiring `Arc::try_unwrap(runtime)`.
//!
//! Pinned contract (see `.intent/phases/102-host-control-ergonomics/plan.md`):
//!
//! - [`ThreadedShutdownHandle::request_shutdown`] is idempotent and
//!   nonblocking; full command queue → [`ShutdownRequestError::CommandFull`].
//! - [`ThreadedShutdownHandle::wait_report`] only waits; it never requests
//!   shutdown. While the runtime is still live and no one has
//!   requested/dropped shutdown, it returns [`ShutdownWaitError::Timeout`].
//! - Terminal truth is cached after the first successful join; every later
//!   waiter (and the consuming `shutdown_report(self)`) gets the same
//!   cloneable [`LocalSystemTerminalReport`].
//! - Runtime `Drop`, consuming `shutdown_report(self)`, and handle waits all
//!   route through the same shared state — no second shutdown path.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Shard, ShardId};

use crate::errors::{ShutdownRequestError, ShutdownWaitError, ThreadedRuntimeError};
use crate::live_report::{
    LiveQueueMetrics, LiveRemoteQueueReport, LiveShardMetrics, LiveShardState, LiveTopologyReport,
};
use crate::local_system::{
    LocalSystemState, LocalSystemTerminalReport, ThreadedWorkerExit, ThreadedWorkerJoin,
};
use crate::mailbox::MailboxFactory;
use crate::threaded::{CommandSender, ThreadedCommand};

/// Cloneable handle that controls runtime-level shutdown without consuming
/// the underlying runtime.
///
/// Returned by [`crate::ThreadedRuntime::shutdown_handle`] and
/// [`crate::ThreadedMultiShardRuntime::shutdown_handle`]. Dropping a handle
/// does **not** trigger shutdown; the runtime owner controls lifetime.
///
/// This is host-control ergonomics, not a service-drain framework. A
/// service that needs a graceful application drain still exposes its own
/// `Stop` / `Drain` protocol (for example [`crate::DrainState`]); this handle
/// only asks the runtime/control plane to begin shutdown.
#[derive(Clone)]
pub struct ThreadedShutdownHandle {
    inner: Arc<dyn ShutdownInner>,
}

impl ThreadedShutdownHandle {
    pub(crate) fn new(inner: Arc<dyn ShutdownInner>) -> Self {
        Self { inner }
    }

    /// Requests that the runtime begin shutdown. Idempotent and nonblocking.
    ///
    /// Returns immediately. On success, every owned worker has had a
    /// `Shutdown` command admitted and the terminal report becomes
    /// available through [`Self::wait_report`].
    ///
    /// On a multi-shard runtime, the call walks workers in deterministic
    /// shard-id order. Each shard whose command queue admits `Shutdown`
    /// is marked signaled and **stays signaled across retries** — a
    /// later [`Self::request_shutdown`] resumes from where the previous
    /// attempt stopped. This keeps the request idempotent in the useful
    /// sense (a fully-shut-down runtime returns `Ok` without re-sending)
    /// while honoring the "nonblocking" contract: a saturated shard
    /// bails immediately with `CommandFull` rather than blocking.
    ///
    /// Errors:
    /// - `CommandFull { shard }` — the named shard's command queue was
    ///   full at this attempt. Earlier shards in iteration order may
    ///   have been signaled. Retry to continue.
    /// - `WorkerStopped { shard }` — the named worker was already gone;
    ///   it is treated as already signaled so a retry will not re-try it.
    pub fn request_shutdown(&self) -> Result<(), ShutdownRequestError> {
        self.inner.request_shutdown()
    }

    /// Waits up to `timeout` for the terminal report.
    ///
    /// This does **not** request shutdown. While the runtime is still live
    /// and no one has requested or dropped shutdown, this returns
    /// [`ShutdownWaitError::Timeout`]. Once a terminal report has been
    /// cached, every subsequent caller receives the same cloned report
    /// without further blocking.
    pub fn wait_report(
        &self,
        timeout: Duration,
    ) -> Result<LocalSystemTerminalReport, ShutdownWaitError> {
        self.inner.wait_report(timeout)
    }
}

impl std::fmt::Debug for ThreadedShutdownHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadedShutdownHandle").finish()
    }
}

pub(crate) trait ShutdownInner: Send + Sync {
    fn request_shutdown(&self) -> Result<(), ShutdownRequestError>;
    fn wait_report(
        &self,
        timeout: Duration,
    ) -> Result<LocalSystemTerminalReport, ShutdownWaitError>;
}

/// One worker entry tracked by the shared shutdown state.
pub(crate) struct ShutdownWorker<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    pub(crate) shard: ShardId,
    pub(crate) commands: CommandSender<S, F>,
    pub(crate) handle: Option<ThreadedWorkerJoin>,
    pub(crate) metrics: Arc<LiveShardMetrics>,
    /// `true` once a `Shutdown` command has been admitted to this
    /// worker (or the worker was already gone). Skipping re-signal on
    /// retries keeps partial-shutdown attempts idempotent.
    pub(crate) signaled: bool,
}

/// Shared, internally synchronised shutdown coordinator.
pub(crate) struct SharedShutdownState<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    state: Mutex<ShutdownState<S, F>>,
    condvar: Condvar,
    /// `true` if this state belongs to a multi-shard runtime; controls
    /// whether `ShutdownRequestError` carries a shard id.
    is_multi_shard: bool,
    /// Optional cross-shard queue metrics for multi-shard runtimes. Empty
    /// for single-shard.
    remote_metrics: BTreeMap<(ShardId, ShardId), Arc<LiveQueueMetrics>>,
}

pub(crate) struct ShutdownState<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    pub(crate) workers: Vec<ShutdownWorker<S, F>>,
    pub(crate) shutdown_requested: bool,
    pub(crate) joining: bool,
    pub(crate) report: Option<LocalSystemTerminalReport>,
    /// Joiner-thread failure (panic). Surfaced through
    /// [`ShutdownWaitError::WorkerStopped`].
    pub(crate) joiner_failed: bool,
}

impl<S, F> SharedShutdownState<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    pub(crate) fn single_shard(worker: ShutdownWorker<S, F>) -> Self {
        Self {
            state: Mutex::new(ShutdownState {
                workers: vec![worker],
                shutdown_requested: false,
                joining: false,
                report: None,
                joiner_failed: false,
            }),
            condvar: Condvar::new(),
            is_multi_shard: false,
            remote_metrics: BTreeMap::new(),
        }
    }

    pub(crate) fn multi_shard(
        workers: Vec<ShutdownWorker<S, F>>,
        remote_metrics: BTreeMap<(ShardId, ShardId), Arc<LiveQueueMetrics>>,
    ) -> Self {
        Self {
            state: Mutex::new(ShutdownState {
                workers,
                shutdown_requested: false,
                joining: false,
                report: None,
                joiner_failed: false,
            }),
            condvar: Condvar::new(),
            is_multi_shard: true,
            remote_metrics,
        }
    }

    /// Blocking shutdown used by `Drop` and consuming `shutdown_report`.
    ///
    /// Only signals workers that have not been marked `signaled` by a
    /// prior bounded `request_shutdown`. The runtime-owner path uses
    /// blocking `send` so teardown always converges even if the queue is
    /// currently full.
    /// Blocking shutdown used by `Drop` and consuming `shutdown_report`.
    ///
    /// Only signals workers that have not been marked `signaled` by a
    /// prior bounded `request_shutdown`. The runtime-owner path uses
    /// blocking `send` so teardown always converges even if the queue is
    /// currently full.
    ///
    /// Note: send-side errors are not used as a `Failed` signal here. A
    /// `Disconnected` send only proves the worker thread has exited —
    /// possibly cleanly via `Shutdown`. The joiner uses `handle.join()`
    /// as the single source of truth for terminal state.
    pub(crate) fn shutdown_blocking(self: &Arc<Self>) {
        // Snapshot unsignaled senders, drop the lock, then send blocking.
        // The send happens without the lock so a saturated command queue
        // doesn't lock the runtime owner against handles that hold the
        // shared state's lock.
        let pending: Vec<(usize, CommandSender<S, F>)> = {
            let state = self.lock_state();
            state
                .workers
                .iter()
                .enumerate()
                .filter(|(_, w)| !w.signaled)
                .map(|(i, w)| (i, w.commands.clone()))
                .collect()
        };
        for (idx, sender) in pending {
            let _ = sender.send(ThreadedCommand::Shutdown);
            let mut state = self.lock_state();
            if let Some(worker) = state.workers.get_mut(idx) {
                worker.signaled = true;
            }
        }
        // Mark the request as committed so `request_shutdown_bounded`
        // becomes idempotent and `wait_report_*` stops returning
        // pre-request `Timeout`.
        {
            let mut state = self.lock_state();
            state.shutdown_requested = true;
        }
        self.ensure_joiner_started();
    }

    fn ensure_joiner_started(self: &Arc<Self>) {
        let mut state = self.lock_state();
        if state.joining || state.report.is_some() {
            return;
        }
        // Only start the joiner once every shard has had a `Shutdown`
        // command admitted (or been observed gone). Joining a worker
        // that hasn't seen `Shutdown` would block the joiner forever
        // because the worker is still waiting on its command queue.
        if !state.workers.iter().all(|w| w.signaled) {
            return;
        }
        // Nothing left to join (every handle already taken by a prior joiner):
        // don't spin up a joiner that would cache an empty report.
        if !state.workers.iter().any(|w| w.handle.is_some()) {
            return;
        }
        state.joining = true;
        drop(state);

        // Do NOT take the worker handles here. `run_joiner` takes them under
        // the lock itself, so a failed `spawn` re-runs the joiner inline with
        // the handles still present. Taking them before the move would drop
        // them with the un-spawned closure on a `spawn` error, silently
        // leaking every worker thread while caching a false `Closed` report
        // with no events (E1).
        let spawned = if force_joiner_spawn_failure() {
            false
        } else {
            let shared = Arc::clone(self);
            thread::Builder::new()
                .name("tina-shutdown-joiner".to_string())
                .spawn(move || joiner_main(shared))
                .is_ok()
        };
        if !spawned {
            // Failed to spawn the joiner thread (rare; FD/thread-limit
            // exhaustion). Join inline so no worker leaks and the report
            // reflects real exits. The handles are still in state, so the
            // panic-safe `joiner_main` wrapper takes and joins them here.
            joiner_main(Arc::clone(self));
        }
    }

    /// Acquire the state lock, transparently recovering from a poisoned
    /// mutex so a joiner-thread panic doesn't cascade-panic every waiter.
    fn lock_state(&self) -> MutexGuard<'_, ShutdownState<S, F>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Blocking wait used by `Drop` and consuming `shutdown_report`.
    pub(crate) fn wait_report_blocking(&self) -> LocalSystemTerminalReport {
        let mut state = self.lock_state();
        loop {
            if let Some(report) = &state.report {
                return report.clone();
            }
            if state.joiner_failed {
                return failed_report_from_state(&state, &self.remote_metrics);
            }
            state = self
                .condvar
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn request_shutdown_bounded(self: &Arc<Self>) -> Result<(), ShutdownRequestError> {
        let mut state = self.lock_state();
        if state.shutdown_requested {
            return Ok(());
        }
        let multi = self.is_multi_shard;
        // Walk unsignaled workers; mark each one as signaled the moment
        // its `Shutdown` command is admitted (or it is observed gone).
        // Bail on the first `Full` shard so the caller can retry later.
        for worker in state.workers.iter_mut() {
            if worker.signaled {
                continue;
            }
            match worker.commands.try_send(ThreadedCommand::Shutdown) {
                Ok(()) => {
                    worker.signaled = true;
                }
                Err(TrySendError::Full(_)) => {
                    return Err(ShutdownRequestError::CommandFull {
                        shard: if multi { Some(worker.shard) } else { None },
                    });
                }
                Err(TrySendError::Disconnected(_)) => {
                    worker.metrics.set_state(LiveShardState::Failed);
                    // The worker is already gone — equivalent to having
                    // received `Shutdown`. Mark signaled so a retry will
                    // not target it again and the joiner can run once
                    // every shard reaches this state.
                    worker.signaled = true;
                    return Err(ShutdownRequestError::WorkerStopped {
                        shard: if multi { Some(worker.shard) } else { None },
                    });
                }
            }
        }
        // Every worker is now signaled; commit the request.
        state.shutdown_requested = true;
        drop(state);
        self.ensure_joiner_started();
        Ok(())
    }

    fn wait_report_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<LocalSystemTerminalReport, ShutdownWaitError> {
        let mut state = self.lock_state();
        if let Some(report) = &state.report {
            return Ok(report.clone());
        }
        if state.joiner_failed {
            return Err(ShutdownWaitError::WorkerStopped);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                if let Some(report) = &state.report {
                    return Ok(report.clone());
                }
                if state.joiner_failed {
                    return Err(ShutdownWaitError::WorkerStopped);
                }
                return Err(ShutdownWaitError::Timeout);
            }
            let remaining = deadline.saturating_duration_since(now);
            let (s, result) = self
                .condvar
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            state = s;
            if let Some(report) = &state.report {
                return Ok(report.clone());
            }
            if state.joiner_failed {
                return Err(ShutdownWaitError::WorkerStopped);
            }
            if result.timed_out() {
                return Err(ShutdownWaitError::Timeout);
            }
        }
    }

    /// Bounded owner/drop wait. Unlike [`Self::wait_report_blocking`],
    /// this returns if a wedged worker prevents the joiner from
    /// producing terminal truth within `timeout`.
    pub(crate) fn wait_report_for_owner_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<LocalSystemTerminalReport, ShutdownWaitError> {
        self.wait_report_with_timeout(timeout)
    }
}

// `SharedShutdownState` itself cannot implement `ShutdownInner` directly:
// `request_shutdown_bounded` and `ensure_joiner_started` need `&Arc<Self>`
// to spawn the joiner thread, and a `&self` cannot reconstruct that. The
// handle below wraps the `Arc` and dispatches through it.
pub(crate) struct ShutdownInnerHandle<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    pub(crate) shared: Arc<SharedShutdownState<S, F>>,
}

impl<S, F> ShutdownInner for ShutdownInnerHandle<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn request_shutdown(&self) -> Result<(), ShutdownRequestError> {
        self.shared.request_shutdown_bounded()
    }

    fn wait_report(
        &self,
        timeout: Duration,
    ) -> Result<LocalSystemTerminalReport, ShutdownWaitError> {
        self.shared.wait_report_with_timeout(timeout)
    }
}

/// Build a public [`ThreadedShutdownHandle`] from a shared state.
pub(crate) fn handle_for<S, F>(shared: &Arc<SharedShutdownState<S, F>>) -> ThreadedShutdownHandle
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    let inner: Arc<dyn ShutdownInner> = Arc::new(ShutdownInnerHandle {
        shared: Arc::clone(shared),
    });
    ThreadedShutdownHandle::new(inner)
}

#[cfg(test)]
thread_local! {
    /// Test seam: when set, [`SharedShutdownState::ensure_joiner_started`]
    /// skips the real `thread::Builder::spawn` and takes the inline fallback,
    /// letting a test drive the spawn-failure path deterministically.
    static FORCE_JOINER_SPAWN_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn force_joiner_spawn_failure() -> bool {
    FORCE_JOINER_SPAWN_FAILURE.with(std::cell::Cell::get)
}

#[cfg(not(test))]
#[inline]
fn force_joiner_spawn_failure() -> bool {
    false
}

/// Panic-safe joiner entry point. Catches any unwind inside
/// [`run_joiner`] so waiters surface
/// [`ShutdownWaitError::WorkerStopped`] instead of hanging on a
/// condvar that will never be notified.
fn joiner_main<S, F>(shared: Arc<SharedShutdownState<S, F>>)
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    let shared_for_guard = Arc::clone(&shared);
    let result = catch_unwind(AssertUnwindSafe(|| run_joiner(shared)));
    if result.is_err() {
        let mut state = shared_for_guard.lock_state();
        state.joiner_failed = true;
        state.joining = false;
        shared_for_guard.condvar.notify_all();
    }
}

/// Joiner thread body. Joins every worker, builds the terminal report,
/// caches it under the shared lock, and notifies every waiter.
fn run_joiner<S, F>(shared: Arc<SharedShutdownState<S, F>>)
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    // Take the worker handles under the lock, then release it before joining
    // (each `join()` blocks until its worker exits). Taking the handles here
    // — not in the caller — is what keeps the spawn-failure fallback correct:
    // whichever path actually runs the joiner finds the handles present.
    let joinable: Vec<(ShardId, ThreadedWorkerJoin, Arc<LiveShardMetrics>)> = {
        let mut state = shared.lock_state();
        let mut joinable = Vec::new();
        for worker in &mut state.workers {
            if let Some(h) = worker.handle.take() {
                joinable.push((worker.shard, h, Arc::clone(&worker.metrics)));
            }
        }
        joinable
    };
    let mut events: Vec<crate::trace::RuntimeEvent> = Vec::new();
    let mut failure: Option<ThreadedRuntimeError> = None;
    for (_shard, handle, metrics) in joinable {
        match handle.join() {
            Ok(exit) => merge_exit(exit, &metrics, &mut events, &mut failure),
            Err(_) => {
                metrics.set_state(LiveShardState::Failed);
                if failure.is_none() {
                    failure = Some(ThreadedRuntimeError::WorkerStopped);
                }
            }
        }
    }
    // Event ids are per-shard-local; group by shard, then local id. This is
    // a stable grouping, not a cross-shard temporal order — a free-running
    // multishard trace has no deterministic global event order.
    events.sort_by_key(|e| (e.shard(), e.id()));

    let topology = build_topology(&shared);
    let report = match failure {
        None => {
            LocalSystemTerminalReport::new_with_topology(LocalSystemState::Closed, events, topology)
        }
        Some(error) => {
            LocalSystemTerminalReport::failed_with_topology_and_trace(error, topology, events)
        }
    };

    let mut state = shared.lock_state();
    state.report = Some(report);
    state.joining = false;
    shared.condvar.notify_all();
}

fn merge_exit(
    exit: ThreadedWorkerExit,
    metrics: &Arc<LiveShardMetrics>,
    events: &mut Vec<crate::trace::RuntimeEvent>,
    failure: &mut Option<ThreadedRuntimeError>,
) {
    if let Some(error) = exit.error {
        metrics.set_state(LiveShardState::Failed);
        if failure.is_none() {
            *failure = Some(error);
        }
    } else {
        metrics.set_state(LiveShardState::Stopped);
    }
    events.extend(exit.trace);
}

fn build_topology<S, F>(shared: &SharedShutdownState<S, F>) -> LiveTopologyReport
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    let state = shared.lock_state();
    let shards: Vec<_> = state.workers.iter().map(|w| w.metrics.report()).collect();
    drop(state);
    if shared.is_multi_shard {
        let remote: Vec<LiveRemoteQueueReport> = shared
            .remote_metrics
            .iter()
            .map(|(&(source, target), metrics)| LiveRemoteQueueReport {
                source,
                target,
                queue: metrics.report(),
            })
            .collect();
        LiveTopologyReport::new(shards, remote)
    } else {
        LiveTopologyReport::single(
            shards
                .into_iter()
                .next()
                .expect("single-shard shutdown state has one worker"),
        )
    }
}

fn failed_report_from_state<S, F>(
    state: &ShutdownState<S, F>,
    remote_metrics: &BTreeMap<(ShardId, ShardId), Arc<LiveQueueMetrics>>,
) -> LocalSystemTerminalReport
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    let shards = state.workers.iter().map(|w| w.metrics.report()).collect();
    let remote: Vec<LiveRemoteQueueReport> = remote_metrics
        .iter()
        .map(|(&(source, target), metrics)| LiveRemoteQueueReport {
            source,
            target,
            queue: metrics.report(),
        })
        .collect();
    let topology = LiveTopologyReport::new(shards, remote);
    LocalSystemTerminalReport::failed_with_topology_and_trace(
        ThreadedRuntimeError::WorkerStopped,
        topology,
        Vec::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::DefaultMailboxFactory;
    use crate::threaded::ThreadedRuntimeConfig;
    use std::collections::BTreeMap;
    use std::sync::mpsc::sync_channel;

    struct TestShard;
    impl Shard for TestShard {
        fn id(&self) -> ShardId {
            ShardId::new(0)
        }
    }

    /// A worker entry whose join handle is a real thread that exits
    /// immediately with a clean exit. Joining it returns a
    /// `ThreadedWorkerExit` and flips the shard's metrics to `Stopped`;
    /// leaking the handle (not joining) leaves the metrics `RUNNING`.
    fn fake_worker(shard_id: u32) -> ShutdownWorker<TestShard, DefaultMailboxFactory> {
        let (commands, _rx) = sync_channel::<ThreadedCommand<TestShard, DefaultMailboxFactory>>(1);
        let commands = CommandSender::new(commands);
        let shard = ShardId::new(shard_id);
        let metrics = Arc::new(LiveShardMetrics::new(
            shard,
            None,
            ThreadedRuntimeConfig::default(),
        ));
        let handle = std::thread::spawn(|| ThreadedWorkerExit::clean(Vec::new()));
        ShutdownWorker {
            shard,
            commands,
            handle: Some(handle),
            metrics,
            signaled: true,
        }
    }

    // E1: when the joiner thread cannot be spawned, the inline fallback must
    // still join every worker and cache a real report. The pre-fix code took
    // the handles before the spawn, dropped them with the un-spawned closure,
    // then re-took already-empty handles inline — leaking every thread and
    // caching an empty `Closed` report that lied.
    #[test]
    fn joiner_spawn_failure_joins_inline_without_leaking_or_lying() {
        let workers = vec![fake_worker(1), fake_worker(2)];
        let shared = Arc::new(SharedShutdownState::multi_shard(workers, BTreeMap::new()));

        FORCE_JOINER_SPAWN_FAILURE.with(|f| f.set(true));
        shared.ensure_joiner_started();
        FORCE_JOINER_SPAWN_FAILURE.with(|f| f.set(false));

        let state = shared.lock_state();
        assert!(
            state.workers.iter().all(|w| w.handle.is_none()),
            "spawn-failure fallback must take every worker handle"
        );
        assert!(
            state
                .workers
                .iter()
                .all(|w| w.metrics.state() == LiveShardState::Stopped),
            "every worker must actually be joined (Stopped), not leaked (RUNNING)"
        );
        let report = state.report.as_ref().expect("terminal report cached");
        assert_eq!(report.state(), LocalSystemState::Closed);
    }
}
