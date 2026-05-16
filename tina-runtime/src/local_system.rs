//! Local system app owners extracted from lib.rs (phase 055).
//!
//! Houses `LocalSystem`, `LocalMultiShardSystem`, their builders/shutdown
//! handles, the bounded-shape `LocalSystemConfig`, terminal report types,
//! and the threaded-worker exit/type-alias helpers consumed by
//! `ThreadedRuntime`.

use std::alloc::Global;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use betelgeuse::IOLoopHandle;
use tina::{Address, Isolate, Outbound as TinaOutbound, Shard, ShardId};
use tina_supervisor::SupervisorConfig;

use crate::call::IntoErasedCall;
use crate::capabilities::RuntimeCapabilities;
use crate::driver;
use crate::errors::{ThreadedRuntimeError, ThreadedSendObservedError, ThreadedTrySendError};
use crate::live_report::{LiveShardReport, LiveShardState, LiveTopologyReport};
use crate::mailbox::MailboxFactory;
use crate::observer::TraceObserver;
use crate::trace::{RuntimeEvent, RuntimeEventKind};
use crate::{
    DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT, IntoErasedSpawn, IntoErasedSpawnObserved,
    PreallocationConfig, Runtime, ThreadedMultiShardRuntime, ThreadedRuntime,
    ThreadedRuntimeConfig, TraceRetention,
};

/// Preferred public bounded-shape config for local Tina systems.
///
/// `ThreadedRuntimeConfig` remains the lower-level worker config. This type is
/// the user-facing manifest: every bounded live resource family is either
/// configurable here or named as a fixed capability in [`RuntimeCapabilities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalSystemConfig {
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
    /// Capacity of the bounded TLS lane.
    pub tls_lane_capacity: usize,
    /// Capacity of the bounded process lane.
    pub process_lane_capacity: usize,
    /// Capacity of runtime-owned signal waits.
    pub signal_capacity: usize,
    /// Desired OS core for shard workers. This is advisory until a backend can
    /// prove hard affinity. Multi-shard local systems treat this as the first
    /// core in stable shard order and assign later shards to contiguous cores.
    pub configured_core: Option<usize>,
    /// Setup-time reserves for runtime-owned metadata.
    pub preallocation: PreallocationConfig,
    /// Trace retention for worker-owned runtimes.
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
            ingress_capacity: ThreadedRuntimeConfig::default().command_capacity,
            shard_pair_capacity: ThreadedRuntimeConfig::default().command_capacity,
            remote_inbound_drain_budget: ThreadedRuntimeConfig::default()
                .remote_inbound_drain_budget,
            storage_lane_capacity: driver::DEFAULT_STORAGE_LANE_CAPACITY,
            dns_lane_capacity: driver::DEFAULT_DNS_LANE_CAPACITY,
            tls_lane_capacity: driver::DEFAULT_TLS_LANE_CAPACITY,
            process_lane_capacity: driver::DEFAULT_PROCESS_LANE_CAPACITY,
            signal_capacity: driver::DEFAULT_SIGNAL_CAPACITY,
            configured_core: None,
            preallocation: PreallocationConfig::default(),
            trace_retention: TraceRetention::Full,
            idle_wait: Duration::from_millis(1),
            shutdown_lane_drain_timeout: DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT,
        }
    }
}

impl LocalSystemConfig {
    /// Validates that no bounded resource silently starts with zero capacity.
    pub fn validate(&self) -> Result<(), LocalSystemConfigError> {
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
        Ok(())
    }

    fn threaded_runtime_config(self) -> ThreadedRuntimeConfig {
        ThreadedRuntimeConfig {
            command_capacity: self.ingress_capacity,
            shard_pair_capacity: self.shard_pair_capacity,
            remote_inbound_drain_budget: self.remote_inbound_drain_budget,
            storage_lane_capacity: self.storage_lane_capacity,
            dns_lane_capacity: self.dns_lane_capacity,
            tls_lane_capacity: self.tls_lane_capacity,
            process_lane_capacity: self.process_lane_capacity,
            signal_capacity: self.signal_capacity,
            configured_core: self.configured_core,
            preallocation: self.preallocation,
            trace_retention: self.trace_retention,
            idle_wait: self.idle_wait,
            shutdown_lane_drain_timeout: self.shutdown_lane_drain_timeout,
        }
    }
}

