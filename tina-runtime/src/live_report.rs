//! Live observability reports: per-shard, per-queue, and topology
//! snapshots that user code reads from a live runtime.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tina::ShardId;

use crate::affinity::AffinityOutcome;
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

/// Startup facts the worker thread publishes once, under a single lock, so a
/// report that shows a worker thread id also shows that worker's final pin
/// outcome (the pin runs before the thread id is recorded).
#[derive(Debug)]
struct WorkerStartup {
    thread_id: Option<String>,
    affinity_status: AffinityStatus,
    observed_core: Option<usize>,
}

#[derive(Debug)]
pub(crate) struct LiveShardMetrics {
    shard: ShardId,
    worker_name: Option<String>,
    configured_core: Option<usize>,
    startup: Mutex<WorkerStartup>,
    preallocation: PreallocationConfig,
    pub(crate) config: ThreadedRuntimeConfig,
    state: AtomicU8,
    pub(crate) ingress: LiveQueueMetrics,
    storage_lane: LiveQueueMetrics,
    trace_retention: TraceRetention,
    owned_resource_count: AtomicUsize,
    worker_held_resource_count: AtomicUsize,
    pending_driver_call_count: AtomicUsize,
    /// Times the worker returned from a blocking park. A fully idle worker
    /// blocks on the kernel and so leaves this flat; the count rises only when a
    /// real wake source fires (I/O readiness, a deadline, or a host command).
    /// Used by the idle-CPU proof to show a quiet worker makes ~0 wakeups.
    park_wakeups: AtomicU64,
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
            configured_core: config.configured_core,
            startup: Mutex::new(WorkerStartup {
                thread_id: None,
                affinity_status: pre_start_affinity_status(config.configured_core),
                observed_core: None,
            }),
            preallocation: config.preallocation,
            config,
            state: AtomicU8::new(LiveShardState::RUNNING),
            ingress: LiveQueueMetrics::new(config.command_capacity),
            storage_lane: LiveQueueMetrics::new(config.storage_lane_capacity),
            trace_retention: config.trace_retention,
            owned_resource_count: AtomicUsize::new(0),
            worker_held_resource_count: AtomicUsize::new(0),
            pending_driver_call_count: AtomicUsize::new(0),
            park_wakeups: AtomicU64::new(0),
        }
    }

    /// Records one return from a blocking park.
    pub(crate) fn record_park_wakeup(&self) {
        self.park_wakeups.fetch_add(1, Ordering::Relaxed);
    }

    /// Total blocking-park wakeups observed so far.
    pub(crate) fn park_wakeups(&self) -> u64 {
        self.park_wakeups.load(Ordering::Relaxed)
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

    /// Records the worker's thread id and its proven pin outcome together, so
    /// any report that names the worker thread also carries that worker's final
    /// affinity status and observed core.
    pub(crate) fn publish_worker_start(&self, thread_id: String, affinity: AffinityOutcome) {
        let mut startup = self.startup.lock().expect("worker startup lock poisoned");
        startup.thread_id = Some(thread_id);
        startup.affinity_status = affinity.status;
        startup.observed_core = affinity.observed_core;
    }

    pub(crate) fn report(&self) -> LiveShardReport {
        self.report_with_trace_dropped(None)
    }

    pub(crate) fn report_with_trace_dropped(&self, trace_dropped: Option<u64>) -> LiveShardReport {
        let (worker_thread_id, affinity_status, observed_core) = {
            let startup = self.startup.lock().expect("worker startup lock poisoned");
            (
                startup.thread_id.clone(),
                startup.affinity_status.clone(),
                startup.observed_core,
            )
        };
        LiveShardReport {
            shard: self.shard,
            worker_name: self.worker_name.clone(),
            worker_thread_id,
            configured_core: self.configured_core,
            observed_core,
            affinity_status,
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
            trace_dropped,
            owned_resource_count: self.owned_resource_count.load(Ordering::Acquire),
            worker_held_resource_count: self.worker_held_resource_count.load(Ordering::Acquire),
            pending_driver_call_count: self.pending_driver_call_count.load(Ordering::Acquire),
        }
    }
}

/// Status this metrics block reports before the worker thread has started and
/// published its proven outcome. Authoritative affinity is only known once the
/// worker runs the pin; until then a requested core makes no claim
/// (`Unsupported`), and `None` is simply `NotRequested`.
fn pre_start_affinity_status(configured_core: Option<usize>) -> AffinityStatus {
    match configured_core {
        None => AffinityStatus::NotRequested,
        Some(_) => AffinityStatus::Unsupported,
    }
}

/// Shard-worker affinity state.
///
/// Authoritative once the worker thread has started (its
/// [`worker_thread_id`](LiveShardReport::worker_thread_id) is present): the pin
/// runs and publishes its outcome before the worker records its thread id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AffinityStatus {
    /// No core affinity was requested (`configured_core` was `None`); no
    /// affinity syscall was made.
    NotRequested,
    /// The platform performed a real hard pin (Linux `sched_setaffinity`) and
    /// the worker observed itself running on the requested core.
    Applied,
    /// The platform offers no hard pin (e.g. macOS exposes only affinity
    /// hints); the worker runs unpinned.
    Unsupported,
    /// A pin was requested but could not be applied — for example the requested
    /// core is not in the process's allowed affinity mask. The string carries
    /// the reason; the worker runs unpinned rather than mis-pinning.
    Failed(String),
    /// Reserved for a future explicit intent-only mode. **Not produced by
    /// `configured_core`**, which now performs a real pin where the platform
    /// can (`Applied`) and reports `Unsupported`/`Failed` otherwise. Kept as a
    /// typed slot in case an advisory-only knob is added; no path produces it
    /// today.
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

    /// Configured TLS lane capacity. Each in-flight TLS operation owns one
    /// worker thread up to this cap; live depth/accept/reject counters are
    /// not measured for this lane today.
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
