//! Live observability reports: per-shard, per-queue, and topology
//! snapshots that user code reads from a live runtime.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use tina::ShardId;

use crate::driver::DriverResourceReport;
use crate::{PreallocationConfig, ThreadedRuntimeConfig, TraceRetention};

/// Observable lifecycle state for one live shard worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveShardState {
    /// The shard worker is running and accepts bounded ingress.
    Running,

    /// The shard worker stopped after graceful shutdown.
    Stopped,

    /// The shard worker failed or became unreachable before clean shutdown.
    Failed,
}

impl LiveShardState {
    const RUNNING: u8 = 0;
    const STOPPED: u8 = 1;
    const FAILED: u8 = 2;

    const fn from_raw(raw: u8) -> Self {
        match raw {
            Self::RUNNING => Self::Running,
            Self::STOPPED => Self::Stopped,
            _ => Self::Failed,
        }
    }
}

/// Snapshot of one bounded queue's visible pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveQueueReport {
    capacity: usize,
    depth: Option<usize>,
    accepted: Option<usize>,
    rejected_full: Option<usize>,
    rejected_closed: Option<usize>,
}

impl LiveQueueReport {
    const fn new(
        capacity: usize,
        depth: Option<usize>,
        accepted: Option<usize>,
        rejected_full: Option<usize>,
        rejected_closed: Option<usize>,
    ) -> Self {
        Self {
            capacity,
            depth,
            accepted,
            rejected_full,
            rejected_closed,
        }
    }

    pub(crate) const fn unmeasured(capacity: usize) -> Self {
        Self::new(capacity, None, None, None, None)
    }

    /// Stable configured queue capacity.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Exact or sampled queue depth when the runtime can report it honestly.
    pub const fn depth(&self) -> Option<usize> {
        self.depth
    }

    /// Count of visible accepted handoffs for this queue, when measured.
    pub const fn accepted(&self) -> Option<usize> {
        self.accepted
    }

    /// Count of visible rejections caused by full bounded capacity, when measured.
    pub const fn rejected_full(&self) -> Option<usize> {
        self.rejected_full
    }

    /// Count of visible rejections caused by a stopped/closed destination, when measured.
    pub const fn rejected_closed(&self) -> Option<usize> {
        self.rejected_closed
    }
}

#[derive(Debug)]
pub(crate) struct LiveQueueMetrics {
    capacity: usize,
    accepted: AtomicUsize,
    rejected_full: AtomicUsize,
    rejected_closed: AtomicUsize,
}

