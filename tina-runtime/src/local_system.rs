//! Local system app owners extracted from lib.rs.
//!
//! Houses `LocalSystem`, `LocalMultiShardSystem`, their builders/shutdown
//! handles, the bounded-shape `LocalSystemConfig`, terminal report types,
//! and the threaded-worker exit/type-alias helpers consumed by
//! `ThreadedRuntime`.

use std::alloc::Global;
use std::fmt;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use betelgeuse::IOLoopHandle;
use tina::{Address, Isolate, Outbound as TinaOutbound, Shard, ShardId};
use tina_supervisor::SupervisorConfig;

use crate::call::IntoErasedCall;
use crate::capabilities::RuntimeCapabilities;
use crate::driver;
use crate::errors::{
    ShutdownAndWaitError, StartupError, ThreadedRuntimeError, ThreadedSendObservedError,
    ThreadedTrySendError,
};
use crate::live_report::{LiveShardReport, LiveShardState, LiveTopologyReport};
use crate::mailbox::MailboxFactory;
use crate::observer::TraceObserver;
use crate::trace::{RuntimeEvent, RuntimeEventKind};
use crate::{
    DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT, HostBurstOutcomes, IntoErasedSpawn,
    IntoErasedSpawnObserved, IntoSendErasedSpawnObserved, PreallocationConfig, Runtime,
    SendObservedUntilError, ThreadedMultiShardRuntime, ThreadedRuntime, ThreadedRuntimeConfig,
    TraceRetention,
};

/// Preferred public bounded-shape config for local Tina systems.
///
/// `ThreadedRuntimeConfig` remains the lower-level worker config. This type is
/// the user-facing manifest: every bounded live resource family is either
/// configurable here or named as a fixed capability in [`RuntimeCapabilities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalSystemConfig {
    /// Address provenance for this local owner. `None` allocates a fresh
    /// nonzero incarnation at startup.
    pub system_incarnation: Option<tina::SystemIncarnation>,
    /// Capacity of the bounded control/ingress queue feeding each shard worker.
    pub ingress_capacity: usize,
    /// Capacity of each local source-shard -> destination-shard transport.
    pub shard_pair_capacity: usize,
    /// Maximum remote envelopes one worker harvests before giving local work a
    /// turn.
    pub remote_inbound_drain_budget: usize,
    /// Capacity of the bounded storage lane used for local filesystem and
    /// persistence work.
    pub storage_lane_capacity: usize,
    /// Capacity of the bounded DNS lane.
    pub dns_lane_capacity: usize,
    /// Capacity of the bounded TLS lane: the cap on in-flight TLS
    /// operations. TLS work rides the shared substrate loop, not
    /// per-operation worker threads.
    pub tls_lane_capacity: usize,
    /// Capacity of the bounded process lane.
    pub process_lane_capacity: usize,
    /// Capacity of runtime-owned signal waits.
    pub signal_capacity: usize,
    /// Max concurrently armed runtime timers per shard. A full timer lane
    /// refuses new sleeps with [`crate::CallError::TimerFull`] instead of
    /// growing without bound.
    pub timer_capacity: usize,
    /// OS CPU id to hard-pin shard workers to. `Some(n)` pins on platforms that
    /// can (Linux), reports [`crate::AffinityStatus::Unsupported`] elsewhere,
    /// and [`crate::AffinityStatus::Failed`] for a core outside the process's
    /// allowed affinity mask. Multi-shard local systems treat this as the core
    /// for the first shard in stable order and assign later shards to
    /// contiguous OS CPU ids (`n + ordinal`).
    pub configured_core: Option<usize>,
    /// Setup-time reserves for runtime-owned metadata.
    pub preallocation: PreallocationConfig,
    /// Trace retention for worker-owned runtimes. Defaults to a bounded ring
    /// ([`DEFAULT_LIVE_TRACE_RETENTION`](crate::DEFAULT_LIVE_TRACE_RETENTION));
    /// set [`TraceRetention::Full`] for replay/debug that needs every event.
    pub trace_retention: TraceRetention,
    /// How long an idle worker may park before checking runtime-owned work.
    pub idle_wait: Duration,
    /// Per-shard budget for draining lane workers during shutdown after
    /// cancellation. Default is
    /// [`DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT`]. When the budget elapses,
    /// shutdown returns even if some lane work could not finish; the
    /// terminal report names the remaining work.
    pub shutdown_lane_drain_timeout: Duration,
}

impl Default for LocalSystemConfig {
    fn default() -> Self {
        Self {
            system_incarnation: None,
            ingress_capacity: ThreadedRuntimeConfig::default().command_capacity,
            shard_pair_capacity: ThreadedRuntimeConfig::default().command_capacity,
            remote_inbound_drain_budget: ThreadedRuntimeConfig::default()
                .remote_inbound_drain_budget,
            storage_lane_capacity: driver::DEFAULT_STORAGE_LANE_CAPACITY,
            dns_lane_capacity: driver::DEFAULT_DNS_LANE_CAPACITY,
            tls_lane_capacity: driver::DEFAULT_TLS_LANE_CAPACITY,
            process_lane_capacity: driver::DEFAULT_PROCESS_LANE_CAPACITY,
            signal_capacity: driver::DEFAULT_SIGNAL_CAPACITY,
            timer_capacity: driver::DEFAULT_DRIVER_TIMER_CAPACITY,
            configured_core: None,
            preallocation: PreallocationConfig::default(),
            // Live worker: bounded ring so trace does not grow with uptime.
            // Replay/sim/tests set `TraceRetention::Full` explicitly.
            trace_retention: TraceRetention::Bounded(crate::DEFAULT_LIVE_TRACE_RETENTION),
            idle_wait: Duration::from_millis(1),
            shutdown_lane_drain_timeout: DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT,
        }
    }
}

impl LocalSystemConfig {
    /// Validates that no bounded resource silently starts with zero capacity.
    pub fn validate(&self) -> Result<(), LocalSystemConfigError> {
        if self
            .system_incarnation
            .is_some_and(tina::SystemIncarnation::is_unscoped)
        {
            return Err(LocalSystemConfigError::UnscopedSystemIncarnation);
        }
        if self.ingress_capacity == 0 {
            return Err(LocalSystemConfigError::ZeroIngressCapacity);
        }
        if self.shard_pair_capacity == 0 {
            return Err(LocalSystemConfigError::ZeroShardPairCapacity);
        }
        if self.remote_inbound_drain_budget == 0 {
            return Err(LocalSystemConfigError::ZeroRemoteInboundDrainBudget);
        }
        if self.storage_lane_capacity == 0 {
            return Err(LocalSystemConfigError::ZeroStorageLaneCapacity);
        }
        if self.dns_lane_capacity == 0 {
            return Err(LocalSystemConfigError::ZeroDnsLaneCapacity);
        }
        if self.tls_lane_capacity == 0 {
            return Err(LocalSystemConfigError::ZeroTlsLaneCapacity);
        }
        if self.process_lane_capacity == 0 {
            return Err(LocalSystemConfigError::ZeroProcessLaneCapacity);
        }
        if self.signal_capacity == 0 {
            return Err(LocalSystemConfigError::ZeroSignalCapacity);
        }
        if self.timer_capacity == 0 {
            return Err(LocalSystemConfigError::ZeroTimerCapacity);
        }
        Ok(())
    }

    fn threaded_runtime_config(self) -> ThreadedRuntimeConfig {
        ThreadedRuntimeConfig {
            system_incarnation: self.system_incarnation,
            command_capacity: self.ingress_capacity,
            shard_pair_capacity: self.shard_pair_capacity,
            remote_inbound_drain_budget: self.remote_inbound_drain_budget,
            storage_lane_capacity: self.storage_lane_capacity,
            dns_lane_capacity: self.dns_lane_capacity,
            tls_lane_capacity: self.tls_lane_capacity,
            process_lane_capacity: self.process_lane_capacity,
            signal_capacity: self.signal_capacity,
            timer_capacity: self.timer_capacity,
            configured_core: self.configured_core,
            preallocation: self.preallocation,
            trace_retention: self.trace_retention,
            idle_wait: self.idle_wait,
            shutdown_lane_drain_timeout: self.shutdown_lane_drain_timeout,
            // Hot-drain bounds + idle re-poll are not part of the local-system
            // builder surface yet; take the behaviour-preserving defaults.
            ..ThreadedRuntimeConfig::default()
        }
    }
}

/// Invalid local system bounded-shape config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSystemConfigError {
    /// A live owner cannot use the zero marker reserved for manual addresses.
    UnscopedSystemIncarnation,
    /// Ingress capacity must be greater than zero.
    ZeroIngressCapacity,
    /// Shard-pair capacity must be greater than zero.
    ZeroShardPairCapacity,
    /// Remote inbound drain budget must be greater than zero.
    ZeroRemoteInboundDrainBudget,
    /// Storage-lane capacity must be greater than zero.
    ZeroStorageLaneCapacity,
    /// DNS-lane capacity must be greater than zero.
    ZeroDnsLaneCapacity,
    /// TLS-lane capacity must be greater than zero.
    ZeroTlsLaneCapacity,
    /// Process-lane capacity must be greater than zero.
    ZeroProcessLaneCapacity,
    /// Signal capacity must be greater than zero.
    ZeroSignalCapacity,
    /// Timer capacity must be greater than zero.
    ZeroTimerCapacity,
}

