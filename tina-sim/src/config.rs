//! Public configuration, scripted resource configs, replay artifacts, and
//! checker surface extracted from lib.rs.
//!
//! This module groups the public data types that callers configure or observe
//! around a `Simulator` run: `SimulatorConfig`, `FaultConfig`, the scripted
//! TCP/UDP/DNS/TLS/signal/process/storage config families, `DurableImage`,
//! `ReplayArtifact`, `MultiShardReplayArtifact`, `ObservedPeerOutput`,
//! `CheckerDecision`, `Checker`, and `CheckerFailure`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tina_runtime::RuntimeEvent;

use crate::MultiShardSimulatorConfig;

/// Deterministic simulator configuration for one run.
///
/// Construct with functional-update syntax over [`Default`]
/// (`SimulatorConfig { seed, ..Default::default() }`) so that adding a
/// new rail config field stays a non-breaking change for callers. New
/// fields here must carry a `Default`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SimulatorConfig {
    /// Replay seed that drives the simulator's narrow seeded perturbation
    /// surface.
    pub seed: u64,

    /// Seeded fault configuration for this run.
    pub faults: FaultConfig,

    /// Scripted TCP configuration for this run.
    pub tcp: ScriptedTcpConfig,

    /// Scripted UDP configuration for this run.
    pub udp: ScriptedUdpConfig,

    /// Scripted DNS configuration for this run.
    pub dns: ScriptedDnsConfig,

    /// Scripted TLS configuration for this run.
    pub tls: ScriptedTlsConfig,

    /// Scripted signal configuration for this run.
    pub signal: ScriptedSignalConfig,

    /// Scripted process configuration for this run.
    pub process: ScriptedProcessConfig,

    /// Deterministic simulator storage limits and persistence fault injection.
    pub storage: ScriptedStorageFaultConfig,

    /// Default per-stream caps for Unix-domain rails in the simulator.
    pub unix: UnixSimConfig,

    /// Reserve this many `IsolateId`s at simulator construction so a sim
    /// replay matches a live `ThreadedRuntime` that registers system
    /// isolates (e.g. its host-call dispatcher pool) at worker startup.
    ///
    /// The simulator never registers those system isolates itself — sim is
    /// the semantic oracle and host-call routing isn't part of its model —
    /// it just advances its `IsolateId` counter so user isolates get the
    /// same ids in both modes and the captured trace replays exactly.
    /// Default `0` keeps every existing test unaffected; live-replay
    /// runners set this to `tina_runtime::HOST_CALL_DISPATCHER_POOL_SIZE`.
    pub reserved_system_isolates: usize,
}

/// Per-stream caps applied to simulator Unix-domain rails.
///
/// The simulator pairs Unix streams dynamically (no scripted-peer
/// configuration): `unix_bind(path)` opens a listener and the next
/// `unix_connect(path)` builds a paired stream. These caps tell the
/// simulator how much inbound data a stream may buffer and how many
/// bytes one write call may accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnixSimConfig {
    /// Inbound buffer cap per stream (bytes).
    pub default_inbound_capacity: usize,
    /// Maximum bytes accepted by a single `unix_write` call. Models
    /// kernel-side short-writes so user state machines see partial
    /// progress identical to live behavior.
    pub default_write_cap: usize,
}

impl Default for UnixSimConfig {
    fn default() -> Self {
        Self {
            default_inbound_capacity: 64 * 1024,
            default_write_cap: 64 * 1024,
        }
    }
}

/// Deterministic durable file image captured by simulator replay.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DurableImage {
    pub(crate) files: BTreeMap<PathBuf, Vec<u8>>,
}

impl DurableImage {
    /// Creates an empty durable image.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces one durable file image.
    pub fn insert(&mut self, path: impl Into<PathBuf>, bytes: Vec<u8>) -> Option<Vec<u8>> {
        self.files.insert(path.into(), bytes)
    }

    /// Returns one durable file image by path.
    pub fn get(&self, path: impl AsRef<std::path::Path>) -> Option<&[u8]> {
        self.files.get(path.as_ref()).map(Vec::as_slice)
    }

    /// Returns all durable file bytes in deterministic path order.
    pub fn files(&self) -> &BTreeMap<PathBuf, Vec<u8>> {
        &self.files
    }
}

