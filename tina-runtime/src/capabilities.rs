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
    /// Runtime-owned local persistence support. On the live runtime the
    /// durability ops (open/pread/pwrite/fsync/size) ride the per-shard
    /// Betelgeuse completion rail, hence `CompletionBacked`. The few ops
    /// Betelgeuse lacks are reported separately on [`Self::storage_metadata_fallback`].
    pub local_persistence: ResourceCapability,
    /// Shared storage lane that bounds total accepted pending storage work.
    pub storage_lane: ResourceCapability,
    /// The thin off-shard fallback worker for the storage ops Betelgeuse has
    /// no opcode for: rename, remove, readdir, and metadata (plus internal
    /// recursive directory creation and torn-tail truncation). Named
    /// explicitly so the report does not imply the whole storage family rides
    /// the reactor.
    pub storage_metadata_fallback: ResourceCapability,
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
    /// platforms (live substrate-backed lane riding the per-shard Betelgeuse
    /// completion loop, like TCP); `Unsupported` elsewhere — the capability
    /// stays named rather than cfg-silently dropped.
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
                ResourceExecutionShape::CompletionBacked,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(storage_lane_capacity),
            ),
            storage_lane: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::CompletionBacked,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(storage_lane_capacity),
            ),
            storage_metadata_fallback: ResourceCapability::new(
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
            // TLS now rides the Betelgeuse TCP rail (rustls sans-I/O on the
            // shard thread), not a blocking worker lane. `tls_lane_capacity`
            // stays as the shard-total cap on in-flight TLS ops.
            tls: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::CompletionBacked,
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
/// On Unix, the live driver runs a completion-backed lane that rides the
/// per-shard Betelgeuse loop — the same substrate as TCP/TLS, on the shard
/// thread, with no private worker. Started work is tombstoned on cancel and
/// shutdown, exactly like TCP. On non-Unix there is no backend, so the
/// capability is reported `Unsupported` with `NotApplicable` shapes —
/// callers see a typed capability, not a cfg-silent gap.
#[cfg(unix)]
const UNIX_RAIL_CAPABILITY: ResourceCapability = ResourceCapability::new(
    ResourceSupport::Supported,
    ResourceExecutionShape::CompletionBacked,
    CancellationSupport::TombstonedAfterStart,
    ShutdownSupport::Tombstoned,
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

/// Substrate posture for the Unix-domain rail on the current platform: it
/// rides the Betelgeuse completion substrate on Unix, and there is no live
/// backend off Unix.
#[cfg(unix)]
const UNIX_RAIL_CLASS: RailClass = RailClass::CompletionBacked;
#[cfg(not(unix))]
const UNIX_RAIL_CLASS: RailClass = RailClass::Unsupported;

/// Why the storage metadata fallback stays a bounded off-shard worker.
const STORAGE_FALLBACK_JUSTIFICATION: &str = "Narrow off-shard worker for the storage ops Betelgeuse has no opcode for: \
     rename, remove, readdir, and metadata (plus internal recursive directory \
     creation and torn-tail truncation). Live read/write/fsync/size/mkdir ride \
     the completion substrate; this is not a general storage lane.";

/// Why DNS stays a bounded blocking lane rather than a substrate opcode.
const DNS_LANE_JUSTIFICATION: &str = "Platform getaddrinfo/resolver behavior (hosts file, nsswitch, mDNS, search \
     domains) has no portable completion opcode. Name resolution is a blocking \
     library call, not reactor I/O, so it runs on a bounded worker lane off the \
     shard thread with explicit capacity, cancellation, and shutdown drain.";

/// Why process spawn/wait stays a bounded blocking lane.
const PROCESS_LANE_JUSTIFICATION: &str = "fork/exec/wait/reap are OS process lifecycle, not reactor I/O, and have no \
     portable completion opcode. The lane stays a bounded blocking worker off \
     the shard thread; cancellation requests a kill and the shutdown drain \
     reports exactly what could not be reaped in budget.";

// -----------------------------------------------------------------------------
// RuntimeCapabilityReport — read-shaped capability discovery
// -----------------------------------------------------------------------------

/// Substrate posture for one runtime-owned rail.
///
/// Whether this rail rides the per-shard Betelgeuse completion substrate or is
/// a bounded blocking/fallback lane that stays off the substrate for a written
/// reason. Every Tina-owned rail must answer with exactly one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailClass {
    /// Rides the per-shard Betelgeuse completion substrate on the shard
    /// thread. No worker thread, no blocking syscall (TCP, TLS, local file,
    /// persistence, storage lane, Unix-domain sockets).
    CompletionBacked,
    /// Small runtime bookkeeping that completes inline on the shard thread
    /// with no I/O (timers).
    Inline,
    /// Nonblocking resource work polled by the Tina driver step on the shard
    /// thread — no worker thread, no blocking syscall (UDP, signal flags).
    PollBacked,
    /// A bounded off-shard worker for the *narrow* set of ops the substrate
    /// has no opcode for. Not a general lane: the justification names the
    /// exact ops (storage metadata fallback).
    FallbackWorker,
    /// A bounded blocking lane deliberately kept off the substrate, with a
    /// written reason: the work is OS lifecycle, not reactor I/O, and has no
    /// portable completion opcode (DNS resolver, process spawn/wait).
    JustifiedBlockingLane,
    /// Scripted by the deterministic simulator only; no live execution shape.
    SimulatorScripted,
    /// No live backend on this platform.
    Unsupported,
}

impl RailClass {
    /// Whether this class is a worker-thread or blocking lane that owes a
    /// written justification in the capability report.
    pub const fn requires_justification(self) -> bool {
        matches!(self, Self::FallbackWorker | Self::JustifiedBlockingLane)
    }

    fn word(self) -> &'static str {
        match self {
            Self::CompletionBacked => "completion_backed",
            Self::Inline => "inline",
            Self::PollBacked => "poll_backed",
            Self::FallbackWorker => "fallback_worker",
            Self::JustifiedBlockingLane => "justified_blocking_lane",
            Self::SimulatorScripted => "simulator_scripted",
            Self::Unsupported => "unsupported",
        }
    }
}