impl fmt::Display for LocalSystemConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let field = match self {
            Self::UnscopedSystemIncarnation => {
                return write!(f, "system_incarnation must be nonzero");
            }
            Self::ZeroIngressCapacity => "ingress_capacity",
            Self::ZeroShardPairCapacity => "shard_pair_capacity",
            Self::ZeroRemoteInboundDrainBudget => "remote_inbound_drain_budget",
            Self::ZeroStorageLaneCapacity => "storage_lane_capacity",
            Self::ZeroDnsLaneCapacity => "dns_lane_capacity",
            Self::ZeroTlsLaneCapacity => "tls_lane_capacity",
            Self::ZeroProcessLaneCapacity => "process_lane_capacity",
            Self::ZeroSignalCapacity => "signal_capacity",
            Self::ZeroTimerCapacity => "timer_capacity",
        };
        write!(f, "{field} must be greater than zero")
    }
}

impl std::error::Error for LocalSystemConfigError {}

pub(crate) type ThreadedCommandFn<S, F> = Box<dyn FnOnce(&mut Runtime<S, F>) + Send>;
pub(crate) type ThreadedIoLoopFactory =
    Box<dyn FnOnce() -> std::io::Result<IOLoopHandle<Global>> + Send>;
pub(crate) type ThreadedWorkerJoin = JoinHandle<ThreadedWorkerExit>;

pub(crate) struct ThreadedWorkerExit {
    pub(crate) trace: Vec<RuntimeEvent>,
    pub(crate) error: Option<ThreadedRuntimeError>,
}

impl ThreadedWorkerExit {
    pub(crate) fn clean(trace: Vec<RuntimeEvent>) -> Self {
        Self { trace, error: None }
    }

    pub(crate) fn failed(error: ThreadedRuntimeError, trace: Vec<RuntimeEvent>) -> Self {
        Self {
            trace,
            error: Some(error),
        }
    }
}

/// Lifecycle state for the canonical local Tina app owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSystemState {
    /// Reserved state for app owners that expose asynchronous startup.
    Starting,
    /// The app owner has a live worker handle and accepts bounded ingress.
    Accepting,
    /// Reserved state for app owners that expose an observable shutdown start.
    Closing,
    /// Reserved state for app owners that expose an observable drain window.
    Draining,
    /// The app has closed cleanly.
    Closed,
    /// The app failed during shutdown or worker execution.
    Failed,
}

/// Terminal report returned by [`LocalSystem`] and [`LocalMultiShardSystem`] shutdown.
///
/// `Clone` is implemented so a cached terminal report can be returned
/// independently to every [`crate::ThreadedShutdownHandle::wait_report`] waiter
/// without contention.
#[derive(Debug, Clone)]
pub struct LocalSystemTerminalReport {
    state: LocalSystemState,
    trace: Vec<RuntimeEvent>,
    error: Option<ThreadedRuntimeError>,
    topology: Option<LiveTopologyReport>,
    shutdown: LocalSystemShutdownReport,
}

/// Why a shutdown is unclean.
///
/// Multiple reasons may apply to one shutdown — for example, a runtime
/// error plus remaining worker-held resources. The terminal report
/// surfaces the full set in priority order via
/// [`LocalSystemShutdownReport::unclean_reasons`]:
/// runtime error > failed shard > final state not Closed >
/// remaining worker-held > remaining pending call > remaining table-owned.
/// [`unclean_reason`](LocalSystemShutdownReport::unclean_reason) keeps
/// the convenience single-reason accessor for callers that only need the
/// most significant cause.
///
/// Not exhaustive: future variants may be added; pattern matches should
/// handle the catchall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShutdownUncleanReason {
    /// A worker thread or driver returned a terminal error before clean
    /// shutdown completed. The wrapped value names the underlying error.
    RuntimeError(ThreadedRuntimeError),
    /// At least one shard ended in [`LiveShardState::Failed`].
    FailedShards,
    /// The system did not reach [`LocalSystemState::Closed`] before the
    /// terminal report was produced.
    NotClosed,
    /// One or more lanes still hold worker-side OS handles or child
    /// processes after the shutdown drain finished.
    WorkerHeldResourcesRemaining,
    /// One or more pending driver calls were still outstanding after the
    /// shutdown drain finished.
    PendingDriverCallsRemaining,
    /// One or more table-owned runtime resources were still alive after
    /// shutdown.
    OwnedResourcesRemaining,
}

/// Terminal shutdown accounting for a local Tina system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSystemShutdownReport {
    final_state: LocalSystemState,
    clean: bool,
    canceled_count: usize,
    tombstoned_count: usize,
    rejected_after_drain_count: usize,
    failed_shards: Vec<ShardId>,
    remaining_owned_resource_count: usize,
    remaining_worker_held_resource_count: usize,
    remaining_pending_driver_call_count: usize,
    unclean_reasons: Vec<ShutdownUncleanReason>,
}

/// Typed failure returned when a terminal report proves shutdown was unclean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncleanShutdownError {
    report: LocalSystemShutdownReport,
}

impl UncleanShutdownError {
    /// Full terminal shutdown accounting that made the clean check fail.
    pub const fn report(&self) -> &LocalSystemShutdownReport {
        &self.report
    }
}

impl fmt::Display for UncleanShutdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unclean shutdown: final_state={:?} reasons={:?} failed_shards={:?} \
             remaining_owned={} remaining_worker_held={} remaining_pending_driver_calls={} \
             canceled={} tombstoned={} rejected_after_drain={}",
            self.report.final_state(),
            self.report.unclean_reasons(),
            self.report.failed_shards(),
            self.report.remaining_owned_resource_count(),
            self.report.remaining_worker_held_resource_count(),
            self.report.remaining_pending_driver_call_count(),
            self.report.canceled_count(),
            self.report.tombstoned_count(),
            self.report.rejected_after_drain_count(),
        )
    }
}

impl std::error::Error for UncleanShutdownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.report.unclean_reasons().first() {
            Some(ShutdownUncleanReason::RuntimeError(error)) => Some(error),
            _ => None,
        }
    }
}

/// Failure while consuming a local system through bounded terminal observation.
///
/// The bound applies to shutdown admission and terminal-report observation.
/// On timeout, the consumed owner does not start a second blocking shutdown
/// attempt; an admitted background joiner or escaped shutdown handle may still
/// observe terminal truth later. After an admission timeout, an escaped handle
/// retains shutdown control and must retry or be dropped. Without one, owner
/// consumption disconnects the remaining control senders; it does not claim
/// terminal truth was observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalShutdownError {
    /// Shutdown admission or terminal-report observation exceeded its budget,
    /// or the shutdown joiner stopped before producing terminal truth.
    Observation(ShutdownAndWaitError),
    /// Terminal truth was observed, but it proved shutdown was not clean.
    Unclean(UncleanShutdownError),
}

impl fmt::Display for TerminalShutdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Observation(error) => write!(f, "failed to observe terminal shutdown: {error}"),
            Self::Unclean(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for TerminalShutdownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Observation(error) => Some(error),
            Self::Unclean(error) => Some(error),
        }
    }
}

/// Sized error adapter for workload report types that expose a standard error.
///
/// Some report containers, including `anyhow::Error`, intentionally do not
/// implement [`std::error::Error`] themselves but do implement
/// `AsRef<dyn Error + Send + Sync>`. This adapter preserves the owned report
/// while making its referenced error available through [`std::error::Error::source`].
#[derive(Debug)]
pub struct ReportedWorkloadError<E>(E);

impl<E> ReportedWorkloadError<E> {
    /// Wraps one owned workload report without formatting or downcasting it.
    pub const fn new(report: E) -> Self {
        Self(report)
    }

    /// Borrows the original workload report.
    pub const fn get_ref(&self) -> &E {
        &self.0
    }

    /// Recovers the original workload report.
    pub fn into_inner(self) -> E {
        self.0
    }
}

impl<E: fmt::Display> fmt::Display for ReportedWorkloadError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<E> std::error::Error for ReportedWorkloadError<E>
where
    E: fmt::Debug
        + fmt::Display
        + AsRef<dyn std::error::Error + Send + Sync + 'static>
        + Send
        + Sync
        + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// Typed result of a fallible workload followed by guaranteed local-system shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunToShutdownError<E> {
    /// The workload failed, while shutdown completed cleanly.
    Workload(E),
    /// The workload succeeded, but shutdown failed.
    Shutdown(TerminalShutdownError),
    /// Both the workload and shutdown failed. Neither failure is erased.
    WorkloadAndShutdown {
        /// Original workload failure.
        workload: E,
        /// Independent terminal shutdown failure.
        shutdown: TerminalShutdownError,
    },
}

impl<E> RunToShutdownError<E> {
    /// Returns the workload failure when one occurred.
    pub const fn workload(&self) -> Option<&E> {
        match self {
            Self::Workload(error)
            | Self::WorkloadAndShutdown {
                workload: error, ..
            } => Some(error),
            Self::Shutdown(_) => None,
        }
    }

    /// Returns the terminal shutdown failure when one occurred.
    pub const fn shutdown(&self) -> Option<&TerminalShutdownError> {
        match self {
            Self::Shutdown(error)
            | Self::WorkloadAndShutdown {
                shutdown: error, ..
            } => Some(error),
            Self::Workload(_) => None,
        }
    }
}