/// Captured replay data for one deterministic simulator run.
///
/// Replay means rerunning the same workload under the same simulator config
/// and reproducing the same event record, final virtual time, and checker
/// failure (when one occurred).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayArtifact {
    pub(crate) config: SimulatorConfig,
    pub(crate) final_time: Duration,
    pub(crate) event_record: Vec<RuntimeEvent>,
    pub(crate) checker_failure: Option<CheckerFailure>,
    pub(crate) observed_peer_output: Vec<ObservedPeerOutput>,
    pub(crate) durable_image: DurableImage,
}

impl ReplayArtifact {
    /// Returns the simulator config used for the original run.
    pub fn config(&self) -> &SimulatorConfig {
        &self.config
    }

    /// Returns the final virtual time reached by the run.
    pub const fn final_time(&self) -> Duration {
        self.final_time
    }

    /// Returns the deterministic event record captured for the run.
    pub fn event_record(&self) -> &[RuntimeEvent] {
        &self.event_record
    }

    /// Returns the optional checker failure captured for the run.
    pub fn checker_failure(&self) -> Option<&CheckerFailure> {
        self.checker_failure.as_ref()
    }

    /// Returns the peer-visible output captured during the run.
    pub fn observed_peer_output(&self) -> &[ObservedPeerOutput] {
        &self.observed_peer_output
    }

    /// Returns the deterministic durable image captured during the run.
    pub fn durable_image(&self) -> &DurableImage {
        &self.durable_image
    }
}

/// Captured replay data for one deterministic multi-shard simulator run.
///
/// Like [`ReplayArtifact`], this records one whole-run event record rather
/// than splitting replay state into per-shard artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiShardReplayArtifact {
    pub(crate) simulator_config: SimulatorConfig,
    pub(crate) multishard_config: MultiShardSimulatorConfig,
    pub(crate) final_time: Duration,
    pub(crate) event_record: Vec<RuntimeEvent>,
    pub(crate) checker_failure: Option<CheckerFailure>,
    pub(crate) observed_peer_output: Vec<ObservedPeerOutput>,
    pub(crate) durable_image: DurableImage,
}

impl MultiShardReplayArtifact {
    /// Returns the single-shard simulator config cloned onto each shard.
    pub fn simulator_config(&self) -> &SimulatorConfig {
        &self.simulator_config
    }

    /// Returns the multi-shard coordinator config used for the run.
    pub const fn multishard_config(&self) -> MultiShardSimulatorConfig {
        self.multishard_config
    }

    /// Returns the final shared virtual time reached by the run.
    pub const fn final_time(&self) -> Duration {
        self.final_time
    }

    /// Returns the deterministic whole-run event record captured for the run.
    pub fn event_record(&self) -> &[RuntimeEvent] {
        &self.event_record
    }

    /// Returns the optional checker failure captured for the run.
    pub fn checker_failure(&self) -> Option<&CheckerFailure> {
        self.checker_failure.as_ref()
    }

    /// Returns the peer-visible output captured during the run.
    pub fn observed_peer_output(&self) -> &[ObservedPeerOutput] {
        &self.observed_peer_output
    }

    /// Returns the deterministic durable image captured during the run.
    pub fn durable_image(&self) -> &DurableImage {
        &self.durable_image
    }
}

/// Seeded fault configuration for one simulator run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FaultConfig {
    /// Perturbation applied to local `Send` delivery.
    pub local_send: LocalSendFaultMode,
    /// Perturbation applied to timer wake delivery.
    pub timer_wake: FaultMode,
    /// Perturbation applied to ready TCP completions.
    pub tcp_completion: TcpCompletionFaultMode,
    /// Perturbation applied to the within-round isolate dispatch order.
    ///
    /// This is the seed's scheduler dimension: the order ready isolates
    /// handle their messages inside one `step()` round. The other three
    /// knobs shift completion/delivery *timing*; this one shifts the
    /// logical *interleaving* the fixed registration order otherwise
    /// pins. Default `None` keeps registration order so existing traces
    /// are byte-identical.
    pub scheduler: SchedulerFaultMode,
}

/// One seeded within-round dispatch-order perturbation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SchedulerFaultMode {
    /// Dispatch ready isolates in registration order (no perturbation).
    #[default]
    None,

    /// Deterministically permute the within-round dispatch order of the
    /// ready isolates using the run seed. Same `(seed, shard, step)` =>
    /// same permutation, so replay stays byte-identical. A zero seed
    /// still yields registration order.
    PermuteReadyOrder,
}

