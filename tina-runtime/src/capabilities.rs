//! Driver-runtime capability vocabulary: what a Tina-shaped backend
//! must, must-not, may, or doesn't claim to provide.

use crate::driver::{
    DEFAULT_DNS_LANE_CAPACITY, DEFAULT_PROCESS_LANE_CAPACITY, DEFAULT_SIGNAL_CAPACITY,
    DEFAULT_TLS_LANE_CAPACITY,
};
use crate::persistence::{LOCAL_PERSISTENCE_SUPPORT, PersistenceSupportLevel};

/// Requirement level for Tina's driver-runtime contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverRuntimeRequirement {
    /// A Tina-shaped driver runtime must provide this.
    Required,
    /// A Tina-shaped driver runtime must not provide this behind Tina's back.
    Forbidden,
    /// This is useful for some backend implementations but not part of the
    /// core Tina contract.
    BackendSpecific,
    /// Tina makes no claim for this capability.
    NotClaimed,
}

/// The substrate contract Tina wants underneath a shard-local runner.
///
/// This is not a claim that Tina is a general Rust async runtime. It names the
/// smaller thing Tina needs: completion-shaped I/O that the Tina runner owns,
/// advances, cancels, and can simulate deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TinaDriverRuntimeContract {
    /// I/O completes by writing into caller-owned completion state.
    pub completion_based_io: DriverRuntimeRequirement,
    /// Runtime ingress and cross-thread commands stay bounded.
    pub bounded_runtime_commands: DriverRuntimeRequirement,
    /// Pending work can be explicitly canceled by runtime-owned call id.
    pub explicit_cancellation: DriverRuntimeRequirement,
    /// Shutdown owns the driver lifecycle and proves completion storage release.
    pub owned_shutdown: DriverRuntimeRequirement,
    /// Driver progress is advanced by the Tina runner, not hidden tasks.
    pub explicit_progress: DriverRuntimeRequirement,
    /// A deterministic simulated backend exists for DST and replay.
    pub deterministic_simulation: DriverRuntimeRequirement,
    /// Hidden executor tasks inside the driver are forbidden.
    pub hidden_executor_tasks: DriverRuntimeRequirement,
    /// A general async/futures executor is outside this contract.
    pub general_async_executor: DriverRuntimeRequirement,
}

/// Tina's current driver-runtime contract target.
pub const TINA_DRIVER_RUNTIME_CONTRACT: TinaDriverRuntimeContract = TinaDriverRuntimeContract {
    completion_based_io: DriverRuntimeRequirement::Required,
    bounded_runtime_commands: DriverRuntimeRequirement::Required,
    explicit_cancellation: DriverRuntimeRequirement::Required,
    owned_shutdown: DriverRuntimeRequirement::Required,
    explicit_progress: DriverRuntimeRequirement::Required,
    deterministic_simulation: DriverRuntimeRequirement::Required,
    hidden_executor_tasks: DriverRuntimeRequirement::Forbidden,
    general_async_executor: DriverRuntimeRequirement::NotClaimed,
};

/// Support status for one runtime-owned resource family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceSupport {
    /// Tina can execute this resource family on the current live runtime.
    Supported,
    /// Tina cannot honestly execute this resource family on the current live runtime.
    Unsupported,
    /// Tina can model this resource family in the deterministic simulator only.
    SimulatedOnly,
    /// Tina expects an explicit user adapter or service isolate for this family.
    AdapterOnly,
}

/// Execution shape for one resource family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceExecutionShape {
    /// Small runtime bookkeeping completes inline.
    Inline,
    /// Work completes through caller-owned completion slots.
    CompletionBacked,
    /// Nonblocking resource work is polled by the Tina driver step.
    PollBacked,
    /// Blocking work runs on a bounded named lane, away from shard handlers.
    LaneBackedBlocking,
    /// Work is delegated to an explicit user adapter or service isolate.
    ExternalAdapter,
    /// No execution shape exists because the resource is unsupported.
    NotApplicable,
}

/// Cancellation shape for one resource family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationSupport {
    /// Accepted work can be canceled before it starts.
    CancelableBeforeStart,
    /// Started work may finish, but its completion is tombstoned.
    TombstonedAfterStart,
    /// Cancellation requires closing the owned resource.
    ResourceCloseOnly,
    /// Tina cannot cancel this resource family on this runtime.
    NotCancelable,
    /// No cancellation shape exists because the resource is unsupported.
    NotApplicable,
}

/// Shutdown shape for one resource family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSupport {
    /// Shutdown drains pending work to a settled terminal state.
    Drained,
    /// Shutdown cancels pending work.
    Canceled,
    /// Shutdown tombstones late completions.
    Tombstoned,
    /// Shutdown cannot manage this resource family.
    Unsupported,
    /// No shutdown shape exists because the resource is unsupported.
    NotApplicable,
}

/// Capability report for one runtime-owned resource family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceCapability {
    support: ResourceSupport,
    execution: ResourceExecutionShape,
    cancellation: CancellationSupport,
    shutdown: ShutdownSupport,
    capacity: Option<usize>,
}