/// Invalid local system bounded-shape config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSystemConfigError {
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
}

pub(crate) type ThreadedCommandFn<S, F> = Box<dyn FnOnce(&mut Runtime<S, F>) + Send>;
pub(crate) type ThreadedIoLoopFactory = Box<dyn FnOnce() -> IOLoopHandle<Global> + Send>;
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
}

impl TraceSnapshot {
    pub(crate) fn complete(events: Vec<RuntimeEvent>) -> Self {
        Self {
            events,
            missing_shards: Vec::new(),
        }
    }

    pub(crate) fn partial(events: Vec<RuntimeEvent>, missing_shards: Vec<ShardId>) -> Self {
        Self {
            events,
            missing_shards,
        }
    }

    /// Retained trace events that could still be collected.
    pub fn events(&self) -> &[RuntimeEvent] {
        &self.events
    }

    /// Whether every shard reported trace successfully.
    pub fn is_complete(&self) -> bool {
        self.missing_shards.is_empty()
    }

    /// Whether at least one shard could not report trace.
    pub fn is_partial(&self) -> bool {
        !self.is_complete()
    }

    /// Shards that could not report trace.
    pub fn missing_shards(&self) -> &[ShardId] {
        &self.missing_shards
    }

    /// Returns complete trace events, or a typed error if any shard was missing.
    pub fn complete_events(self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        if self.is_complete() {
            Ok(self.events)
        } else {
            Err(ThreadedRuntimeError::WorkerStopped)
        }
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
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: 'static,
    {
        self.runtime()
            .register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
    }

    /// Configures a registered root as a supervisor.
    pub fn supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedRuntimeError> {
        self.runtime().supervise(parent, config)
    }

    /// Attempts one bounded ingress handoff.
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        self.runtime().try_send(address, message)
    }

    /// Attempts one ingress send and observes the mailbox outcome.
    pub fn send_and_observe<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedSendObservedError> {
        self.runtime().send_and_observe(address, message)
    }

    /// Returns retained trace without failing the observability path.
    pub fn trace(&self) -> TraceSnapshot {
        self.runtime().trace()
    }

    /// Returns complete trace, failing if the worker can no longer report.
    pub fn complete_trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        self.runtime().complete_trace()
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

    /// Records desired worker core ownership as advisory intent.
    ///
    /// The current portable backend does not hard-pin the worker. Topology
    /// reports show [`crate::AffinityStatus::AdvisoryOnly`] when this is set.
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
    pub fn build(self) -> LocalSystem<S, F> {
        self.config
            .validate()
            .expect("invalid LocalSystemConfig for single-shard system");
        let runtime = match self.trace_observer {
            Some(observer) => ThreadedRuntime::with_config_and_trace_observer(
                self.shard,
                self.mailbox_factory,
                self.config.threaded_runtime_config(),
                observer,
            ),
            None => ThreadedRuntime::with_config(
                self.shard,
                self.mailbox_factory,
                self.config.threaded_runtime_config(),
            ),
        };
        LocalSystem {
            runtime: Some(runtime),
        }
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

    /// Records desired worker core ownership as advisory intent for every
    /// shard in this local system.
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
    pub fn build(self) -> LocalMultiShardSystem<S, F> {
        self.config
            .validate()
            .expect("invalid LocalSystemConfig for multi-shard system");
        let runtime = match self.trace_observer {
            Some(observer) => ThreadedMultiShardRuntime::with_config_and_trace_observer(
                self.shards,
                self.mailbox_factory,
                self.config.threaded_runtime_config(),
                observer,
            ),
            None => ThreadedMultiShardRuntime::with_config(
                self.shards,
                self.mailbox_factory,
                self.config.threaded_runtime_config(),
            ),
        };
        LocalMultiShardSystem {
            runtime: Some(runtime),
        }
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
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: Send + 'static,
    {
        self.runtime()
            .register_with_capacity_on::<I, Outbound>(shard, isolate, mailbox_capacity)
    }

    /// Configures a registered root as a supervisor.
    pub fn supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedRuntimeError> {
        self.runtime().supervise(parent, config)
    }

    /// Attempts one bounded ingress handoff to the owning worker shard.
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        self.runtime().try_send(address, message)
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