/// One narrow seeded local-send fault mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LocalSendFaultMode {
    /// No perturbation.
    #[default]
    None,

    /// Delay matching sends by a deterministic number of extra delivery
    /// rounds.
    DelayByRounds {
        /// One out of every `one_in` candidate sends is delayed.
        one_in: u64,

        /// Extra delivery rounds skipped before the message becomes visible.
        rounds: u64,
    },
}

/// One narrow seeded fault mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FaultMode {
    /// No perturbation.
    #[default]
    None,

    /// Delay matching events by a deterministic amount.
    DelayBy {
        /// One out of every `one_in` candidate events is delayed.
        one_in: u64,

        /// Extra virtual delay applied when the fault fires.
        by: Duration,
    },
}

/// One narrow seeded TCP-completion fault mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TcpCompletionFaultMode {
    /// No perturbation.
    #[default]
    None,

    /// Delay matching TCP completions by a deterministic number of extra
    /// simulator steps.
    DelayBySteps {
        /// One out of every `one_in` candidate completions is delayed.
        one_in: u64,

        /// Extra simulator steps skipped before the completion becomes visible.
        steps: u64,
    },

    /// Reorder ready TCP completions that tie in the same simulator step while
    /// still preserving per-resource FIFO.
    ReorderReady {
        /// One out of every `one_in` ready batches is reordered.
        one_in: u64,
    },
}

/// Scripted TCP configuration for one simulator run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedTcpConfig {
    /// Maximum number of async TCP completions that may be pending inside the
    /// simulator at once.
    pub pending_completion_capacity: usize,

    /// Listener scripts available to `TcpBind`.
    pub listeners: Vec<ScriptedListenerConfig>,
}

/// Scripted UDP configuration for one simulator run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedUdpConfig {
    /// Maximum number of async UDP completions that may be pending.
    pub pending_completion_capacity: usize,

    /// Socket scripts available to `UdpBind`.
    pub sockets: Vec<ScriptedUdpSocketConfig>,
}

/// Scripted UDP socket configuration for one bindable address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedUdpSocketConfig {
    /// The address isolates request in `CallInput::UdpBind`.
    pub bind_addr: SocketAddr,

    /// The local address returned through `CallOutput::UdpBound`.
    pub local_addr: SocketAddr,

    /// Maximum datagrams buffered before a pending receive consumes them.
    pub recv_capacity: usize,

    /// Scripted inbound datagrams.
    pub inbound_datagrams: Vec<ScriptedUdpDatagramConfig>,
}

/// Scripted inbound UDP datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedUdpDatagramConfig {
    /// First simulator step on which this datagram may be received.
    pub deliver_after_step: u64,

    /// Sender address reported to the isolate.
    pub peer_addr: SocketAddr,

    /// Datagram bytes.
    pub bytes: Vec<u8>,
}

/// Scripted DNS configuration for one simulator run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedDnsConfig {
    /// Maximum number of async DNS completions that may be pending.
    pub pending_completion_capacity: usize,

    /// Lookup scripts available to `DnsLookup`.
    pub lookups: Vec<ScriptedDnsLookupConfig>,
}

/// Scripted DNS lookup configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedDnsLookupConfig {
    /// Host name or address string.
    pub host: String,

    /// Service port.
    pub port: u16,

    /// First simulator step on which this lookup may complete.
    pub complete_after_step: u64,

    /// Scripted lookup outcome.
    pub result: ScriptedDnsResult,
}

/// Scripted DNS lookup outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedDnsResult {
    /// Lookup resolves to these addresses.
    Resolved(Vec<SocketAddr>),

    /// Lookup fails as a normal I/O error.
    Failed,

    /// Lookup exceeds its deadline.
    Timeout,
}

/// Scripted TLS configuration for one simulator run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedTlsConfig {
    /// Maximum number of async TLS completions that may be pending.
    pub pending_completion_capacity: usize,

    /// TLS connect scripts.
    pub connects: Vec<ScriptedTlsConnectConfig>,
}

/// Scripted TLS connect configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedTlsConnectConfig {
    /// Remote address.
    pub addr: SocketAddr,

    /// Server name.
    pub server_name: String,

    /// First simulator step on which the handshake may complete.
    pub complete_after_step: u64,

    /// Scripted connect outcome.
    pub result: ScriptedTlsConnectResult,
}