impl<E: fmt::Display> fmt::Display for RunToShutdownError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workload(error) => write!(f, "workload failed: {error}"),
            Self::Shutdown(error) => write!(f, "shutdown failed: {error}"),
            Self::WorkloadAndShutdown { workload, shutdown } => {
                write!(
                    f,
                    "workload failed: {workload}; shutdown also failed: {shutdown}"
                )
            }
        }
    }
}

impl<E> std::error::Error for RunToShutdownError<E>
where
    E: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Workload(error)
            | Self::WorkloadAndShutdown {
                workload: error, ..
            } => Some(error),
            Self::Shutdown(error) => Some(error),
        }
    }
}

fn finish_run_to_shutdown<T, E>(
    workload: Result<T, E>,
    terminal: Result<LocalSystemTerminalReport, ShutdownAndWaitError>,
) -> Result<T, RunToShutdownError<E>> {
    let shutdown = terminal
        .map_err(TerminalShutdownError::Observation)
        .and_then(|report| {
            report
                .ensure_clean()
                .map_err(TerminalShutdownError::Unclean)
        });
    match (workload, shutdown) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(RunToShutdownError::Workload(error)),
        (Ok(_), Err(error)) => Err(RunToShutdownError::Shutdown(error)),
        (Err(workload), Err(shutdown)) => {
            Err(RunToShutdownError::WorkloadAndShutdown { workload, shutdown })
        }
    }
}

impl LocalSystemShutdownReport {
    pub(crate) fn from_parts(
        state: LocalSystemState,
        trace: &[RuntimeEvent],
        error: Option<ThreadedRuntimeError>,
        topology: Option<&LiveTopologyReport>,
    ) -> Self {
        let summary = terminal_summary(trace);
        let failed_shards: Vec<ShardId> = topology
            .map(|topology| {
                topology
                    .shards()
                    .iter()
                    .filter(|shard| shard.state() == LiveShardState::Failed)
                    .map(LiveShardReport::shard)
                    .collect()
            })
            .unwrap_or_default();
        let remaining_owned_resource_count = topology
            .map(|topology| {
                topology
                    .shards()
                    .iter()
                    .map(LiveShardReport::owned_resource_count)
                    .sum()
            })
            .unwrap_or_default();
        let remaining_worker_held_resource_count = topology
            .map(|topology| {
                topology
                    .shards()
                    .iter()
                    .map(LiveShardReport::worker_held_resource_count)
                    .sum()
            })
            .unwrap_or_default();
        let remaining_pending_driver_call_count = topology
            .map(|topology| {
                topology
                    .shards()
                    .iter()
                    .map(LiveShardReport::pending_driver_call_count)
                    .sum()
            })
            .unwrap_or_default();
        // Collect every applicable unclean reason in priority order so
        // callers can see the full picture, not just the first cause.
        let mut unclean_reasons = Vec::new();
        if let Some(error) = error {
            unclean_reasons.push(ShutdownUncleanReason::RuntimeError(error));
        }
        if !failed_shards.is_empty() {
            unclean_reasons.push(ShutdownUncleanReason::FailedShards);
        }
        if state != LocalSystemState::Closed {
            unclean_reasons.push(ShutdownUncleanReason::NotClosed);
        }
        if remaining_worker_held_resource_count > 0 {
            unclean_reasons.push(ShutdownUncleanReason::WorkerHeldResourcesRemaining);
        }
        if remaining_pending_driver_call_count > 0 {
            unclean_reasons.push(ShutdownUncleanReason::PendingDriverCallsRemaining);
        }
        if remaining_owned_resource_count > 0 {
            unclean_reasons.push(ShutdownUncleanReason::OwnedResourcesRemaining);
        }
        Self {
            final_state: state,
            clean: unclean_reasons.is_empty(),
            canceled_count: summary.call_completion_rejected,
            tombstoned_count: summary.call_reply_rejected,
            rejected_after_drain_count: summary.send_rejected,
            failed_shards,
            remaining_owned_resource_count,
            remaining_worker_held_resource_count,
            remaining_pending_driver_call_count,
            unclean_reasons,
        }
    }

    /// Final lifecycle state.
    pub const fn final_state(&self) -> LocalSystemState {
        self.final_state
    }

    /// Whether shutdown completed cleanly before any terminal failure.
    pub const fn clean(&self) -> bool {
        self.clean
    }

    /// Count of canceled completion deliveries visible in the terminal trace.
    pub const fn canceled_count(&self) -> usize {
        self.canceled_count
    }

    /// Count of tombstoned late replies visible in the terminal trace.
    pub const fn tombstoned_count(&self) -> usize {
        self.tombstoned_count
    }

    /// Count of send rejections visible after shutdown/drain accounting.
    pub const fn rejected_after_drain_count(&self) -> usize {
        self.rejected_after_drain_count
    }

    /// Failed shard ids named by the final topology snapshot.
    pub fn failed_shards(&self) -> &[ShardId] {
        &self.failed_shards
    }

    /// Remaining table-owned runtime resources at terminal report time.
    pub const fn remaining_owned_resource_count(&self) -> usize {
        self.remaining_owned_resource_count
    }

    /// Remaining worker-held resources (TLS in-flight clones, live process
    /// children) at terminal report time.
    pub const fn remaining_worker_held_resource_count(&self) -> usize {
        self.remaining_worker_held_resource_count
    }

    /// Remaining pending driver calls at terminal report time.
    pub const fn remaining_pending_driver_call_count(&self) -> usize {
        self.remaining_pending_driver_call_count
    }

    /// Highest-priority typed reason this shutdown is unclean.
    ///
    /// `None` iff [`clean`](Self::clean) is `true`. Use
    /// [`unclean_reasons`](Self::unclean_reasons) for the full list.
    /// See [`ShutdownUncleanReason`] for priority ordering.
    pub fn unclean_reason(&self) -> Option<ShutdownUncleanReason> {
        self.unclean_reasons.first().copied()
    }

    /// Every applicable unclean-shutdown reason in priority order.
    ///
    /// Empty iff [`clean`](Self::clean) is `true`. A shutdown can be
    /// unclean for several reasons at once (for example, a runtime error
    /// plus stuck worker-held resources); this list shows every one.
    pub fn unclean_reasons(&self) -> &[ShutdownUncleanReason] {
        &self.unclean_reasons
    }
}

/// Counted terminal work visible in a [`LocalSystemTerminalReport`] trace.
///
/// This is accounting from Tina's public trace, not a hidden runtime metrics
/// channel. It is meant to answer the shutdown question: what completed,
/// failed, was rejected, or was abandoned in the work Tina can see?
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalSystemTerminalSummary {
    /// Successful runtime-owned call completions delivered to requesters.
    pub call_completed: usize,
    /// Runtime-owned calls that produced typed failure outcomes.
    pub call_failed: usize,
    /// Runtime-owned completions that could not be delivered.
    pub call_completion_rejected: usize,
    /// Late or stale isolate-call replies rejected by the runtime.
    pub call_reply_rejected: usize,
    /// Sends rejected by mailbox, address, or transport pressure.
    pub send_rejected: usize,
    /// Buffered messages abandoned because an isolate stopped.
    pub message_abandoned: usize,
    /// Successful journal append trace events.
    pub journal_appended: usize,
    /// Failed persistence trace events.
    pub persistence_failed: usize,
    /// Finished recovery trace events.
    pub recovery_finished: usize,
    /// Failed recovery trace events.
    pub recovery_failed: usize,
}

/// Trace events plus whether every live shard could report them.
///
/// This is the default live trace shape because observability should keep
/// working after the thing being observed breaks. Use
/// [`complete_events`](Self::complete_events) when code needs proof that no
/// shard was missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceSnapshot {
    events: Vec<RuntimeEvent>,
    missing_shards: Vec<ShardId>,
    dropped_events: u64,
}

impl TraceSnapshot {
    pub(crate) fn complete(events: Vec<RuntimeEvent>) -> Self {
        Self {
            events,
            missing_shards: Vec::new(),
            dropped_events: 0,
        }
    }

    pub(crate) fn partial(events: Vec<RuntimeEvent>, missing_shards: Vec<ShardId>) -> Self {
        Self {
            events,
            missing_shards,
            dropped_events: 0,
        }
    }

    pub(crate) fn retained_suffix(events: Vec<RuntimeEvent>, dropped_events: u64) -> Self {
        Self {
            events,
            missing_shards: Vec::new(),
            dropped_events,
        }
    }

    /// Retained trace events that could still be collected.
    pub fn events(&self) -> &[RuntimeEvent] {
        &self.events
    }

    /// Whether every shard reported trace successfully and no retained prefix
    /// was dropped by trace retention.
    pub fn is_complete(&self) -> bool {
        self.missing_shards.is_empty() && self.dropped_events == 0
    }

    /// Whether at least one shard could not report trace, or retention already
    /// dropped events.
    pub fn is_partial(&self) -> bool {
        !self.is_complete()
    }

    /// Shards that could not report trace.
    pub fn missing_shards(&self) -> &[ShardId] {
        &self.missing_shards
    }

    /// Number of events dropped by retention before this retained suffix.
    pub const fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    /// Returns complete trace events, or a typed error if any shard was missing
    /// or retention already dropped a prefix.
    pub fn complete_events(self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        if !self.missing_shards.is_empty() {
            return Err(ThreadedRuntimeError::WorkerStopped);
        }
        if self.dropped_events != 0 {
            return Err(ThreadedRuntimeError::WorkerStopped);
        }
        Ok(self.events)
    }