impl LiveQueueMetrics {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            accepted: AtomicUsize::new(0),
            rejected_full: AtomicUsize::new(0),
            rejected_closed: AtomicUsize::new(0),
        }
    }

    pub(crate) fn accepted(&self) {
        self.accepted.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn rejected_full(&self) {
        self.rejected_full.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn rejected_closed(&self) {
        self.rejected_closed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn report(&self) -> LiveQueueReport {
        LiveQueueReport::new(
            self.capacity,
            None,
            Some(self.accepted.load(Ordering::Relaxed)),
            Some(self.rejected_full.load(Ordering::Relaxed)),
            Some(self.rejected_closed.load(Ordering::Relaxed)),
        )
    }
}

#[derive(Debug)]
pub(crate) struct LiveShardMetrics {
    shard: ShardId,
    worker_name: Option<String>,
    worker_thread_id: Mutex<Option<String>>,
    configured_core: Option<usize>,
    affinity_status: AffinityStatus,
    preallocation: PreallocationConfig,
    pub(crate) config: ThreadedRuntimeConfig,
    state: AtomicU8,
    pub(crate) ingress: LiveQueueMetrics,
    storage_lane: LiveQueueMetrics,
    trace_retention: TraceRetention,
    owned_resource_count: AtomicUsize,
    worker_held_resource_count: AtomicUsize,
    pending_driver_call_count: AtomicUsize,
}

impl LiveShardMetrics {
    pub(crate) fn new(
        shard: ShardId,
        worker_name: Option<String>,
        config: ThreadedRuntimeConfig,
    ) -> Self {
        Self {
            shard,
            worker_name,
            worker_thread_id: Mutex::new(None),
            configured_core: config.configured_core,
            affinity_status: if config.configured_core.is_some() {
                AffinityStatus::AdvisoryOnly
            } else {
                AffinityStatus::NotRequested
            },
            preallocation: config.preallocation,
            config,
            state: AtomicU8::new(LiveShardState::RUNNING),
            ingress: LiveQueueMetrics::new(config.command_capacity),
            storage_lane: LiveQueueMetrics::new(config.storage_lane_capacity),
            trace_retention: config.trace_retention,
            owned_resource_count: AtomicUsize::new(0),
            worker_held_resource_count: AtomicUsize::new(0),
            pending_driver_call_count: AtomicUsize::new(0),
        }
    }

    pub(crate) fn state(&self) -> LiveShardState {
        LiveShardState::from_raw(self.state.load(Ordering::Acquire))
    }

    pub(crate) fn set_state(&self, state: LiveShardState) {
        let raw = match state {
            LiveShardState::Running => LiveShardState::RUNNING,
            LiveShardState::Stopped => LiveShardState::STOPPED,
            LiveShardState::Failed => LiveShardState::FAILED,
        };
        self.state.store(raw, Ordering::Release);
    }

    pub(crate) fn set_resource_counts(&self, report: DriverResourceReport) {
        self.owned_resource_count
            .store(report.owned_resource_count(), Ordering::Release);
        self.worker_held_resource_count
            .store(report.worker_held_resource_count(), Ordering::Release);
        self.pending_driver_call_count
            .store(report.pending_driver_call_count(), Ordering::Release);
    }

    pub(crate) fn set_worker_thread_id(&self, id: String) {
        *self
            .worker_thread_id
            .lock()
            .expect("worker thread id lock poisoned") = Some(id);
    }

    pub(crate) fn report(&self) -> LiveShardReport {
        LiveShardReport {
            shard: self.shard,
            worker_name: self.worker_name.clone(),
            worker_thread_id: self
                .worker_thread_id
                .lock()
                .expect("worker thread id lock poisoned")
                .clone(),
            configured_core: self.configured_core,
            observed_core: None,
            affinity_status: self.affinity_status.clone(),
            preallocation: self.preallocation,
            remote_inbound_drain_budget: self.config.remote_inbound_drain_budget,
            shutdown_lane_drain_timeout: self.config.shutdown_lane_drain_timeout,
            state: self.state(),
            ingress: self.ingress.report(),
            storage_lane: LiveQueueReport::unmeasured(self.storage_lane.capacity),
            dns_lane: LiveQueueReport::unmeasured(self.config.dns_lane_capacity),
            tls_lane: LiveQueueReport::unmeasured(self.config.tls_lane_capacity),
            process_lane: LiveQueueReport::unmeasured(self.config.process_lane_capacity),
            signal_lane: LiveQueueReport::unmeasured(self.config.signal_capacity),
            trace_retention: self.trace_retention,
            trace_dropped: None,
            owned_resource_count: self.owned_resource_count.load(Ordering::Acquire),
            worker_held_resource_count: self.worker_held_resource_count.load(Ordering::Acquire),
            pending_driver_call_count: self.pending_driver_call_count.load(Ordering::Acquire),
        }
    }
}

/// Shard-worker affinity state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AffinityStatus {
    /// No core affinity was requested.
    NotRequested,
    /// The backend proved hard affinity was applied.
    Applied,
    /// The platform/backend cannot support hard affinity.
    Unsupported,
    /// Affinity was requested but failed with a visible reason.
    Failed(String),
    /// Affinity is recorded as ownership intent only; no OS scheduling control
    /// is claimed.
    AdvisoryOnly,
}

/// Snapshot of one live shard worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveShardReport {
    pub(crate) shard: ShardId,
    pub(crate) worker_name: Option<String>,
    pub(crate) worker_thread_id: Option<String>,
    pub(crate) configured_core: Option<usize>,
    pub(crate) observed_core: Option<usize>,
    pub(crate) affinity_status: AffinityStatus,
    pub(crate) preallocation: PreallocationConfig,
    pub(crate) remote_inbound_drain_budget: usize,
    pub(crate) shutdown_lane_drain_timeout: Duration,
    pub(crate) state: LiveShardState,
    pub(crate) ingress: LiveQueueReport,
    pub(crate) storage_lane: LiveQueueReport,
    pub(crate) dns_lane: LiveQueueReport,
    pub(crate) tls_lane: LiveQueueReport,
    pub(crate) process_lane: LiveQueueReport,
    pub(crate) signal_lane: LiveQueueReport,
    pub(crate) trace_retention: TraceRetention,
    pub(crate) trace_dropped: Option<u64>,
    pub(crate) owned_resource_count: usize,
    pub(crate) worker_held_resource_count: usize,
    pub(crate) pending_driver_call_count: usize,
}

impl LiveShardReport {
    /// Shard owned by this worker.
    pub const fn shard(&self) -> ShardId {
        self.shard
    }

    /// Worker thread name when the live substrate can name it.
    pub fn worker_name(&self) -> Option<&str> {
        self.worker_name.as_deref()
    }

    /// Worker thread id formatted by the live backend, when the worker has
    /// started and reported it.
    pub fn worker_thread_id(&self) -> Option<&str> {
        self.worker_thread_id.as_deref()
    }

    /// Desired core configured for this shard worker.
    pub const fn configured_core(&self) -> Option<usize> {
        self.configured_core
    }

    /// Observed core when the backend can report it honestly.
    pub const fn observed_core(&self) -> Option<usize> {
        self.observed_core
    }

    /// Affinity status for this worker.
    pub const fn affinity_status(&self) -> &AffinityStatus {
        &self.affinity_status
    }

    /// Runtime-owned metadata reserves configured for this shard.
    pub const fn preallocation(&self) -> PreallocationConfig {
        self.preallocation
    }

    /// Maximum remote envelopes this worker harvests before giving local work
    /// a turn.
    pub const fn remote_inbound_drain_budget(&self) -> usize {
        self.remote_inbound_drain_budget
    }