/// Scripted TLS connect outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedTlsConnectResult {
    /// Handshake succeeds and creates a logical TLS stream.
    Connected {
        /// Logical read outcomes for later `TlsRead` calls.
        reads: Vec<ScriptedTlsReadResult>,
        /// Logical write outcomes for later `TlsWrite` calls.
        writes: Vec<ScriptedTlsWriteResult>,
    },

    /// Handshake fails as a normal I/O error.
    Failed,

    /// Certificate validation fails.
    Certificate,

    /// Server name parsing/validation fails.
    Name,

    /// Handshake exceeds its deadline.
    Timeout,
}

/// Scripted TLS read outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedTlsReadResult {
    /// Read returns logical decrypted bytes.
    Bytes(Vec<u8>),
    /// Read observes peer close.
    Eof,
    /// Read fails as I/O.
    Failed,
    /// Read exceeds its deadline.
    Timeout,
}

/// Scripted TLS write outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedTlsWriteResult {
    /// Write accepts this many plaintext bytes.
    Wrote(usize),
    /// Write fails as I/O.
    Failed,
    /// Write exceeds its deadline.
    Timeout,
}

/// Scripted signal configuration for one simulator run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedSignalConfig {
    /// Maximum number of async signal completions that may be pending.
    pub pending_completion_capacity: usize,

    /// Signal events available to `SignalWait`.
    pub events: Vec<ScriptedSignalEventConfig>,
}

/// Scripted signal event configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedSignalEventConfig {
    /// Runtime-owned signal name.
    pub name: String,

    /// First simulator step on which this event may complete.
    pub deliver_after_step: u64,

    /// Scripted signal outcome.
    pub result: ScriptedSignalResult,
}

/// Scripted signal wait outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedSignalResult {
    /// Signal is delivered.
    Received,

    /// Signal wait fails as a normal I/O error.
    Failed,

    /// Signal wait exceeds its deadline.
    Timeout,
}

/// Scripted process configuration for one simulator run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedProcessConfig {
    /// Maximum number of async process completions that may be pending.
    pub pending_completion_capacity: usize,

    /// Process scripts available to `ProcessRun`.
    pub runs: Vec<ScriptedProcessRunConfig>,
}

/// Scripted process run configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedProcessRunConfig {
    /// Executable path or command name.
    pub command: String,

    /// Exact argument list.
    pub args: Vec<String>,

    /// First simulator step on which this run may complete.
    pub complete_after_step: u64,

    /// Scripted process outcome.
    pub result: ScriptedProcessResult,
}

/// Scripted process outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedProcessResult {
    /// Process exits with captured output.
    Exited {
        /// Portable exit code.
        code: Option<i32>,
        /// Captured stdout before request-level truncation.
        stdout: Vec<u8>,
        /// Captured stderr before request-level truncation.
        stderr: Vec<u8>,
    },

    /// Process spawn or wait fails as I/O.
    Failed,

    /// Process exceeds its timeout.
    Timeout,

    /// Kill/reap outcome is uncertain.
    KillUncertain,
}

/// Simulator-only deterministic storage limits and fault configuration.
///
/// These faults mutate the simulator durable image or completion result. They
/// are not claims about native filesystem crash behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScriptedStorageFaultConfig {
    /// Maximum bytes accepted by one simulated file write.
    pub file_write_cap: Option<usize>,

    /// Maximum length of one simulated file, in bytes.
    ///
    /// A write whose non-empty range would end past this boundary fails with
    /// [`tina_runtime::CallError::StorageFull`] without changing the durable
    /// image. Empty positional writes remain no-ops at every offset. The
    /// default is 64 MiB per file.
    pub max_file_bytes: u64,

    /// Fail the selected storage operation when it is a journal append.
    pub fail_journal_append_at: Option<u64>,

    /// Fail the selected storage operation when it is a snapshot commit.
    pub fail_snapshot_commit_at: Option<u64>,

    /// After the selected journal append writes bytes, truncate the committed
    /// tail so recovery reports a valid prefix warning.
    pub truncate_journal_tail_at: Option<u64>,

    /// After the selected journal append writes bytes, corrupt the committed
    /// record so recovery reports a corrupt record.
    pub corrupt_journal_record_at: Option<u64>,

    /// After the selected snapshot commit writes bytes, report
    /// `CommitUncertain` to model a completed rename with unproven final
    /// durability.
    pub commit_uncertain_snapshot_at: Option<u64>,
}