    /// Consumes the snapshot and returns whatever events could be collected.
    pub fn into_events(self) -> Vec<RuntimeEvent> {
        self.events
    }
}

impl LocalSystemTerminalReport {
    /// Creates a terminal report from final state and trace.
    pub fn new(state: LocalSystemState, trace: Vec<RuntimeEvent>) -> Self {
        let shutdown = LocalSystemShutdownReport::from_parts(state, &trace, None, None);
        Self {
            state,
            trace,
            error: None,
            topology: None,
            shutdown,
        }
    }

    /// Creates a failed terminal report.
    pub fn failed(error: ThreadedRuntimeError) -> Self {
        let shutdown =
            LocalSystemShutdownReport::from_parts(LocalSystemState::Failed, &[], Some(error), None);
        Self {
            state: LocalSystemState::Failed,
            trace: Vec::new(),
            error: Some(error),
            topology: None,
            shutdown,
        }
    }

    /// Creates a terminal report with the final live topology snapshot.
    pub fn new_with_topology(
        state: LocalSystemState,
        trace: Vec<RuntimeEvent>,
        topology: LiveTopologyReport,
    ) -> Self {
        let shutdown = LocalSystemShutdownReport::from_parts(state, &trace, None, Some(&topology));
        Self {
            state,
            trace,
            error: None,
            topology: Some(topology),
            shutdown,
        }
    }

    /// Creates a failed terminal report with the final live topology snapshot.
    pub fn failed_with_topology(error: ThreadedRuntimeError, topology: LiveTopologyReport) -> Self {
        Self::failed_with_topology_and_trace(error, topology, Vec::new())
    }

    /// Creates a failed terminal report with topology and trace collected from
    /// workers that could still report.
    pub fn failed_with_topology_and_trace(
        error: ThreadedRuntimeError,
        topology: LiveTopologyReport,
        trace: Vec<RuntimeEvent>,
    ) -> Self {
        let shutdown = LocalSystemShutdownReport::from_parts(
            LocalSystemState::Failed,
            &trace,
            Some(error),
            Some(&topology),
        );
        Self {
            state: LocalSystemState::Failed,
            trace,
            error: Some(error),
            topology: Some(topology),
            shutdown,
        }
    }

    /// Final lifecycle state.
    pub const fn state(&self) -> LocalSystemState {
        self.state
    }

    /// Final trace returned by the live worker.
    pub fn trace(&self) -> &[RuntimeEvent] {
        &self.trace
    }

    /// Summarizes terminal work visible in the final trace.
    pub fn summary(&self) -> LocalSystemTerminalSummary {
        terminal_summary(&self.trace)
    }

    /// Terminal failure, if shutdown or worker execution failed.
    pub const fn error(&self) -> Option<ThreadedRuntimeError> {
        self.error
    }

    /// Final topology snapshot if the owner could still report it.
    pub fn topology(&self) -> Option<&LiveTopologyReport> {
        self.topology.as_ref()
    }

    /// Terminal shutdown accounting.
    pub const fn shutdown_report(&self) -> &LocalSystemShutdownReport {
        &self.shutdown
    }

    /// Confirms that terminal shutdown accounting is clean.
    ///
    /// A successfully observed terminal report can still describe a failed or
    /// resource-leaking shutdown. This check keeps transport/wait success
    /// distinct from lifecycle truth and preserves the full typed accounting
    /// in [`UncleanShutdownError`].
    pub fn ensure_clean(&self) -> Result<(), UncleanShutdownError> {
        if self.shutdown.clean() {
            Ok(())
        } else {
            Err(UncleanShutdownError {
                report: self.shutdown.clone(),
            })
        }
    }

    /// Consumes the report and returns the final trace.
    pub fn into_trace(self) -> Vec<RuntimeEvent> {
        self.trace
    }
}

fn terminal_summary(trace: &[RuntimeEvent]) -> LocalSystemTerminalSummary {
    let mut summary = LocalSystemTerminalSummary::default();
    for event in trace {
        match event.kind() {
            RuntimeEventKind::CallCompleted { .. } => summary.call_completed += 1,
            RuntimeEventKind::CallFailed { .. } => summary.call_failed += 1,
            RuntimeEventKind::CallCompletionRejected { .. } => {
                summary.call_completion_rejected += 1;
            }
            RuntimeEventKind::CallReplyRejected { .. } => summary.call_reply_rejected += 1,
            RuntimeEventKind::SendRejected { .. } => summary.send_rejected += 1,
            RuntimeEventKind::MessageAbandoned => summary.message_abandoned += 1,
            RuntimeEventKind::JournalAppended { .. } => summary.journal_appended += 1,
            RuntimeEventKind::SnapshotCommitFailed { .. }
            | RuntimeEventKind::JournalAppendFailed { .. } => {
                summary.persistence_failed += 1;
            }
            RuntimeEventKind::RecoveryFinished => summary.recovery_finished += 1,
            RuntimeEventKind::RecoveryFailed { .. } => summary.recovery_failed += 1,
            _ => {}
        }
    }
    summary
}

/// Canonical live app owner for one local Tina shard.
///
/// `LocalSystem` is the preferred user-facing owner for local live services.
/// [`ThreadedRuntime`] remains the lower-level backend-honest runner
/// underneath it.
pub struct LocalSystem<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    runtime: Option<ThreadedRuntime<S, F>>,
}

