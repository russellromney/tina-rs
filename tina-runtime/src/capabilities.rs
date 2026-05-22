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
    /// Runtime-owned Unix-domain socket support. `Supported` on Unix
    /// platforms (live OS-backed lane); `Unsupported` elsewhere — the
    /// capability stays named rather than cfg-silently dropped.
    pub unix: ResourceCapability,
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
            unix: UNIX_RAIL_CAPABILITY,
            durability: DurabilityCapability::local(),
        }
    }
}

/// Live Unix-domain rail capability for the current platform.
///
/// On Unix, the live driver runs an OS-backed lane that polls
/// non-blocking sockets. On non-Unix there is no backend, so the
/// capability is reported `Unsupported` with `NotApplicable` shapes —
/// callers see a typed capability, not a cfg-silent gap.
#[cfg(unix)]
const UNIX_RAIL_CAPABILITY: ResourceCapability = ResourceCapability::new(
    ResourceSupport::Supported,
    ResourceExecutionShape::PollBacked,
    CancellationSupport::ResourceCloseOnly,
    ShutdownSupport::Canceled,
    None,
);

#[cfg(not(unix))]
const UNIX_RAIL_CAPABILITY: ResourceCapability = ResourceCapability::new(
    ResourceSupport::Unsupported,
    ResourceExecutionShape::NotApplicable,
    CancellationSupport::NotApplicable,
    ShutdownSupport::NotApplicable,
    None,
);

// -----------------------------------------------------------------------------
// RuntimeCapabilityReport — read-shaped capability discovery
// -----------------------------------------------------------------------------

/// One rail's capability, paired with a stable name.
///
/// This is a faithful view over a [`ResourceCapability`] — it renames
/// nothing and invents nothing. The predicate helpers exist so callers,
/// dashboards, and extension authors can ask the plan's questions
/// ("is this supported? sim-backed? cancel-backed? drain-backed?")
/// against one stable vocabulary instead of matching four enums by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCapabilityRow {
    /// Stable rail name (e.g. `"tcp"`, `"local_persistence"`).
    pub name: &'static str,
    /// The underlying capability row.
    pub capability: ResourceCapability,
}

impl RuntimeCapabilityRow {
    /// Tina can execute this rail on the live runtime.
    pub const fn is_supported(&self) -> bool {
        matches!(self.capability.support(), ResourceSupport::Supported)
    }

    /// Tina cannot honestly execute this rail on the live runtime.
    pub const fn is_unsupported(&self) -> bool {
        matches!(self.capability.support(), ResourceSupport::Unsupported)
    }

    /// This rail exists only inside the deterministic simulator.
    pub const fn is_sim_only(&self) -> bool {
        matches!(self.capability.support(), ResourceSupport::SimulatedOnly)
    }

    /// This rail expects an explicit user adapter or service isolate.
    pub const fn is_adapter_only(&self) -> bool {
        matches!(self.capability.support(), ResourceSupport::AdapterOnly)
    }

    /// Pending work on this rail can be canceled (before start or
    /// tombstoned after start).
    pub const fn is_cancel_backed(&self) -> bool {
        matches!(
            self.capability.cancellation(),
            CancellationSupport::CancelableBeforeStart | CancellationSupport::TombstonedAfterStart
        )
    }

    /// Started work on this rail is tombstoned (cancel or shutdown lets a
    /// late completion land as a recorded tombstone, not a fresh effect).
    pub const fn is_tombstoned(&self) -> bool {
        matches!(
            self.capability.cancellation(),
            CancellationSupport::TombstonedAfterStart
        ) || matches!(self.capability.shutdown(), ShutdownSupport::Tombstoned)
    }

    /// Shutdown drains this rail's pending work to a settled terminal state.
    pub const fn is_drain_backed(&self) -> bool {
        matches!(self.capability.shutdown(), ShutdownSupport::Drained)
    }

    fn support_word(&self) -> &'static str {
        match self.capability.support() {
            ResourceSupport::Supported => "supported",
            ResourceSupport::Unsupported => "unsupported",
            ResourceSupport::SimulatedOnly => "simulated_only",
            ResourceSupport::AdapterOnly => "adapter_only",
        }
    }

    fn execution_word(&self) -> &'static str {
        match self.capability.execution() {
            ResourceExecutionShape::Inline => "inline",
            ResourceExecutionShape::CompletionBacked => "completion_backed",
            ResourceExecutionShape::PollBacked => "poll_backed",
            ResourceExecutionShape::LaneBackedBlocking => "lane_backed_blocking",
            ResourceExecutionShape::ExternalAdapter => "external_adapter",
            ResourceExecutionShape::NotApplicable => "n/a",
        }
    }

    fn cancellation_word(&self) -> &'static str {
        match self.capability.cancellation() {
            CancellationSupport::CancelableBeforeStart => "cancelable_before_start",
            CancellationSupport::TombstonedAfterStart => "tombstoned_after_start",
            CancellationSupport::ResourceCloseOnly => "resource_close_only",
            CancellationSupport::NotCancelable => "not_cancelable",
            CancellationSupport::NotApplicable => "n/a",
        }
    }

    fn shutdown_word(&self) -> &'static str {
        match self.capability.shutdown() {
            ShutdownSupport::Drained => "drained",
            ShutdownSupport::Canceled => "canceled",
            ShutdownSupport::Tombstoned => "tombstoned",
            ShutdownSupport::Unsupported => "unsupported",
            ShutdownSupport::NotApplicable => "n/a",
        }
    }

    /// One grep-friendly discovery line for this rail.
    pub fn discovery_line(&self) -> String {
        let cap = match self.capability.capacity() {
            Some(c) => c.to_string(),
            None => "-".to_string(),
        };
        format!(
            "cap rail={} support={} exec={} cancel={} shutdown={} capacity={}",
            self.name,
            self.support_word(),
            self.execution_word(),
            self.cancellation_word(),
            self.shutdown_word(),
            cap,
        )
    }
}