impl Default for ScriptedStorageFaultConfig {
    fn default() -> Self {
        Self {
            file_write_cap: None,
            max_file_bytes: 64 * 1024 * 1024,
            fail_journal_append_at: None,
            fail_snapshot_commit_at: None,
            truncate_journal_tail_at: None,
            corrupt_journal_record_at: None,
            commit_uncertain_snapshot_at: None,
        }
    }
}

impl Default for ScriptedTcpConfig {
    fn default() -> Self {
        Self {
            pending_completion_capacity: usize::MAX,
            listeners: Vec::new(),
        }
    }
}

impl Default for ScriptedUdpConfig {
    fn default() -> Self {
        Self {
            pending_completion_capacity: usize::MAX,
            sockets: Vec::new(),
        }
    }
}

impl Default for ScriptedDnsConfig {
    fn default() -> Self {
        Self {
            pending_completion_capacity: usize::MAX,
            lookups: Vec::new(),
        }
    }
}

impl Default for ScriptedTlsConfig {
    fn default() -> Self {
        Self {
            pending_completion_capacity: usize::MAX,
            connects: Vec::new(),
        }
    }
}

impl Default for ScriptedSignalConfig {
    fn default() -> Self {
        Self {
            pending_completion_capacity: usize::MAX,
            events: Vec::new(),
        }
    }
}

impl Default for ScriptedProcessConfig {
    fn default() -> Self {
        Self {
            pending_completion_capacity: usize::MAX,
            runs: Vec::new(),
        }
    }
}

/// Scripted listener configuration for one bindable address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedListenerConfig {
    /// The address isolates request in `CallInput::TcpBind`.
    pub bind_addr: SocketAddr,

    /// The local address returned through `CallOutput::TcpBound`.
    pub local_addr: SocketAddr,

    /// Maximum number of accepted-but-not-yet-consumed peers that may wait in
    /// the listener's ready queue.
    pub backlog_capacity: usize,

    /// Scripted peers that may arrive on this listener.
    pub peers: Vec<ScriptedPeerConfig>,
}

/// Scripted peer configuration for one simulated accepted connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedPeerConfig {
    /// The first simulator step on which this peer may become visible to a
    /// pending `TcpAccept`.
    pub accept_after_step: u64,

    /// The remote address returned through `CallOutput::TcpAccepted`.
    pub peer_addr: SocketAddr,

    /// Inbound byte chunks the peer will make readable to the isolate.
    pub inbound_chunks: Vec<Vec<u8>>,

    /// Explicit bound for the peer's readable-buffer bytes.
    pub inbound_capacity: usize,

    /// Optional extra cap applied to each read result after `max_len`.
    pub read_chunk_cap: Option<usize>,

    /// Maximum number of bytes one write completion may accept.
    pub write_cap: usize,

    /// Explicit bound for the bytes the peer will accept from the isolate.
    pub output_capacity: usize,
}

/// One captured peer-visible output stream from a simulated run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedPeerOutput {
    pub(crate) peer_addr: SocketAddr,
    pub(crate) bytes: Vec<u8>,
}

impl ObservedPeerOutput {
    /// Returns the peer address associated with this captured output.
    pub const fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Returns the bytes written to this simulated peer.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Decision returned by a simulator checker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckerDecision {
    /// Keep running.
    Continue,

    /// Halt the run with a structured reason.
    Fail(String),
}

/// Small user-facing checker surface for `tina-sim`.
pub trait Checker {
    /// Stable checker name recorded into replay artifacts.
    fn name(&self) -> &'static str;

    /// Observes one emitted runtime event and decides whether the run should
    /// continue.
    fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision;
}

/// Structured checker failure captured by the simulator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckerFailure {
    pub(crate) checker_name: &'static str,
    pub(crate) event_id: tina_runtime::EventId,
    pub(crate) reason: String,
}

impl CheckerFailure {
    /// Returns the checker name.
    pub const fn checker_name(&self) -> &'static str {
        self.checker_name
    }

    /// Returns the event that triggered the failure.
    pub const fn event_id(&self) -> tina_runtime::EventId {
        self.event_id
    }

    /// Returns the failure reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}