impl<S, F> LocalSystem<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Returns the provenance stamped into addresses issued by this owner.
    pub fn system_incarnation(&self) -> tina::SystemIncarnation {
        self.runtime().system_incarnation()
    }

    /// Starts configuring one single-shard local app.
    pub fn single_shard(shard: S, mailbox_factory: F) -> LocalSystemSingleShardBuilder<S, F> {
        LocalSystemSingleShardBuilder {
            shard,
            mailbox_factory,
            config: LocalSystemConfig::default(),
            trace_observer: None,
        }
    }

    /// Starts configuring one multi-shard local app.
    pub fn multi_shard(mailbox_factory: F) -> LocalSystemMultiShardBuilder<S, F>
    where
        F: Clone,
    {
        LocalSystemMultiShardBuilder {
            shards: Vec::new(),
            mailbox_factory,
            config: LocalSystemConfig::default(),
            trace_observer: None,
        }
    }

    /// Returns the owner-local lifecycle state.
    ///
    /// This does not synchronously probe worker health. Operations such as
    /// registration, sending, tracing, or shutdown report worker failure with a
    /// typed error.
    pub fn state(&self) -> LocalSystemState {
        if self.runtime.is_some() {
            LocalSystemState::Accepting
        } else {
            LocalSystemState::Closed
        }
    }

    /// Returns a live topology snapshot for this app.
    pub fn topology(&self) -> LiveTopologyReport {
        self.runtime().topology()
    }

    /// Returns the live runtime capability table for this app.
    pub fn capabilities(&self) -> RuntimeCapabilities {
        self.runtime().capabilities()
    }

    /// Registers one root isolate with a runtime-allocated mailbox.
    #[allow(private_bounds)]
    pub fn register_root<I, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        self.runtime()
            .register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
    }

    /// Registers one root isolate and atomically prefills its bootstrap message.
    ///
    /// The address is returned only after the bootstrap message has been
    /// admitted to the new bounded mailbox. The lower threaded owner preserves
    /// bootstrap authority on pre-admission failures and publishes no isolate
    /// entry when mailbox prefill is refused.
    #[allow(private_bounds, clippy::type_complexity)]
    pub fn register_root_with_bootstrap<I, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap: I::Message,
    ) -> Result<Address<I::Message, I::Reply>, crate::ThreadedRegisterBootstrapError<I::Message>>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: Send + 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        self.runtime()
            .register_with_capacity_and_bootstrap::<I, Outbound>(
                isolate,
                mailbox_capacity,
                bootstrap,
            )
    }

    /// Registers one root whose constructor receives its final typed address.
    /// The entry is not published until construction succeeds. Pre-admission
    /// failure drops the constructor without running it; an accepted
    /// `WorkerUnresponsive` constructor may still publish later.
    #[allow(private_bounds)]
    pub fn register_root_using<I, Outbound, Ctor>(
        &self,
        mailbox_capacity: usize,
        construct: Ctor,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
        Ctor: FnOnce(Address<I::Message, I::Reply>) -> I + Send + 'static,
    {
        self.runtime()
            .register_with_capacity_using::<I, Outbound, _>(mailbox_capacity, construct)
    }

    /// Registers one split event/request root service.
    #[allow(private_bounds)]
    pub fn register_split_service<I, Event, Request, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<crate::SplitServiceHandle<Event, Request, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<
                Shard = S,
                Message = tina::ServiceMessage<Event, Request>,
                Send = TinaOutbound<Outbound>,
            > + tina::CallableIsolate
            + Send
            + 'static,
        Event: 'static,
        Request: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        self.runtime()
            .register_split_service::<I, Event, Request, Outbound>(isolate, mailbox_capacity)
    }

    /// Registers one event-only root service.
    #[allow(private_bounds)]
    pub fn register_event_service<I, Event, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<crate::EventServiceHandle<Event>, ThreadedRuntimeError>
    where
        I: Isolate<
                Shard = S,
                Message = tina::ServiceMessage<Event, std::convert::Infallible>,
                Reply = (),
                Send = TinaOutbound<Outbound>,
            > + Send
            + 'static,
        Event: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        self.runtime()
            .register_event_service::<I, Event, Outbound>(isolate, mailbox_capacity)
    }

    /// Registers one request-only root service.
    #[allow(private_bounds)]
    pub fn register_request_service<I, Request, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<crate::RequestServiceHandle<Request, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<
                Shard = S,
                Message = tina::ServiceMessage<std::convert::Infallible, Request>,
                Send = TinaOutbound<Outbound>,
            > + tina::CallableIsolate
            + Send
            + 'static,
        Request: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        self.runtime()
            .register_request_service::<I, Request, Outbound>(isolate, mailbox_capacity)
    }

    /// Configures a registered root as a supervisor.
    pub fn supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedRuntimeError> {
        self.runtime().supervise(parent, config)
    }

    /// Configures a registered root as a supervisor without panicking when
    /// `parent` is unknown or stale.
    ///
    /// The nested result preserves the lower owner's distinction between a
    /// domain registration failure and a worker/control-plane failure.
    pub fn try_supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<Result<(), crate::SuperviseError>, ThreadedRuntimeError> {
        self.runtime().try_supervise(parent, config)
    }

    /// Attempts one bounded ingress handoff.
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        self.runtime().try_send(address, message)
    }

    /// Attempts bounded ingress through a service event capability.
    pub fn try_send_event<Event, Request>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        event: Event,
    ) -> Result<(), ThreadedTrySendError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
    {
        self.runtime().try_send_event(address, event)
    }

    /// Performs one typed isolate call from the host thread and returns its
    /// ordinary terminal [`crate::CallOutcome`].
    ///
    /// This is the host-call companion to [`Self::register_root`]. The timeout
    /// is the target call deadline; backend admission and worker failures are
    /// returned as [`ThreadedRuntimeError`].
    pub fn call_blocking<M, R>(
        &self,
        address: Address<M, R>,
        message: M,
        timeout: Duration,
    ) -> Result<crate::CallOutcome<R>, ThreadedRuntimeError>
    where
        M: Send + 'static,
        R: Send + 'static,
    {
        self.runtime().call_blocking(address, message, timeout)
    }

    /// Performs one blocking host call through a split-service request
    /// capability.
    ///
    /// The request is wrapped in the private service envelope by the runtime;
    /// callers retain the full [`crate::CallOutcome`] terminal vocabulary.
    pub fn call_blocking_request<Event, Request, Reply>(
        &self,
        address: tina::ServiceRequestAddress<Event, Request, Reply>,
        request: Request,
        timeout: Duration,
    ) -> Result<crate::CallOutcome<Reply>, ThreadedRuntimeError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
        Reply: Send + 'static,
    {
        self.runtime()
            .call_blocking_request(address, request, timeout)
    }

    /// Attempts one ingress send and observes the mailbox outcome.
    pub fn send_and_observe<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedSendObservedError> {
        self.runtime().send_and_observe(address, message)
    }

    /// Sends one typed service event and observes the exact mailbox outcome
    /// without exposing the private service envelope.
    pub fn send_event_and_observe<Event, Request>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        event: Event,
    ) -> Result<(), ThreadedSendObservedError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
    {
        self.runtime().send_event_and_observe(address, event)
    }

    /// Attempts one typed ingress send and records its eventual mailbox outcome.
    ///
    /// This is the [`LocalSystem`] facade for
    /// [`ThreadedRuntime::try_send_outcome`]. It preserves the lower-level
    /// ownership contract: `message` is consumed even when ingress or mailbox
    /// admission fails. The shared counter records exactly one terminal bucket
    /// for every submitted message.
    pub fn try_send_outcome<M, R>(
        &self,
        address: Address<M, R>,
        message: M,
        outcomes: &HostBurstOutcomes,
    ) -> Result<(), ThreadedTrySendError>
    where
        M: Send + 'static,
        R: 'static,
    {
        self.runtime().try_send_outcome(address, message, outcomes)
    }

    /// Retries observed admission until the message lands or `deadline` passes.
    ///
    /// This forwards [`ThreadedRuntime::send_observed_until`] without changing
    /// its terminal vocabulary or ownership semantics. `make_message` runs once
    /// per real attempt, allowing non-`Clone` messages to be rebuilt after
    /// `Full`; an already elapsed deadline does not invoke it and cannot cause a
    /// later delivery.
    pub fn send_observed_until<M, R, MakeMessage>(
        &self,
        address: Address<M, R>,
        deadline: Instant,
        backoff: Duration,
        make_message: MakeMessage,
    ) -> Result<(), SendObservedUntilError>
    where
        M: Send + 'static,
        R: 'static,
        MakeMessage: FnMut() -> M,
    {
        self.runtime()
            .send_observed_until(address, deadline, backoff, make_message)
    }

    /// Retries typed split-service event admission until it lands or the
    /// deadline passes, without exposing the private service envelope.
    pub fn send_event_observed_until<Event, Request, MakeEvent>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        deadline: Instant,
        backoff: Duration,
        make_event: MakeEvent,
    ) -> Result<(), SendObservedUntilError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
        MakeEvent: FnMut() -> Event,
    {
        self.runtime()
            .send_event_observed_until(address, deadline, backoff, make_event)
    }

    /// Registers a typed waiter for the terminal value produced by
    /// [`tina::stop_with`] at `address`.
    ///
    /// Register the waiter before triggering the isolate. Eager registration
    /// failures and waiter outcomes match [`ThreadedRuntime::observe_result`]
    /// exactly; the local-system facade does not flatten result authority or
    /// terminal failure reasons.
    pub fn observe_result<T: Send + 'static, M: 'static, R: 'static>(
        &self,
        address: Address<M, R>,
    ) -> Result<crate::IsolateResultWaiter<T>, crate::ResultWaitError> {
        self.runtime().observe_result::<T, M, R>(address)
    }

    /// Registers a waiter for the next successful runtime TCP bind.
    ///
    /// Register before triggering the bind. FIFO registration, bounded
    /// observation, call failure, and runtime-stopped outcomes match
    /// [`ThreadedRuntime::observe_next_bound`] exactly.
    pub fn observe_next_bound(&self) -> Result<crate::BoundAddressWaiter, ThreadedRuntimeError> {
        self.runtime().observe_next_bound()
    }

    /// Registers a waiter for the targeted isolate's terminal stop event.
    pub fn observe_isolate_complete<M: 'static, R: 'static>(
        &self,
        address: Address<M, R>,
    ) -> Result<crate::IsolateCompleteWaiter, ThreadedRuntimeError> {
        self.runtime().observe_isolate_complete(address)
    }

    /// Registers a waiter for the next supervised restart of a direct child
    /// owned by `parent`.
    pub fn observe_child_restarted<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
    ) -> Result<crate::ChildRestartedWaiter, ThreadedRuntimeError> {
        self.runtime().observe_child_restarted(parent)
    }

    /// Returns the runtime-owned lifecycle report for direct children of
    /// `parent`, preserving foreign-system, unknown-shard, and stale-parent
    /// outcomes from [`ThreadedRuntime::child_lifecycle_report`].
    pub fn child_lifecycle_report<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
    ) -> Result<crate::ChildLifecycleReport, ThreadedRuntimeError> {
        self.runtime().child_lifecycle_report(parent)
    }

    /// Returns retained trace without failing the observability path.
    pub fn trace(&self) -> TraceSnapshot {
        self.runtime().trace()
    }

    /// Returns complete trace, failing if the worker can no longer report.
    pub fn complete_trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        self.runtime().complete_trace()
    }

    /// Returns counted pressure facts from the retained runtime trace.
    pub fn pressure_summary(&self) -> Result<crate::PressureSummary, ThreadedRuntimeError> {
        self.runtime().pressure_summary()
    }

    /// Returns cloneable runtime-level shutdown control without consuming the
    /// local-system owner.
    ///
    /// This is the bounded shutdown path for a [`LocalSystem`] shared through
    /// [`Arc`]. It preserves the same cached terminal report as consuming
    /// [`Self::shutdown`], without requiring `Arc::try_unwrap` or exposing the
    /// underlying threaded runtime. Service-level drain remains an application
    /// protocol; this handle controls runtime shutdown only.
    pub fn shutdown_handle(&self) -> crate::ThreadedShutdownHandle {
        self.runtime().shutdown_handle()
    }

    /// Runs a fallible workload, then always consumes this owner through
    /// bounded shutdown and terminal-report observation.
    ///
    /// The closure borrows the live system so `?` can be used for registration,
    /// host calls, sends, waits, and application validation without bypassing
    /// shutdown. `timeout` is one total budget for shutdown admission and
    /// terminal observation; it does not include workload execution. An
    /// observed report is also required to prove clean shutdown. After this
    /// bounded attempt, consuming the owner does not perform a second blocking
    /// shutdown attempt. A timed-out worker may therefore finish later, and an
    /// escaped shutdown handle retains control so it can retry admission or
    /// observe its cached report; the handle must eventually retry or be
    /// dropped. Without an escaped handle, consuming the owner disconnects the
    /// remaining control senders rather than claiming terminal truth.
    ///
    /// Workload and shutdown failures remain independent in
    /// [`RunToShutdownError`]. If the closure panics, the panic is not converted
    /// into an error. The bounded shutdown attempt and destructor disarm happen
    /// only after the closure returns, so panic unwinding uses the owner's
    /// existing blocking teardown contract.
    pub fn run_to_shutdown<T, E>(
        mut self,
        timeout: Duration,
        workload: impl FnOnce(&Self) -> Result<T, E>,
    ) -> Result<T, RunToShutdownError<E>> {
        let result = workload(&self);
        let shutdown = self.shutdown_handle().request_and_wait_report(timeout);
        self.runtime
            .as_mut()
            .expect("local system runtime is available")
            .disarm_owner_drop();
        drop(self);
        finish_run_to_shutdown(result, shutdown)
    }

    /// Runs a fallible workload whose report exposes a standard error, then
    /// performs the same bounded consuming shutdown as [`Self::run_to_shutdown`].
    ///
    /// This is the report-container form for types such as `anyhow::Error`
    /// that implement `AsRef<dyn Error + Send + Sync>` without implementing
    /// [`std::error::Error`] directly. The owned report is retained inside
    /// [`ReportedWorkloadError`], its real error remains the source, and
    /// workload-only, shutdown-only, and dual failures remain distinct.
    pub fn run_to_shutdown_reported<T, E>(
        self,
        timeout: Duration,
        workload: impl FnOnce(&Self) -> Result<T, E>,
    ) -> Result<T, RunToShutdownError<ReportedWorkloadError<E>>>
    where
        E: fmt::Debug
            + fmt::Display
            + AsRef<dyn std::error::Error + Send + Sync + 'static>
            + Send
            + Sync
            + 'static,
    {
        self.run_to_shutdown(timeout, |app| {
            workload(app).map_err(ReportedWorkloadError::new)
        })
    }

    /// Begins graceful shutdown.
    pub fn shutdown(self) -> LocalSystemShutdown<S, F> {
        LocalSystemShutdown {
            runtime: self.runtime,
        }
    }

    /// Consumes the app and returns its lower-level ThreadedRuntime.
    ///
    /// This is for bridge/adapters that need to share the backend runner behind
    /// an `Arc` while preserving `LocalSystem` as the preferred construction path.
    pub fn into_threaded_runtime(mut self) -> ThreadedRuntime<S, F> {
        self.runtime
            .take()
            .expect("local app runtime is unavailable after shutdown")
    }

    fn runtime(&self) -> &ThreadedRuntime<S, F> {
        self.runtime
            .as_ref()
            .expect("local app runtime is unavailable after shutdown")
    }
}