/// One rail's capability, paired with a stable name.
///
/// This is a faithful view over a [`ResourceCapability`] — it renames
/// nothing and invents nothing. The predicate helpers exist so callers,
/// dashboards, and extension authors can ask the plan's questions
/// ("is this supported? sim-backed? cancel-backed? drain-backed?")
/// against one stable vocabulary instead of matching four enums by hand.
///
/// [`RailClass`] answers the substrate posture question, and
/// [`Self::justification`] carries the written reason for any rail that is a
/// bounded blocking or fallback lane rather than substrate-backed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCapabilityRow {
    /// Stable rail name (e.g. `"tcp"`, `"local_persistence"`).
    pub name: &'static str,
    /// The underlying capability row.
    pub capability: ResourceCapability,
    /// Substrate posture: substrate-backed, fallback worker, justified
    /// blocking lane, simulator-scripted, or unsupported.
    pub class: RailClass,
    /// Written reason a rail stays a bounded blocking/fallback lane instead
    /// of riding the substrate. `Some` exactly when
    /// [`RailClass::requires_justification`] holds.
    pub justification: Option<&'static str>,
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

    /// This rail rides the per-shard Betelgeuse completion substrate.
    pub const fn is_completion_backed(&self) -> bool {
        matches!(self.class, RailClass::CompletionBacked)
    }

    /// This rail is a bounded off-shard worker or blocking lane that stays
    /// off the substrate — and therefore carries a written justification.
    pub const fn is_blocking_or_fallback_lane(&self) -> bool {
        self.class.requires_justification()
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
            "cap rail={} class={} support={} exec={} cancel={} shutdown={} capacity={}",
            self.name,
            self.class.word(),
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
                class: RailClass::Inline,
                justification: None,
            },
            RuntimeCapabilityRow {
                name: "tcp",
                capability: caps.tcp,
                class: RailClass::CompletionBacked,
                justification: None,
            },
            RuntimeCapabilityRow {
                name: "local_file",
                capability: caps.local_file,
                class: RailClass::CompletionBacked,
                justification: None,
            },
            RuntimeCapabilityRow {
                name: "local_persistence",
                capability: caps.local_persistence,
                class: RailClass::CompletionBacked,
                justification: None,
            },
            RuntimeCapabilityRow {
                name: "storage_lane",
                capability: caps.storage_lane,
                class: RailClass::CompletionBacked,
                justification: None,
            },
            RuntimeCapabilityRow {
                name: "storage_metadata_fallback",
                capability: caps.storage_metadata_fallback,
                class: RailClass::FallbackWorker,
                justification: Some(STORAGE_FALLBACK_JUSTIFICATION),
            },
            RuntimeCapabilityRow {
                name: "dns",
                capability: caps.dns,
                class: RailClass::JustifiedBlockingLane,
                justification: Some(DNS_LANE_JUSTIFICATION),
            },
            RuntimeCapabilityRow {
                name: "udp",
                capability: caps.udp,
                class: RailClass::PollBacked,
                justification: None,
            },
            RuntimeCapabilityRow {
                name: "tls",
                capability: caps.tls,
                class: RailClass::CompletionBacked,
                justification: None,
            },
            RuntimeCapabilityRow {
                name: "process",
                capability: caps.process,
                class: RailClass::JustifiedBlockingLane,
                justification: Some(PROCESS_LANE_JUSTIFICATION),
            },
            RuntimeCapabilityRow {
                name: "signal",
                capability: caps.signal,
                class: RailClass::PollBacked,
                justification: None,
            },
            RuntimeCapabilityRow {
                name: "unix",
                capability: caps.unix,
                class: UNIX_RAIL_CLASS,
                justification: None,
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
        assert_eq!(report.rows().len(), 12);
        // tcp is supported, completion-backed, tombstoned on cancel and
        // shutdown — exactly what the table says.
        let tcp = report.rail("tcp").expect("tcp rail present");
        assert!(tcp.is_supported());
        assert!(tcp.is_cancel_backed());
        assert!(tcp.is_tombstoned());
        assert!(!tcp.is_drain_backed());
        assert!(tcp.is_completion_backed());
        // Live durability rides the Betelgeuse completion rail; only the ops
        // Betelgeuse lacks fall to the named fallback worker.
        let persistence = report
            .rail("local_persistence")
            .expect("local_persistence rail present");
        assert_eq!(
            persistence.capability.execution(),
            ResourceExecutionShape::CompletionBacked
        );
        let fallback = report
            .rail("storage_metadata_fallback")
            .expect("storage_metadata_fallback rail present");
        assert_eq!(
            fallback.capability.execution(),
            ResourceExecutionShape::LaneBackedBlocking
        );
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
        assert!(text.lines().count() == 12);
        assert!(text.contains("cap rail=tcp class=completion_backed support=supported"));
        assert!(text.contains("rail=local_persistence"));
        assert!(text.contains(
            "rail=local_persistence class=completion_backed support=supported exec=completion_backed"
        ));
        assert!(text.contains(
            "rail=storage_metadata_fallback class=fallback_worker support=supported exec=lane_backed_blocking"
        ));
        assert!(text.contains("rail=dns class=justified_blocking_lane"));
        assert!(text.contains("rail=process class=justified_blocking_lane"));
        assert!(text.contains("capacity=4096"));
    }

    #[test]
    fn every_rail_has_a_class_and_blocking_lanes_carry_a_justification() {
        let caps = RuntimeCapabilities::threaded(4096);
        let report = caps.report();
        for row in report.rows() {
            // A justification is present exactly for fallback/blocking lanes.
            assert_eq!(
                row.justification.is_some(),
                row.class.requires_justification(),
                "rail {} justification/class mismatch",
                row.name,
            );
            if let Some(reason) = row.justification {
                assert!(
                    reason.len() > 40,
                    "rail {} justification is too thin to be a real reason",
                    row.name,
                );
            }
        }
        // The substrate story: the socket/file/persistence rails ride the
        // completion substrate; only DNS and process remain justified blocking
        // lanes, and only the metadata fallback remains a fallback worker.
        for completion in [
            "tcp",
            "tls",
            "local_file",
            "local_persistence",
            "storage_lane",
        ] {
            assert!(
                report.rail(completion).unwrap().is_completion_backed(),
                "{completion} should be completion-backed",
            );
        }
        assert_eq!(
            report.rail("dns").unwrap().class,
            RailClass::JustifiedBlockingLane
        );
        assert_eq!(
            report.rail("process").unwrap().class,
            RailClass::JustifiedBlockingLane
        );
        assert_eq!(
            report.rail("storage_metadata_fallback").unwrap().class,
            RailClass::FallbackWorker
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_rail_rides_the_completion_substrate_on_unix() {
        let caps = RuntimeCapabilities::threaded(4096);
        let unix = *caps.report().rail("unix").expect("unix rail named");
        assert!(unix.is_supported());
        assert!(unix.is_completion_backed());
        assert!(unix.is_tombstoned());
        // No worker-lane justification: it rides the substrate.
        assert!(unix.justification.is_none());
        assert_eq!(
            unix.capability.execution(),
            ResourceExecutionShape::CompletionBacked
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn unix_rail_is_explicitly_unsupported_off_unix() {
        let caps = RuntimeCapabilities::threaded(4096);
        let unix = caps.report().rail("unix").expect("unix rail named");
        assert!(unix.is_unsupported());
    }
}