    /// Per-shard budget for draining lane work during shutdown.
    pub const fn shutdown_lane_drain_timeout(&self) -> Duration {
        self.shutdown_lane_drain_timeout
    }

    /// Observable lifecycle state.
    pub const fn state(&self) -> LiveShardState {
        self.state
    }

    /// Visible bounded ingress queue pressure.
    pub const fn ingress(&self) -> &LiveQueueReport {
        &self.ingress
    }

    /// Visible bounded storage lane pressure.
    pub const fn storage_lane(&self) -> &LiveQueueReport {
        &self.storage_lane
    }

    /// Configured DNS lane capacity. Live depth/accept/reject counters
    /// are not measured for this lane today; the capacity is reported
    /// for honest topology coverage.
    pub const fn dns_lane(&self) -> &LiveQueueReport {
        &self.dns_lane
    }

    /// Configured TLS lane capacity. Live depth/accept/reject counters
    /// are not measured for this lane today.
    pub const fn tls_lane(&self) -> &LiveQueueReport {
        &self.tls_lane
    }

    /// Configured process lane capacity. Live depth/accept/reject
    /// counters are not measured for this lane today.
    pub const fn process_lane(&self) -> &LiveQueueReport {
        &self.process_lane
    }

    /// Configured signal-wait capacity. Live depth/accept/reject
    /// counters are not measured for this lane today.
    pub const fn signal_lane(&self) -> &LiveQueueReport {
        &self.signal_lane
    }

    /// Configured trace retention.
    pub const fn trace_retention(&self) -> TraceRetention {
        self.trace_retention
    }

    /// Dropped trace count when available without probing the worker.
    pub const fn trace_dropped(&self) -> Option<u64> {
        self.trace_dropped
    }

    /// Live table-owned driver resource handles known to this worker.
    ///
    /// Counts runtime-table ids handed back to user code: TCP listeners and
    /// streams, TLS listeners and streams, UDP sockets, files. See
    /// [`worker_held_resource_count`](Self::worker_held_resource_count) for
    /// in-flight ops that hold cloned OS handles, and
    /// [`pending_driver_call_count`](Self::pending_driver_call_count) for
    /// runtime-owned operations waiting for completion.
    pub const fn owned_resource_count(&self) -> usize {
        self.owned_resource_count
    }

    /// Worker-held resources parked inside in-flight lane work.
    ///
    /// Each TLS accept/handshake/read/write/close keeps cloned listener and
    /// stream `Arc`s alive on the worker thread for the duration of the
    /// operation. Each running process call holds a `std::process::Child`.
    /// These do not appear in [`owned_resource_count`](Self::owned_resource_count)
    /// because they are not represented by a runtime table id, and they may
    /// outlive the call's table id when the table id is dropped first.
    pub const fn worker_held_resource_count(&self) -> usize {
        self.worker_held_resource_count
    }

    /// Runtime-owned operations waiting for completion.
    ///
    /// Counts every pending driver call regardless of lane: TCP/file/UDP
    /// reads and writes, TLS ops, DNS lookups, storage jobs, process calls,
    /// signal waits, and timers. A TLS or process op contributes to both
    /// this count and [`worker_held_resource_count`](Self::worker_held_resource_count);
    /// a DNS lookup contributes only here.
    pub const fn pending_driver_call_count(&self) -> usize {
        self.pending_driver_call_count
    }
}

/// Snapshot of one live source-to-target shard transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRemoteQueueReport {
    pub(crate) source: ShardId,
    pub(crate) target: ShardId,
    pub(crate) queue: LiveQueueReport,
}

impl LiveRemoteQueueReport {
    /// Source shard for this local transport.
    pub const fn source(&self) -> ShardId {
        self.source
    }

    /// Target shard for this local transport.
    pub const fn target(&self) -> ShardId {
        self.target
    }

    /// Visible bounded transport pressure.
    pub const fn queue(&self) -> &LiveQueueReport {
        &self.queue
    }
}

/// Snapshot of one local Tina live topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTopologyReport {
    shards: Vec<LiveShardReport>,
    remote_queues: Vec<LiveRemoteQueueReport>,
}

impl LiveTopologyReport {
    pub(crate) fn single(shard: LiveShardReport) -> Self {
        Self {
            shards: vec![shard],
            remote_queues: Vec::new(),
        }
    }

    pub(crate) fn new(
        shards: Vec<LiveShardReport>,
        remote_queues: Vec<LiveRemoteQueueReport>,
    ) -> Self {
        Self {
            shards,
            remote_queues,
        }
    }

    /// Shard reports in stable shard order.
    pub fn shards(&self) -> &[LiveShardReport] {
        &self.shards
    }

    /// Remote queue reports in stable source/target order.
    pub fn remote_queues(&self) -> &[LiveRemoteQueueReport] {
        &self.remote_queues
    }

    /// Finds one shard report.
    pub fn shard(&self, shard: ShardId) -> Option<&LiveShardReport> {
        self.shards.iter().find(|report| report.shard == shard)
    }
}