impl ResourceCapability {
    /// Creates one capability row.
    pub const fn new(
        support: ResourceSupport,
        execution: ResourceExecutionShape,
        cancellation: CancellationSupport,
        shutdown: ShutdownSupport,
        capacity: Option<usize>,
    ) -> Self {
        Self {
            support,
            execution,
            cancellation,
            shutdown,
            capacity,
        }
    }

    /// Resource support status.
    pub const fn support(&self) -> ResourceSupport {
        self.support
    }

    /// Resource execution shape.
    pub const fn execution(&self) -> ResourceExecutionShape {
        self.execution
    }

    /// Resource cancellation shape.
    pub const fn cancellation(&self) -> CancellationSupport {
        self.cancellation
    }

    /// Resource shutdown shape.
    pub const fn shutdown(&self) -> ShutdownSupport {
        self.shutdown
    }

    /// Configured bounded capacity, when this resource owns a bounded lane.
    pub const fn capacity(&self) -> Option<usize> {
        self.capacity
    }
}

/// Durability capability details for local persistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurabilityCapability {
    /// Whether snapshot commits write a temp file before rename.
    pub temp_write_before_rename: PersistenceSupportLevel,
    /// Whether snapshot commit rename replacement is claimed on this platform.
    pub rename_commit: PersistenceSupportLevel,
    /// Whether data file fsync is claimed on this platform.
    pub file_fsync: PersistenceSupportLevel,
    /// Whether parent-directory fsync after rename is claimed on this platform.
    pub directory_fsync_after_rename: PersistenceSupportLevel,
    /// Whether commit-uncertain is a possible visible outcome.
    pub commit_uncertain_possible: bool,
    /// Whether journal replay validates checksums.
    pub checksum_validation: PersistenceSupportLevel,
    /// Whether truncated journal tails are visible warnings.
    pub truncated_tail_warning: PersistenceSupportLevel,
}

impl DurabilityCapability {
    const fn local() -> Self {
        Self {
            temp_write_before_rename: LOCAL_PERSISTENCE_SUPPORT.temp_write_before_rename,
            rename_commit: LOCAL_PERSISTENCE_SUPPORT.rename_commit,
            file_fsync: LOCAL_PERSISTENCE_SUPPORT.file_fsync,
            directory_fsync_after_rename: LOCAL_PERSISTENCE_SUPPORT.directory_fsync_after_rename,
            commit_uncertain_possible: true,
            checksum_validation: LOCAL_PERSISTENCE_SUPPORT.checksum_validation,
            truncated_tail_warning: LOCAL_PERSISTENCE_SUPPORT.truncated_tail_warning,
        }
    }
}

/// Structured live-runtime capability table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    /// Runtime-owned timer support.
    pub timers: ResourceCapability,
    /// Runtime-owned TCP support.
    pub tcp: ResourceCapability,
    /// Runtime-owned local file support.
    pub local_file: ResourceCapability,
    /// Runtime-owned local persistence support.
    pub local_persistence: ResourceCapability,
    /// Shared storage lane used by blocking storage and persistence work.
    pub storage_lane: ResourceCapability,
    /// Runtime-owned DNS support.
    pub dns: ResourceCapability,
    /// Runtime-owned UDP support.
    pub udp: ResourceCapability,
    /// Runtime-owned TLS support.
    pub tls: ResourceCapability,
    /// Runtime-owned process support.
    pub process: ResourceCapability,
    /// Runtime-owned signal support.
    pub signal: ResourceCapability,
    /// Platform durability support details.
    pub durability: DurabilityCapability,
}

impl RuntimeCapabilities {
    /// Returns the current live [`crate::ThreadedRuntime`] capability table.
    pub const fn threaded(storage_lane_capacity: usize) -> Self {
        Self::threaded_with_capacities(
            storage_lane_capacity,
            DEFAULT_DNS_LANE_CAPACITY,
            DEFAULT_TLS_LANE_CAPACITY,
            DEFAULT_PROCESS_LANE_CAPACITY,
            DEFAULT_SIGNAL_CAPACITY,
        )
    }

    /// Returns the current live capability table with explicit bounded lane capacities.
    pub const fn threaded_with_capacities(
        storage_lane_capacity: usize,
        dns_lane_capacity: usize,
        tls_lane_capacity: usize,
        process_lane_capacity: usize,
        signal_capacity: usize,
    ) -> Self {
        Self {
            timers: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::Inline,
                CancellationSupport::CancelableBeforeStart,
                ShutdownSupport::Canceled,
                None,
            ),
            tcp: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::CompletionBacked,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                None,
            ),
            local_file: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::CompletionBacked,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                None,
            ),
            local_persistence: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::LaneBackedBlocking,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(storage_lane_capacity),
            ),
            storage_lane: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::LaneBackedBlocking,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(storage_lane_capacity),
            ),
            dns: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::LaneBackedBlocking,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(dns_lane_capacity),
            ),
            udp: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::PollBacked,
                CancellationSupport::CancelableBeforeStart,
                ShutdownSupport::Canceled,
                None,
            ),
            tls: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::LaneBackedBlocking,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(tls_lane_capacity),
            ),
            process: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::LaneBackedBlocking,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(process_lane_capacity),
            ),
            signal: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::PollBacked,
                CancellationSupport::CancelableBeforeStart,
                ShutdownSupport::Drained,
                Some(signal_capacity),
            ),
            durability: DurabilityCapability::local(),
        }
    }
}