/// Read-shaped capability report over a [`RuntimeCapabilities`] table.
///
/// `RuntimeCapabilities` is the structured truth; this report is the
/// grep-friendly, predicate-friendly rendering of it. It names every
/// runtime-owned rail and says, explicitly, whether each is supported,
/// unsupported, simulated-only, cancel-backed, tombstoned, or
/// drain-backed. Extension authors use it to discover what the runtime
/// they were handed can actually do — without reaching into private
/// runtime state.
///
/// The report renames nothing: `simulated_only` is the existing
/// [`ResourceSupport::SimulatedOnly`], `tombstoned` is the existing
/// tombstone shape, and so on. It is a view, not a second source of
/// truth.
#[derive(Debug, Clone)]
pub struct RuntimeCapabilityReport {
    rows: Vec<RuntimeCapabilityRow>,
}

impl RuntimeCapabilityReport {
    /// Build the report from a capability table.
    pub fn from_capabilities(caps: &RuntimeCapabilities) -> Self {
        let rows = vec![
            RuntimeCapabilityRow {
                name: "timers",
                capability: caps.timers,
            },
            RuntimeCapabilityRow {
                name: "tcp",
                capability: caps.tcp,
            },
            RuntimeCapabilityRow {
                name: "local_file",
                capability: caps.local_file,
            },
            RuntimeCapabilityRow {
                name: "local_persistence",
                capability: caps.local_persistence,
            },
            RuntimeCapabilityRow {
                name: "storage_lane",
                capability: caps.storage_lane,
            },
            RuntimeCapabilityRow {
                name: "dns",
                capability: caps.dns,
            },
            RuntimeCapabilityRow {
                name: "udp",
                capability: caps.udp,
            },
            RuntimeCapabilityRow {
                name: "tls",
                capability: caps.tls,
            },
            RuntimeCapabilityRow {
                name: "process",
                capability: caps.process,
            },
            RuntimeCapabilityRow {
                name: "signal",
                capability: caps.signal,
            },
            RuntimeCapabilityRow {
                name: "unix",
                capability: caps.unix,
            },
        ];
        Self { rows }
    }

    /// All rail rows, in a stable order.
    pub fn rows(&self) -> &[RuntimeCapabilityRow] {
        &self.rows
    }

    /// Look up one rail by name.
    pub fn rail(&self, name: &str) -> Option<&RuntimeCapabilityRow> {
        self.rows.iter().find(|r| r.name == name)
    }

    /// One discovery line per rail, newline-joined.
    pub fn discovery_report(&self) -> String {
        let mut out = String::new();
        for row in &self.rows {
            out.push_str(&row.discovery_line());
            out.push('\n');
        }
        out
    }
}

impl RuntimeCapabilities {
    /// Render this table as a read-shaped [`RuntimeCapabilityReport`].
    pub fn report(&self) -> RuntimeCapabilityReport {
        RuntimeCapabilityReport::from_capabilities(self)
    }
}

#[cfg(test)]
mod capability_report_tests {
    use super::*;

    #[test]
    fn report_names_every_rail_and_renames_nothing() {
        let caps = RuntimeCapabilities::threaded(4096);
        let report = caps.report();
        assert_eq!(report.rows().len(), 11);
        // tcp is supported, completion-backed, tombstoned on cancel and
        // shutdown — exactly what the table says.
        let tcp = report.rail("tcp").expect("tcp rail present");
        assert!(tcp.is_supported());
        assert!(tcp.is_cancel_backed());
        assert!(tcp.is_tombstoned());
        assert!(!tcp.is_drain_backed());
        // timers cancel before start and drain on... they cancel on
        // shutdown, so not drain-backed but cancel-backed.
        let timers = report.rail("timers").expect("timers rail present");
        assert!(timers.is_cancel_backed());
        // signal drains on shutdown.
        let signal = report.rail("signal").expect("signal rail present");
        assert!(signal.is_drain_backed());
    }

    #[test]
    fn discovery_report_is_grep_friendly() {
        let caps = RuntimeCapabilities::threaded(4096);
        let text = caps.report().discovery_report();
        assert!(text.lines().count() == 11);
        assert!(text.contains("cap rail=tcp support=supported"));
        assert!(text.contains("rail=local_persistence"));
        assert!(text.contains("capacity=4096"));
    }

    #[cfg(not(unix))]
    #[test]
    fn unix_rail_is_explicitly_unsupported_off_unix() {
        let caps = RuntimeCapabilities::threaded(4096);
        let unix = caps.report().rail("unix").expect("unix rail named");
        assert!(unix.is_unsupported());
    }
}