/// Builder for a single-shard [`LocalSystem`].
pub struct LocalSystemSingleShardBuilder<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    shard: S,
    mailbox_factory: F,
    config: LocalSystemConfig,
    trace_observer: Option<Arc<dyn TraceObserver>>,
}

impl<S, F> LocalSystemSingleShardBuilder<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Sets bounded ingress command capacity.
    pub const fn ingress_capacity(mut self, capacity: usize) -> Self {
        self.config.ingress_capacity = capacity;
        self.config.shard_pair_capacity = capacity;
        self
    }

    /// Sets trace retention for the worker-owned runtime.
    pub const fn trace_retention(mut self, retention: TraceRetention) -> Self {
        self.config.trace_retention = retention;
        self
    }

    /// Sets bounded storage-lane capacity for local persistence work.
    pub const fn storage_lane_capacity(mut self, capacity: usize) -> Self {
        self.config.storage_lane_capacity = capacity;
        self
    }

    /// Sets bounded DNS-lane capacity.
    pub const fn dns_lane_capacity(mut self, capacity: usize) -> Self {
        self.config.dns_lane_capacity = capacity;
        self
    }

    /// Sets bounded TLS-lane capacity.
    pub const fn tls_lane_capacity(mut self, capacity: usize) -> Self {
        self.config.tls_lane_capacity = capacity;
        self
    }

    /// Sets bounded process-lane capacity.
    pub const fn process_lane_capacity(mut self, capacity: usize) -> Self {
        self.config.process_lane_capacity = capacity;
        self
    }

    /// Sets bounded signal-wait capacity.
    pub const fn signal_capacity(mut self, capacity: usize) -> Self {
        self.config.signal_capacity = capacity;
        self
    }

    /// Sets the remote-inbound drain budget for fairness under cross-shard
    /// pressure.
    pub const fn remote_inbound_drain_budget(mut self, budget: usize) -> Self {
        self.config.remote_inbound_drain_budget = budget;
        self
    }

    /// Hard-pins this shard worker to OS CPU id `core` where the platform can.
    ///
    /// On Linux the worker pins via `sched_setaffinity` and topology reports
    /// [`crate::AffinityStatus::Applied`] with the observed core; `core` is an
    /// OS CPU id checked against the process's allowed affinity mask, not an
    /// index into `0..num_cpus`. Platforms without a hard pin report
    /// [`crate::AffinityStatus::Unsupported`]; a core outside the mask reports
    /// [`crate::AffinityStatus::Failed`] and the worker runs unpinned.
    pub const fn configured_core(mut self, core: usize) -> Self {
        self.config.configured_core = Some(core);
        self
    }

    /// Sets runtime-owned metadata reserves.
    pub const fn preallocation(mut self, preallocation: PreallocationConfig) -> Self {
        self.config.preallocation = preallocation;
        self
    }

    /// Sets the whole bounded-shape config.
    pub const fn config(mut self, config: LocalSystemConfig) -> Self {
        self.config = config;
        self
    }

    /// Sets idle wait duration for the live worker.
    pub const fn idle_wait(mut self, idle_wait: Duration) -> Self {
        self.config.idle_wait = idle_wait;
        self
    }

    /// Sets the per-shard shutdown lane drain timeout.
    pub const fn shutdown_lane_drain_timeout(mut self, timeout: Duration) -> Self {
        self.config.shutdown_lane_drain_timeout = timeout;
        self
    }

    /// Wires a live trace observer before the worker records anything.
    /// One observer. See [`crate::TraceObserver`] for hook rules.
    pub fn trace_observer(mut self, observer: Arc<dyn TraceObserver>) -> Self {
        self.trace_observer = Some(observer);
        self
    }

    /// Builds the local app and starts its worker.
    ///
    /// # Panics
    ///
    /// Panics when [`Self::try_build`] returns a startup error. Applications
    /// should prefer `try_build`; this method is a setup/test convenience.
    pub fn build(self) -> LocalSystem<S, F> {
        self.try_build()
            .expect("failed to build single-shard local system")
    }

    /// Validates configuration and starts the worker, returning startup failures.
    pub fn try_build(self) -> Result<LocalSystem<S, F>, StartupError> {
        self.config.validate()?;
        let runtime = match self.trace_observer {
            Some(observer) => ThreadedRuntime::try_with_config_and_trace_observer(
                self.shard,
                self.mailbox_factory,
                self.config.threaded_runtime_config(),
                observer,
            ),
            None => ThreadedRuntime::try_with_config(
                self.shard,
                self.mailbox_factory,
                self.config.threaded_runtime_config(),
            ),
        }?;
        Ok(LocalSystem {
            runtime: Some(runtime),
        })
    }
}

/// Graceful shutdown handle for [`LocalSystem`].
pub struct LocalSystemShutdown<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    runtime: Option<ThreadedRuntime<S, F>>,
}

impl<S, F> LocalSystemShutdown<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Marks the shutdown as draining runtime-owned work.
    ///
    /// Current Betelgeuse-backed shutdown drains/cancels inside worker
    /// shutdown. The method exists to keep the user-facing lifecycle explicit.
    pub fn drain(self) -> Self {
        self
    }

    /// Joins the worker and returns its terminal report.
    pub fn join(self) -> Result<LocalSystemTerminalReport, ThreadedRuntimeError> {
        let report = self.join_report();
        if let Some(error) = report.error() {
            Err(error)
        } else {
            Ok(report)
        }
    }

    /// Joins the worker and always returns the terminal lifecycle report.
    pub fn join_report(mut self) -> LocalSystemTerminalReport {
        let Some(runtime) = self.runtime.take() else {
            return LocalSystemTerminalReport::new(LocalSystemState::Closed, Vec::new());
        };
        runtime.shutdown_report()
    }
}

/// Builder for a multi-shard local app.
pub struct LocalSystemMultiShardBuilder<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    shards: Vec<S>,
    mailbox_factory: F,
    config: LocalSystemConfig,
    trace_observer: Option<Arc<dyn TraceObserver>>,
}

impl<S, F> LocalSystemMultiShardBuilder<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    /// Adds one shard to the local app topology.
    pub fn shard(mut self, shard: S) -> Self {
        self.shards.push(shard);
        self
    }

    /// Sets bounded ingress command capacity.
    pub const fn ingress_capacity(mut self, capacity: usize) -> Self {
        self.config.ingress_capacity = capacity;
        self
    }

    /// Names desired shard-pair capacity for live remote sends.
    ///
    /// The current live substrate routes remote sends through the target
    /// worker's bounded command queue, so this sets the same underlying
    /// capacity as [`ingress_capacity`](Self::ingress_capacity) until a
    /// dedicated live shard-pair queue exists.
    pub const fn shard_pair_capacity(mut self, capacity: usize) -> Self {
        self.config.shard_pair_capacity = capacity;
        self
    }

    /// Sets trace retention for worker-owned runtimes.
    pub const fn trace_retention(mut self, retention: TraceRetention) -> Self {
        self.config.trace_retention = retention;
        self
    }

    /// Sets bounded storage-lane capacity for local persistence work.
    pub const fn storage_lane_capacity(mut self, capacity: usize) -> Self {
        self.config.storage_lane_capacity = capacity;
        self
    }

    /// Sets bounded DNS-lane capacity.
    pub const fn dns_lane_capacity(mut self, capacity: usize) -> Self {
        self.config.dns_lane_capacity = capacity;
        self
    }

    /// Sets bounded TLS-lane capacity.
    pub const fn tls_lane_capacity(mut self, capacity: usize) -> Self {
        self.config.tls_lane_capacity = capacity;
        self
    }

    /// Sets bounded process-lane capacity.
    pub const fn process_lane_capacity(mut self, capacity: usize) -> Self {
        self.config.process_lane_capacity = capacity;
        self
    }

    /// Sets bounded signal-wait capacity.
    pub const fn signal_capacity(mut self, capacity: usize) -> Self {
        self.config.signal_capacity = capacity;
        self
    }

    /// Sets the per-worker remote-inbound drain budget for fairness under
    /// cross-shard pressure.
    pub const fn remote_inbound_drain_budget(mut self, budget: usize) -> Self {
        self.config.remote_inbound_drain_budget = budget;
        self
    }

    /// Hard-pins shard workers to OS CPU ids starting at `core`, where the
    /// platform can. The first shard in stable order pins to `core` and later
    /// shards to contiguous ids (`core + ordinal`). On Linux each worker pins
    /// via `sched_setaffinity` and reports [`crate::AffinityStatus::Applied`];
    /// platforms without a hard pin report
    /// [`crate::AffinityStatus::Unsupported`], and an id outside the process's
    /// allowed affinity mask reports [`crate::AffinityStatus::Failed`].
    pub const fn configured_core(mut self, core: usize) -> Self {
        self.config.configured_core = Some(core);
        self
    }

    /// Sets runtime-owned metadata reserves for every shard.
    pub const fn preallocation(mut self, preallocation: PreallocationConfig) -> Self {
        self.config.preallocation = preallocation;
        self
    }

    /// Sets the whole bounded-shape config.
    pub const fn config(mut self, config: LocalSystemConfig) -> Self {
        self.config = config;
        self
    }

    /// Sets idle wait duration for each live worker.
    pub const fn idle_wait(mut self, idle_wait: Duration) -> Self {
        self.config.idle_wait = idle_wait;
        self
    }

    /// Sets the per-shard shutdown lane drain timeout.
    pub const fn shutdown_lane_drain_timeout(mut self, timeout: Duration) -> Self {
        self.config.shutdown_lane_drain_timeout = timeout;
        self
    }

    /// Wires one live trace observer for every shard before any
    /// records. Per-shard order preserved; cross-shard order is
    /// whatever the threads produce.
    pub fn trace_observer(mut self, observer: Arc<dyn TraceObserver>) -> Self {
        self.trace_observer = Some(observer);
        self
    }

    /// Builds the multi-shard local app and starts one worker per shard.
    ///
    /// # Panics
    ///
    /// Panics when [`Self::try_build`] returns a startup error. Applications
    /// should prefer `try_build`; this method is a setup/test convenience.
    pub fn build(self) -> LocalMultiShardSystem<S, F> {
        self.try_build()
            .expect("failed to build multi-shard local system")
    }

    /// Validates topology/configuration and starts every shard worker.
    pub fn try_build(self) -> Result<LocalMultiShardSystem<S, F>, StartupError> {
        self.config.validate()?;
        let runtime = match self.trace_observer {
            Some(observer) => ThreadedMultiShardRuntime::try_with_config_and_trace_observer(
                self.shards,
                self.mailbox_factory,
                self.config.threaded_runtime_config(),
                observer,
            ),
            None => ThreadedMultiShardRuntime::try_with_config(
                self.shards,
                self.mailbox_factory,
                self.config.threaded_runtime_config(),
            ),
        }?;
        Ok(LocalMultiShardSystem {
            runtime: Some(runtime),
        })
    }
}

/// Canonical live app owner for a local multi-shard Tina service.
pub struct LocalMultiShardSystem<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    runtime: Option<ThreadedMultiShardRuntime<S, F>>,
}

impl<S, F> LocalMultiShardSystem<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    /// Returns the provenance stamped into addresses issued by this owner.
    pub fn system_incarnation(&self) -> tina::SystemIncarnation {
        self.runtime().system_incarnation()
    }

    /// Registers one root isolate on the chosen shard.
    #[allow(private_bounds)]
    pub fn register_root_on<I, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        self.runtime()
            .register_with_capacity_on::<I, Outbound>(shard, isolate, mailbox_capacity)
    }

    /// Registers one root isolate on `shard` and atomically prefills its
    /// bootstrap message.
    ///
    /// Unknown-shard, bounded command-admission, worker-lifecycle, and mailbox
    /// prefill failures retain the exact authority semantics of
    /// [`ThreadedMultiShardRuntime::register_with_capacity_and_bootstrap_on`].
    #[allow(private_bounds, clippy::type_complexity)]
    pub fn register_root_with_bootstrap_on<I, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap: I::Message,
    ) -> Result<Address<I::Message, I::Reply>, crate::ThreadedRegisterBootstrapError<I::Message>>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: Send + 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        self.runtime()
            .register_with_capacity_and_bootstrap_on::<I, Outbound>(
                shard,
                isolate,
                mailbox_capacity,
                bootstrap,
            )
    }

    /// Registers one root on the chosen shard whose constructor receives its
    /// final typed address before the entry is published. Pre-admission failure
    /// drops the constructor without running it; an accepted
    /// `WorkerUnresponsive` constructor may still publish later on that shard.
    #[allow(private_bounds)]
    pub fn register_root_using_on<I, Outbound, Ctor>(
        &self,
        shard: ShardId,
        mailbox_capacity: usize,
        construct: Ctor,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
        Ctor: FnOnce(Address<I::Message, I::Reply>) -> I + Send + 'static,
    {
        self.runtime()
            .register_with_capacity_using_on::<I, Outbound, _>(shard, mailbox_capacity, construct)
    }

    /// Registers one split event/request root service on the chosen shard.
    ///
    /// Returns [`ThreadedRuntimeError::UnknownShard`] when `shard` is not
    /// owned by this local system.
    #[allow(private_bounds)]
    pub fn register_split_service_on<I, Event, Request, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<crate::SplitServiceHandle<Event, Request, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<
                Shard = S,
                Message = tina::ServiceMessage<Event, Request>,
                Send = TinaOutbound<Outbound>,
            > + tina::CallableIsolate
            + Send
            + 'static,
        Event: 'static,
        Request: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        self.runtime()
            .register_split_service_on::<I, Event, Request, Outbound>(
                shard,
                isolate,
                mailbox_capacity,
            )
    }

    /// Registers one event-only root service on the chosen shard.
    ///
    /// Returns [`ThreadedRuntimeError::UnknownShard`] when `shard` is not
    /// owned by this local system.
    #[allow(private_bounds)]
    pub fn register_event_service_on<I, Event, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<crate::EventServiceHandle<Event>, ThreadedRuntimeError>
    where
        I: Isolate<
                Shard = S,
                Message = tina::ServiceMessage<Event, std::convert::Infallible>,
                Reply = (),
                Send = TinaOutbound<Outbound>,
            > + Send
            + 'static,
        Event: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        self.runtime()
            .register_event_service_on::<I, Event, Outbound>(shard, isolate, mailbox_capacity)
    }

    /// Registers one request-only root service on the chosen shard.
    ///
    /// Returns [`ThreadedRuntimeError::UnknownShard`] when `shard` is not
    /// owned by this local system.
    #[allow(private_bounds)]
    pub fn register_request_service_on<I, Request, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<crate::RequestServiceHandle<Request, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<
                Shard = S,
                Message = tina::ServiceMessage<std::convert::Infallible, Request>,
                Send = TinaOutbound<Outbound>,
            > + tina::CallableIsolate
            + Send
            + 'static,
        Request: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        self.runtime()
            .register_request_service_on::<I, Request, Outbound>(shard, isolate, mailbox_capacity)
    }

    /// Configures a registered root as a supervisor.
    pub fn supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedRuntimeError> {
        self.runtime().supervise(parent, config)
    }

    /// Registers a waiter for the next direct-child restart reported on the
    /// parent address's owning shard.
    ///
    /// The outer error preserves worker and unknown-shard routing failures
    /// from [`ThreadedMultiShardRuntime::observe_child_restarted`].
    pub fn observe_child_restarted<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
    ) -> Result<crate::ChildRestartedWaiter, ThreadedRuntimeError> {
        self.runtime().observe_child_restarted(parent)
    }

    /// Returns the runtime-owned lifecycle report for direct children of
    /// `parent` on its owning shard, preserving typed provenance, routing,
    /// and stale-parent outcomes.
    pub fn child_lifecycle_report<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
    ) -> Result<crate::ChildLifecycleReport, ThreadedRuntimeError> {
        self.runtime().child_lifecycle_report(parent)
    }

    /// Attempts one bounded ingress handoff to the owning worker shard.
    ///
    /// Returns [`ThreadedTrySendError::UnknownShard`] when the address targets
    /// a shard not owned by this local system.
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        self.runtime().try_send(address, message)
    }

    /// Attempts bounded ingress through a service event capability.
    ///
    /// Returns [`ThreadedTrySendError::UnknownShard`] when the address targets
    /// a shard not owned by this local system.
    pub fn try_send_event<Event, Request>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        event: Event,
    ) -> Result<(), ThreadedTrySendError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
    {
        self.runtime().try_send_event(address, event)
    }

    /// Sends one raw typed message and observes the exact mailbox outcome on
    /// its owning shard.
    ///
    /// Returns [`ThreadedSendObservedError::UnknownShard`] when the address
    /// targets a shard not owned by this local system.
    pub fn send_and_observe<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedSendObservedError> {
        self.runtime().send_and_observe(address, message)
    }

    /// Sends one typed service event and observes the exact mailbox outcome on
    /// its owning shard without exposing the private service envelope.
    ///
    /// Returns [`ThreadedSendObservedError::UnknownShard`] when the address
    /// targets a shard not owned by this local system.
    pub fn send_event_and_observe<Event, Request>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        event: Event,
    ) -> Result<(), ThreadedSendObservedError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
    {
        self.runtime().send_event_and_observe(address, event)
    }

    /// Attempts one typed ingress send on the address's owning shard and
    /// records its eventual mailbox outcome.
    ///
    /// `message` is consumed on every host- and worker-side outcome. Accepted
    /// observations settle exactly once through `outcomes`.
    ///
    /// Returns [`ThreadedTrySendError::UnknownShard`] before registering a
    /// burst submission when `address` targets another shard topology.
    pub fn try_send_outcome<M, R>(
        &self,
        address: Address<M, R>,
        message: M,
        outcomes: &HostBurstOutcomes,
    ) -> Result<(), ThreadedTrySendError>
    where
        M: Send + 'static,
        R: 'static,
    {
        self.runtime().try_send_outcome(address, message, outcomes)
    }

    /// Retries observed admission on the address's owning shard until the
    /// message lands or `deadline` passes.
    ///
    /// A `Timeout` cannot deliver later, and `make_message` runs only for a real
    /// bounded attempt.
    ///
    /// Returns [`SendObservedUntilError::UnknownShard`] before invoking
    /// `make_message` when `address` targets another shard topology.
    pub fn send_observed_until<M, R, MakeMessage>(
        &self,
        address: Address<M, R>,
        deadline: Instant,
        backoff: Duration,
        make_message: MakeMessage,
    ) -> Result<(), SendObservedUntilError>
    where
        M: Send + 'static,
        R: 'static,
        MakeMessage: FnMut() -> M,
    {
        self.runtime()
            .send_observed_until(address, deadline, backoff, make_message)
    }

    /// Retries typed split-service event admission on the owning shard without
    /// exposing the private service envelope.
    ///
    /// Returns [`SendObservedUntilError::UnknownShard`] before invoking
    /// `make_event` when `address` targets another shard topology.
    pub fn send_event_observed_until<Event, Request, MakeEvent>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        deadline: Instant,
        backoff: Duration,
        make_event: MakeEvent,
    ) -> Result<(), SendObservedUntilError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
        MakeEvent: FnMut() -> Event,
    {
        self.runtime()
            .send_event_observed_until(address, deadline, backoff, make_event)
    }

    /// Performs one typed isolate call from the host thread, routed by the
    /// shard carried in `address`.
    ///
    /// This is the host-call companion to [`Self::register_root_on`]. It
    /// preserves the backend's [`crate::CallOutcome`] terminal vocabulary.
    ///
    /// # Panics
    ///
    /// Panics when the address targets a shard not owned by this local system,
    /// matching [`Self::try_send`] and the lower-level threaded owner.
    pub fn call_blocking<M, R>(
        &self,
        address: Address<M, R>,
        message: M,
        timeout: Duration,
    ) -> Result<crate::CallOutcome<R>, ThreadedRuntimeError>
    where
        M: Send + 'static,
        R: Send + 'static,
    {
        self.runtime().call_blocking(address, message, timeout)
    }

    /// Performs one blocking host call through a split-service request
    /// capability, routed by the shard carried in `address`.
    ///
    /// # Panics
    ///
    /// Panics when the address targets a shard not owned by this local system,
    /// matching [`Self::call_blocking`].
    pub fn call_blocking_request<Event, Request, Reply>(
        &self,
        address: tina::ServiceRequestAddress<Event, Request, Reply>,
        request: Request,
        timeout: Duration,
    ) -> Result<crate::CallOutcome<Reply>, ThreadedRuntimeError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
        Reply: Send + 'static,
    {
        self.runtime()
            .call_blocking_request(address, request, timeout)
    }

    /// Registers a typed waiter for the terminal value produced by
    /// [`tina::stop_with`] on the shard carried by `address`.
    ///
    /// Routing, eager errors, and waiter outcomes match
    /// [`ThreadedMultiShardRuntime::observe_result`]. An address for a shard
    /// outside this local system returns [`crate::ResultWaitError::UnknownShard`].
    pub fn observe_result<T: Send + 'static, M: 'static, R: 'static>(
        &self,
        address: Address<M, R>,
    ) -> Result<crate::IsolateResultWaiter<T>, crate::ResultWaitError> {
        self.runtime().observe_result::<T, M, R>(address)
    }

    /// Returns retained trace without failing the observability path.
    pub fn trace(&self) -> TraceSnapshot {
        self.runtime().trace()
    }

    /// Returns complete trace, failing if any shard can no longer report.
    pub fn complete_trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        self.runtime().complete_trace()
    }

    /// Returns a live topology snapshot for this app.
    pub fn topology(&self) -> LiveTopologyReport {
        self.runtime().topology()
    }

    /// Returns the live runtime capability table for this app.
    pub fn capabilities(&self) -> RuntimeCapabilities {
        self.runtime().capabilities()
    }

    /// Returns cloneable runtime-level shutdown control for every owned shard
    /// without consuming the local-system owner.
    ///
    /// Partial multi-shard admission progress, retry behavior, and the cached
    /// terminal report are identical to
    /// [`ThreadedMultiShardRuntime::shutdown_handle`].
    pub fn shutdown_handle(&self) -> crate::ThreadedShutdownHandle {
        self.runtime().shutdown_handle()
    }

    /// Runs a fallible workload, then always consumes every owned shard through
    /// bounded shutdown and terminal-report observation.
    ///
    /// This is the multi-shard parity form of [`LocalSystem::run_to_shutdown`].
    /// Shutdown admission progress remains shard-aware and both workload and
    /// terminal failures are preserved in [`RunToShutdownError`]. As in the
    /// single-shard form, `timeout` covers admission and observation, not the
    /// workload. Consuming the owner does not extend that deadline with its
    /// ordinary blocking `Drop` shutdown path. The single-shard timeout and
    /// escaped-handle ownership rules apply identically to partial multi-shard
    /// admission.
    pub fn run_to_shutdown<T, E>(
        mut self,
        timeout: Duration,
        workload: impl FnOnce(&Self) -> Result<T, E>,
    ) -> Result<T, RunToShutdownError<E>> {
        let result = workload(&self);
        let shutdown = self.shutdown_handle().request_and_wait_report(timeout);
        self.runtime
            .as_mut()
            .expect("local multi-shard runtime is available")
            .disarm_owner_drop();
        drop(self);
        finish_run_to_shutdown(result, shutdown)
    }

    /// Multi-shard parity form of [`LocalSystem::run_to_shutdown_reported`].
    ///
    /// The workload report stays owned and source-linked while the existing
    /// shard-aware bounded shutdown and dual-failure contract remains unchanged.
    pub fn run_to_shutdown_reported<T, E>(
        self,
        timeout: Duration,
        workload: impl FnOnce(&Self) -> Result<T, E>,
    ) -> Result<T, RunToShutdownError<ReportedWorkloadError<E>>>
    where
        E: fmt::Debug
            + fmt::Display
            + AsRef<dyn std::error::Error + Send + Sync + 'static>
            + Send
            + Sync
            + 'static,
    {
        self.run_to_shutdown(timeout, |app| {
            workload(app).map_err(ReportedWorkloadError::new)
        })
    }

    /// Begins graceful shutdown.
    pub fn shutdown(self) -> LocalMultiShardSystemShutdown<S, F> {
        LocalMultiShardSystemShutdown {
            runtime: self.runtime,
        }
    }

    fn runtime(&self) -> &ThreadedMultiShardRuntime<S, F> {
        self.runtime
            .as_ref()
            .expect("local multi-shard app runtime is unavailable after shutdown")
    }
}

/// Graceful shutdown handle for [`LocalMultiShardSystem`].
pub struct LocalMultiShardSystemShutdown<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    runtime: Option<ThreadedMultiShardRuntime<S, F>>,
}

impl<S, F> LocalMultiShardSystemShutdown<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    /// Marks the shutdown as draining runtime-owned work.
    pub fn drain(self) -> Self {
        self
    }

    /// Joins all workers and returns a terminal report.
    pub fn join(self) -> Result<LocalSystemTerminalReport, ThreadedRuntimeError> {
        let report = self.join_report();
        if let Some(error) = report.error() {
            Err(error)
        } else {
            Ok(report)
        }
    }

    /// Joins all workers and always returns the terminal lifecycle report.
    pub fn join_report(mut self) -> LocalSystemTerminalReport {
        let Some(runtime) = self.runtime.take() else {
            return LocalSystemTerminalReport::new(LocalSystemState::Closed, Vec::new());
        };
        runtime.shutdown_report()
    }
}
