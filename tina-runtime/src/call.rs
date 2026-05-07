//! Runtime-owned external call vocabulary for `tina-runtime`.
//!
//! `tina` only owns the [`Effect::Call`](tina::Effect::Call) slot and the
//! `Isolate::Call` associated type. The concrete request and result types
//! live here so the trait crate stays substrate-neutral. A future
//! Mariner runtime crate (a different completion-driven backend, a
//! deterministic simulator, …) can implement the same `Effect::Call`
//! slot with its own request/result vocabulary without touching `tina`.
//!
//! The first shipped call family is backed by Betelgeuse on nightly Rust and
//! now covers both:
//!
//! - TCP bind / accept / read / write / close
//! - one-shot relative sleep / timer wake
//!
//! Future verbs still extend [`CallInput`] / [`CallOutput`] in this crate,
//! not the `tina` trait boundary.
//!
//! ## Design constraints honored here
//!
//! - Resource ids ([`ListenerId`], [`StreamId`], [`CallId`]) are
//!   runtime-assigned monotonic counters. The runtime, not the OS, owns
//!   identity.
//! - No wall-clock time leaks into the call payload. The call family
//!   names relative timer duration, not `Instant`, so a deterministic
//!   simulator can implement virtual time without renegotiating the
//!   contract.
//! - Raw socket handles never escape the runtime. Isolate code only sees
//!   opaque ids inside its own message vocabulary.

use std::any::Any;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tina::{Address, AddressGeneration, IsolateId, ShardId};

type ErasedReply = Box<dyn Any>;
type ErasedCallOutcome = CallOutcome<ErasedReply>;
type IsolateCallTranslator<M> = Box<dyn FnOnce(ErasedCallOutcome) -> M>;
type ErasedIsolateCallTranslator = Box<dyn FnOnce(ErasedCallOutcome) -> Box<dyn Any>>;

/// Stable identifier for one runtime-issued call.
///
/// The runtime assigns `CallId`s in submission order, starting at `1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallId(u64);

impl CallId {
    /// Creates a call identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw call identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for a TCP listener resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListenerId(u64);

impl ListenerId {
    /// Creates a listener identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw listener identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for a TCP stream resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(u64);

impl StreamId {
    /// Creates a stream identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw stream identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for a UDP socket resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UdpSocketId(u64);

impl UdpSocketId {
    /// Creates a UDP socket identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw UDP socket identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for a TLS stream resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TlsStreamId(u64);

impl TlsStreamId {
    /// Creates a TLS stream identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw TLS stream identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for a TLS listener resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TlsListenerId(u64);

impl TlsListenerId {
    /// Creates a TLS listener identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw listener identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for an opened file resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(u64);

impl FileId {
    /// Creates a file identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw file identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned file open flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FileOpenOptions {
    /// Open the file for reading.
    pub read: bool,
    /// Open the file for writing.
    pub write: bool,
    /// Create the file if it does not already exist.
    pub create: bool,
    /// Truncate the file to zero length on open.
    pub truncate: bool,
}

/// Exit status for a bounded local process run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessStatus {
    /// Portable process exit code when the platform exposes one.
    pub code: Option<i32>,
}

/// Result of a bounded local process run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRunResult {
    /// Process exit status.
    pub status: ProcessStatus,
    /// Captured stdout, capped by request.
    pub stdout: Vec<u8>,
    /// Captured stderr, capped by request.
    pub stderr: Vec<u8>,
    /// Whether stdout exceeded its cap.
    pub stdout_truncated: bool,
    /// Whether stderr exceeded its cap.
    pub stderr_truncated: bool,
}

/// Coarse kind of one local filesystem path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathKind {
    /// The path does not exist.
    Missing,

    /// The path is a regular file.
    File,

    /// The path is a directory.
    Directory,

    /// The path exists, but is neither a regular file nor a directory.
    Other,
}

/// Metadata Tina exposes for a local filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathMetadata {
    /// Coarse path kind.
    pub kind: PathKind,

    /// File length when the path is a regular file.
    pub len: Option<u64>,
}

impl PathMetadata {
    /// Returns metadata for a missing path.
    pub const fn missing() -> Self {
        Self {
            kind: PathKind::Missing,
            len: None,
        }
    }

    /// Returns whether the path exists.
    pub const fn exists(&self) -> bool {
        !matches!(self.kind, PathKind::Missing)
    }
}

impl FileOpenOptions {
    /// Opens an existing file for reading only.
    pub const fn read_only() -> Self {
        Self {
            read: true,
            write: false,
            create: false,
            truncate: false,
        }
    }

    /// Opens an existing file for writing only.
    pub const fn write_only() -> Self {
        Self {
            read: false,
            write: true,
            create: false,
            truncate: false,
        }
    }

    /// Opens an existing file for reading and writing.
    pub const fn read_write() -> Self {
        Self {
            read: true,
            write: true,
            create: false,
            truncate: false,
        }
    }

    /// Opens a file for reading and writing, creating it if needed and
    /// truncating any existing contents.
    pub const fn read_write_create_truncate() -> Self {
        Self {
            read: true,
            write: true,
            create: true,
            truncate: true,
        }
    }
}

/// Durable snapshot loaded by Tina's local persistence helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotImage {
    /// Opaque user payload.
    pub bytes: Vec<u8>,
    /// Last journal record reflected by this snapshot.
    pub last_journal_index: u64,
}

/// One durable domain record decoded from a journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    /// Monotonic application-owned record index.
    pub index: u64,
    /// Opaque user payload.
    pub bytes: Vec<u8>,
}

/// Non-fatal journal replay warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JournalReplayWarning {
    /// The journal ended with an incomplete final record. Valid prefix records
    /// were returned.
    TruncatedTail,
}

/// Journal replay output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalReplay {
    /// Valid records in durable order.
    pub records: Vec<JournalRecord>,
    /// Warning for a non-fatal tail condition.
    pub warning: Option<JournalReplayWarning>,
}

/// One concrete call shape understood by `tina-runtime`.
///
/// New verbs are added by extending this enum, not by adding a top-level
/// [`tina::Effect`] variant per verb.
#[derive(Debug, Clone)]
pub enum CallInput {
    /// Bind a TCP listener to `addr`.
    ///
    /// Uses [`SocketAddr`] rather than a logical name. The runtime reports the
    /// actual bound address back through [`CallOutput::TcpBound::local_addr`],
    /// including when the caller requests port `0` and lets the kernel pick a
    /// free ephemeral port.
    TcpBind {
        /// The address the listener should bind to.
        addr: SocketAddr,
    },

    /// Accept one inbound connection on a previously-bound listener.
    TcpAccept {
        /// The listener to accept on.
        listener: ListenerId,
    },

    /// Connect one outbound TCP stream to `addr`.
    TcpConnect {
        /// The remote address to connect to.
        addr: SocketAddr,
    },

    /// Read up to `max_len` bytes from a stream.
    ///
    /// A successful read of zero bytes signals end of stream; the runtime
    /// surfaces that in [`CallOutput::TcpRead`] with an empty `bytes`
    /// vector.
    TcpRead {
        /// The stream to read from.
        stream: StreamId,

        /// The maximum number of bytes the runtime may deliver in this
        /// completion.
        max_len: usize,
    },

    /// Write `bytes` to a stream. Partial writes are surfaced through
    /// [`CallOutput::TcpWrote`] so the issuing isolate can decide whether
    /// to issue another write for the remaining bytes.
    TcpWrite {
        /// The stream to write to.
        stream: StreamId,

        /// The payload to write.
        bytes: Vec<u8>,
    },

    /// Close a previously-bound listener and release its resources.
    TcpListenerClose {
        /// The listener to close.
        listener: ListenerId,
    },

    /// Close a previously-accepted stream and release its resources.
    TcpStreamClose {
        /// The stream to close.
        stream: StreamId,
    },

    /// Bind a UDP socket to `addr`.
    UdpBind {
        /// The address the UDP socket should bind to.
        addr: SocketAddr,
    },

    /// Send one UDP datagram.
    UdpSendTo {
        /// The UDP socket to send from.
        socket: UdpSocketId,
        /// Destination address.
        peer: SocketAddr,
        /// Datagram payload.
        bytes: Vec<u8>,
    },

    /// Receive one UDP datagram.
    UdpRecvFrom {
        /// The UDP socket to receive from.
        socket: UdpSocketId,
        /// Maximum payload bytes to deliver.
        max_len: usize,
    },

    /// Close a UDP socket and release its resources.
    UdpSocketClose {
        /// The UDP socket to close.
        socket: UdpSocketId,
    },

    /// Open one client TLS stream to `addr`.
    TlsConnect {
        /// Remote socket address.
        addr: SocketAddr,
        /// Server name used for certificate validation.
        server_name: String,
        /// Root certificates in DER form. This first native TLS slice requires
        /// explicit trust roots instead of silently reaching into platform
        /// stores.
        root_certificates: Vec<Vec<u8>>,
        /// Maximum time for TCP connect and TLS handshake.
        timeout: Duration,
    },

    /// Bind one server TLS listener to `addr`.
    TlsBind {
        /// Local socket address.
        addr: SocketAddr,
        /// Certificate chain in DER form.
        certificate_chain: Vec<Vec<u8>>,
        /// Private key in DER form.
        private_key: Vec<u8>,
    },

    /// Accept one inbound TLS stream from a TLS listener.
    TlsAccept {
        /// TLS listener to accept from.
        listener: TlsListenerId,
        /// Maximum time for TCP accept and TLS handshake.
        timeout: Duration,
    },

    /// Close one TLS listener.
    TlsListenerClose {
        /// TLS listener to close.
        listener: TlsListenerId,
    },

    /// Read decrypted bytes from a TLS stream.
    TlsRead {
        /// TLS stream to read from.
        stream: TlsStreamId,
        /// Maximum decrypted bytes to return.
        max_len: usize,
        /// Maximum time for this read.
        timeout: Duration,
    },

    /// Write decrypted bytes to a TLS stream.
    TlsWrite {
        /// TLS stream to write to.
        stream: TlsStreamId,
        /// Plaintext bytes to write through TLS.
        bytes: Vec<u8>,
        /// Maximum time for this write.
        timeout: Duration,
    },

    /// Close one TLS stream.
    TlsClose {
        /// TLS stream to close.
        stream: TlsStreamId,
        /// Maximum time for TLS close-notify/flush.
        timeout: Duration,
    },

    /// Resolve one host/port pair through the runtime-owned DNS rail.
    DnsLookup {
        /// Host name or address string.
        host: String,
        /// Service port.
        port: u16,
        /// Maximum time the caller is willing to wait. Already-started
        /// platform lookups may continue in the DNS lane and be tombstoned.
        timeout: Duration,
    },

    /// Wait for one runtime-owned signal event by name.
    ///
    /// Live OS-signal support is substrate-specific and currently unsupported
    /// on the shipped local system. The deterministic simulator implements
    /// this rail through scripted injection so signal-handling state machines
    /// can still be tested without installing process-global handlers.
    SignalWait {
        /// Runtime-owned signal name.
        name: String,
        /// Maximum time the caller is willing to wait.
        timeout: Duration,
    },

    /// Run one local process with bounded captured output.
    ProcessRun {
        /// Executable path or command name.
        command: String,
        /// Command arguments. No shell expansion is performed.
        args: Vec<String>,
        /// Maximum time the process may run before Tina attempts kill/reap.
        timeout: Duration,
        /// Maximum stdout bytes delivered to the isolate.
        stdout_limit: usize,
        /// Maximum stderr bytes delivered to the isolate.
        stderr_limit: usize,
    },

    /// Open a file and return a runtime-owned file id.
    FileOpen {
        /// Path to open.
        path: PathBuf,
        /// Open flags.
        options: FileOpenOptions,
    },

    /// Read bytes from a file at `offset`.
    FileReadAt {
        /// The file to read.
        file: FileId,
        /// Maximum bytes to read.
        len: usize,
        /// File offset.
        offset: u64,
    },

    /// Write bytes to a file at `offset`.
    FileWriteAt {
        /// The file to write.
        file: FileId,
        /// Bytes to write.
        bytes: Vec<u8>,
        /// File offset.
        offset: u64,
    },

    /// Flush file data to stable storage.
    FileFsync {
        /// The file to flush.
        file: FileId,
    },

    /// Query file size.
    FileSize {
        /// The file to query.
        file: FileId,
    },

    /// Close a runtime-owned file resource.
    FileClose {
        /// The file to close.
        file: FileId,
    },

    /// Create one directory.
    Mkdir {
        /// Directory path.
        path: PathBuf,
        /// Directory mode on Unix-like substrates.
        mode: u32,
    },

    /// Query coarse metadata for a path.
    PathMetadata {
        /// Path to inspect.
        path: PathBuf,
    },

    /// Rename `from` to `to`, replacing the destination where the platform
    /// explicitly supports existing-target replacement.
    RenameReplace {
        /// Source path.
        from: PathBuf,
        /// Destination path.
        to: PathBuf,
    },

    /// Remove one regular file.
    RemoveFile {
        /// File path to remove.
        path: PathBuf,
    },

    /// Read one directory and return deterministic sorted entries.
    ReadDir {
        /// Directory path to list.
        path: PathBuf,
    },

    /// Sync the parent directory for one path where the platform supports it.
    SyncParent {
        /// Path whose parent directory should be synced.
        path: PathBuf,
    },

    /// Commit one local snapshot.
    SnapshotCommit {
        /// Snapshot path.
        path: PathBuf,
        /// Opaque user payload.
        bytes: Vec<u8>,
        /// Last journal record reflected by this snapshot.
        last_journal_index: u64,
    },

    /// Load one local snapshot.
    SnapshotLoad {
        /// Snapshot path.
        path: PathBuf,
    },

    /// Append one local journal record.
    JournalAppend {
        /// Journal path.
        path: PathBuf,
        /// Monotonic application-owned record index.
        record_index: u64,
        /// Opaque user payload.
        bytes: Vec<u8>,
    },

    /// Replay one local journal.
    JournalReplay {
        /// Journal path.
        path: PathBuf,
    },

    /// Sleep for a relative duration.
    ///
    /// Completion fires no earlier than `armed_at + after` on a future
    /// step. The runtime samples its monotonic clock once per step;
    /// timers due at or before that sampled instant become eligible in
    /// that step.
    Sleep {
        /// The duration to wait before waking.
        after: Duration,
    },
}

impl CallInput {
    /// Returns the trace-level kind for this request.
    pub(crate) fn kind(&self) -> crate::trace::CallKind {
        match self {
            Self::TcpBind { .. } => crate::trace::CallKind::TcpBind,
            Self::TcpAccept { .. } => crate::trace::CallKind::TcpAccept,
            Self::TcpConnect { .. } => crate::trace::CallKind::TcpConnect,
            Self::TcpRead { .. } => crate::trace::CallKind::TcpRead,
            Self::TcpWrite { .. } => crate::trace::CallKind::TcpWrite,
            Self::TcpListenerClose { .. } => crate::trace::CallKind::TcpListenerClose,
            Self::TcpStreamClose { .. } => crate::trace::CallKind::TcpStreamClose,
            Self::UdpBind { .. } => crate::trace::CallKind::UdpBind,
            Self::UdpSendTo { .. } => crate::trace::CallKind::UdpSendTo,
            Self::UdpRecvFrom { .. } => crate::trace::CallKind::UdpRecvFrom,
            Self::UdpSocketClose { .. } => crate::trace::CallKind::UdpSocketClose,
            Self::TlsConnect { .. } => crate::trace::CallKind::TlsConnect,
            Self::TlsBind { .. } => crate::trace::CallKind::TlsBind,
            Self::TlsAccept { .. } => crate::trace::CallKind::TlsAccept,
            Self::TlsListenerClose { .. } => crate::trace::CallKind::TlsListenerClose,
            Self::TlsRead { .. } => crate::trace::CallKind::TlsRead,
            Self::TlsWrite { .. } => crate::trace::CallKind::TlsWrite,
            Self::TlsClose { .. } => crate::trace::CallKind::TlsClose,
            Self::DnsLookup { .. } => crate::trace::CallKind::DnsLookup,
            Self::SignalWait { .. } => crate::trace::CallKind::SignalWait,
            Self::ProcessRun { .. } => crate::trace::CallKind::ProcessRun,
            Self::FileOpen { .. } => crate::trace::CallKind::FileOpen,
            Self::FileReadAt { .. } => crate::trace::CallKind::FileReadAt,
            Self::FileWriteAt { .. } => crate::trace::CallKind::FileWriteAt,
            Self::FileFsync { .. } => crate::trace::CallKind::FileFsync,
            Self::FileSize { .. } => crate::trace::CallKind::FileSize,
            Self::FileClose { .. } => crate::trace::CallKind::FileClose,
            Self::Mkdir { .. } => crate::trace::CallKind::Mkdir,
            Self::PathMetadata { .. } => crate::trace::CallKind::PathMetadata,
            Self::RenameReplace { .. } => crate::trace::CallKind::RenameReplace,
            Self::RemoveFile { .. } => crate::trace::CallKind::RemoveFile,
            Self::ReadDir { .. } => crate::trace::CallKind::ReadDir,
            Self::SyncParent { .. } => crate::trace::CallKind::SyncParent,
            Self::SnapshotCommit { .. } => crate::trace::CallKind::SnapshotCommit,
            Self::SnapshotLoad { .. } => crate::trace::CallKind::SnapshotLoad,
            Self::JournalAppend { .. } => crate::trace::CallKind::JournalAppend,
            Self::JournalReplay { .. } => crate::trace::CallKind::JournalReplay,
            Self::Sleep { .. } => crate::trace::CallKind::Sleep,
        }
    }

    #[doc(hidden)]
    pub const fn persistence_trace_info(&self) -> Option<PersistenceTraceInfo> {
        match self {
            Self::SnapshotCommit { .. } => Some(PersistenceTraceInfo::SnapshotCommit),
            Self::SnapshotLoad { .. } | Self::JournalReplay { .. } => {
                Some(PersistenceTraceInfo::Recovery)
            }
            Self::JournalAppend { record_index, .. } => Some(PersistenceTraceInfo::JournalAppend {
                record_index: *record_index,
            }),
            _ => None,
        }
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceTraceInfo {
    /// Snapshot commit trace category.
    SnapshotCommit,
    /// Journal append trace category.
    JournalAppend {
        /// Application-owned record index.
        record_index: u64,
    },
    /// Recovery trace category.
    Recovery,
}

/// One concrete call completion delivered to the issuing isolate.
#[derive(Debug, Clone)]
pub enum CallOutput {
    /// A listener was successfully bound and is ready to accept.
    TcpBound {
        /// The runtime-assigned listener identifier.
        listener: ListenerId,

        /// The actual bound address reported by the runtime's I/O substrate.
        local_addr: SocketAddr,
    },

    /// A connection was accepted on a listener.
    TcpAccepted {
        /// The runtime-assigned stream identifier for the new connection.
        stream: StreamId,

        /// The remote peer address of the accepted stream.
        peer_addr: SocketAddr,
    },

    /// An outbound TCP connection completed.
    TcpConnected {
        /// The runtime-assigned stream identifier for the connected stream.
        stream: StreamId,

        /// The local address assigned to the stream.
        local_addr: SocketAddr,

        /// The remote peer address of the connected stream.
        peer_addr: SocketAddr,
    },

    /// A read returned `bytes`. An empty `bytes` vector means end of stream.
    TcpRead {
        /// The bytes the runtime read from the stream.
        bytes: Vec<u8>,
    },

    /// A write moved `count` bytes onto the stream. The issuing isolate is
    /// responsible for issuing another write if `count` is less than the
    /// requested length.
    TcpWrote {
        /// The number of bytes the runtime accepted.
        count: usize,
    },

    /// A listener was closed and its resources released.
    TcpListenerClosed,

    /// A stream was closed and its resources released.
    TcpStreamClosed,

    /// A UDP socket was bound.
    UdpBound {
        /// The runtime-assigned UDP socket id.
        socket: UdpSocketId,
        /// The local address reported by the OS.
        local_addr: SocketAddr,
    },

    /// One UDP datagram was sent.
    UdpSent {
        /// Number of bytes sent.
        count: usize,
    },

    /// One UDP datagram was received.
    UdpReceived {
        /// Sender address.
        peer_addr: SocketAddr,
        /// Payload bytes delivered to the isolate.
        bytes: Vec<u8>,
        /// Whether the datagram was truncated to the requested maximum length.
        truncated: bool,
    },

    /// A UDP socket was closed and its resources released.
    UdpSocketClosed,

    /// A TLS stream completed connection and handshake.
    TlsConnected {
        /// The runtime-assigned TLS stream id.
        stream: TlsStreamId,
    },

    /// A TLS listener was bound and is ready to accept.
    TlsBound {
        /// Runtime-assigned TLS listener id.
        listener: TlsListenerId,
        /// Actual bound address.
        local_addr: SocketAddr,
    },

    /// A TLS server stream was accepted and handshaken.
    TlsAccepted {
        /// Runtime-assigned TLS stream id.
        stream: TlsStreamId,
        /// Remote peer address.
        peer_addr: SocketAddr,
    },

    /// A TLS stream read decrypted bytes.
    TlsRead {
        /// Decrypted bytes.
        bytes: Vec<u8>,
    },

    /// A TLS stream wrote plaintext bytes.
    TlsWrote {
        /// Plaintext byte count accepted by the TLS stream.
        count: usize,
    },

    /// A TLS stream was closed.
    TlsClosed,

    /// A TLS listener was closed.
    TlsListenerClosed,

    /// A DNS lookup resolved to one or more socket addresses.
    DnsResolved {
        /// Resolved socket addresses.
        addrs: Vec<SocketAddr>,
    },

    /// A runtime-owned signal event was delivered.
    SignalReceived {
        /// Runtime-owned signal name.
        name: String,
    },

    /// A bounded local process exited and captured output was delivered.
    ProcessExited {
        /// Process exit status.
        status: ProcessStatus,
        /// Captured stdout, capped by the request.
        stdout: Vec<u8>,
        /// Captured stderr, capped by the request.
        stderr: Vec<u8>,
        /// Whether stdout exceeded the cap and was drained/discarded.
        stdout_truncated: bool,
        /// Whether stderr exceeded the cap and was drained/discarded.
        stderr_truncated: bool,
    },

    /// A file was opened.
    FileOpened {
        /// The runtime-assigned file id.
        file: FileId,
    },

    /// A positional file read completed.
    FileRead {
        /// The bytes read.
        bytes: Vec<u8>,
    },

    /// A positional file write completed.
    FileWrote {
        /// The number of bytes written.
        count: usize,
    },

    /// File data was flushed.
    FileSynced,

    /// File size was read.
    FileSize {
        /// File size in bytes.
        size: u64,
    },

    /// A file resource was closed.
    FileClosed,

    /// A directory was created.
    DirectoryCreated,

    /// Path metadata was read.
    PathMetadata {
        /// Coarse metadata.
        metadata: PathMetadata,
    },

    /// A path was renamed/replaced.
    PathRenamed,

    /// A regular file was removed.
    FileRemoved,

    /// A directory was listed.
    DirectoryRead {
        /// Sorted entries.
        entries: Vec<PathBuf>,
    },

    /// A parent directory was synced.
    ParentSynced,

    /// A snapshot was committed.
    SnapshotCommitted,

    /// A snapshot was loaded.
    SnapshotLoaded {
        /// Loaded snapshot, or `None` when no snapshot exists yet.
        snapshot: Option<SnapshotImage>,
    },

    /// A journal record was appended.
    JournalAppended {
        /// Appended record index.
        record_index: u64,
    },

    /// A journal replay completed.
    JournalReplayed {
        /// Replayed journal records.
        replay: JournalReplay,
    },

    /// A timer sleep completed and the isolate should wake.
    TimerFired,

    /// The runtime could not complete the call. The trace already records
    /// the failure with a richer reason; this variant is what the issuing
    /// isolate observes in its own vocabulary.
    Failed(CallError),
}

/// Why a runtime-owned call failed before it could complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallError {
    /// The referenced listener or stream id is not registered with the
    /// runtime.
    InvalidResource,

    /// The requested path or resource does not exist.
    NotFound,

    /// The runtime's underlying I/O substrate returned an error. The trace
    /// also records this; the isolate-facing variant is intentionally
    /// opaque to keep `os::ErrorKind` out of the call contract.
    Io,

    /// The runtime cannot honestly perform the requested operation on
    /// this substrate revision.
    ///
    /// Reserved for capability gaps the runtime cannot honestly
    /// perform on the current substrate revision. Future gaps should
    /// surface here rather than through silent fallbacks.
    Unsupported,

    /// The referenced runtime resource lane already has an in-flight
    /// operation. TCP accept, read, and write are separate lanes; a
    /// stream may have one read and one write pending at once, but
    /// duplicate work on the same lane fails here. Close does not
    /// surface as `ResourceBusy`: it cancels the pending op and
    /// closes the resource.
    ResourceBusy,

    /// A complete journal record had a checksum mismatch.
    CorruptRecord,

    /// Snapshot rename finished, but the runtime could not complete the final
    /// durability step such as syncing the parent directory.
    ///
    /// The caller must treat the durable state as unknown and recover from
    /// disk before applying follow-up assumptions.
    CommitUncertain,

    /// The bounded storage lane was full when the runtime tried to submit
    /// local filesystem or persistence work.
    StorageFull,

    /// The storage lane was already closed when the runtime tried to submit
    /// local filesystem or persistence work.
    StorageClosed,

    /// The target isolate's mailbox was full when the runtime attempted an
    /// isolate-to-isolate call.
    TargetFull,

    /// The target isolate was closed, stale, or otherwise unavailable when
    /// the runtime attempted an isolate-to-isolate call.
    TargetClosed,

    /// The target isolate did not reply before the caller's timeout elapsed.
    Timeout,

    /// The bounded DNS lane was full when the runtime tried to submit a lookup.
    DnsFull,

    /// The DNS lane was already closed when the runtime tried to submit a lookup.
    DnsClosed,

    /// The bounded TLS lane was full when the runtime tried to submit TLS work.
    TlsFull,

    /// The TLS lane was already closed when the runtime tried to submit work.
    TlsClosed,

    /// TLS certificate validation failed.
    TlsCertificate,

    /// TLS server-name validation or parsing failed.
    TlsName,

    /// TLS handshake or protocol processing failed.
    TlsHandshake,

    /// The bounded signal lane was full when the runtime tried to wait.
    SignalFull,

    /// The signal lane was already closed when the runtime tried to wait.
    SignalClosed,

    /// The bounded process lane was full when the runtime tried to submit work.
    ProcessFull,

    /// The process lane was already closed when the runtime tried to submit work.
    ProcessClosed,

    /// Tina attempted process kill/reap after timeout or cancel, but the
    /// platform did not prove the child was gone.
    KillUncertain,
}

/// User-visible outcome for an observed send.
///
/// Ordinary [`tina::send`] stays fire-and-forget. This outcome is only
/// produced by [`send_observed`], for code that needs explicit overload
/// policy in normal isolate message flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SendOutcome {
    /// The runtime accepted the send into the destination mailbox or, for a
    /// remote shard, into the bounded transport toward that shard.
    Accepted,

    /// The destination mailbox or bounded transport was full.
    Full,

    /// The destination was closed, stale, or otherwise no longer live.
    Closed,
}

/// User-visible outcome for an isolate-to-isolate call.
///
/// A call is just a bounded message send plus one later reply. The timeout is
/// mandatory so callers cannot accidentally create invisible forever-waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome<T> {
    /// The target isolate replied with a value of the expected type.
    Replied(T),

    /// The target isolate's mailbox was full.
    Full,

    /// The target isolate was closed, stale, or otherwise unavailable.
    Closed,

    /// The target isolate did not reply before the timeout elapsed.
    Timeout,
}

impl SendOutcome {
    pub(crate) fn from_rejected(reason: crate::trace::SendRejectedReason) -> Self {
        match reason {
            crate::trace::SendRejectedReason::Full => Self::Full,
            crate::trace::SendRejectedReason::Closed => Self::Closed,
        }
    }
}

/// Backend-neutral runtime-owned call request issued by an isolate.
///
/// The translator turns the runtime's later [`CallOutput`] back into one
/// ordinary [`tina::Isolate::Message`] value. The runtime never invokes a
/// second public handler entry point — completion always travels through
/// the isolate's existing [`Isolate::handle`](tina::Isolate::handle).
///
/// The struct is not [`Clone`] on purpose: a call request is meant to be
/// moved into the runtime, not duplicated, and the translator boxes a
/// non-`Clone` `FnOnce`.
#[must_use = "a call request has no effect until a runtime executes it"]
pub struct RuntimeCall<M> {
    kind: RuntimeCallKind<M>,
}

mod runtime_callable_sealed {
    pub trait Sealed {}
}

/// Marker trait identifying `Isolate::Call` types accepted by simulator-
/// driven and runtime-call-aware contexts.
///
/// This trait is implemented only for [`RuntimeCall`].
/// Surfaces in simulator and runtime-call bounds get a clearer compile
/// error than the previous `Call = RuntimeCall<...>` equality mismatch
/// when an isolate is authored with `#[tina::isolate]` (which defaults
/// `Call = Infallible`).
///
/// `RuntimeCall` satisfies the bound:
///
/// ```
/// use tina_runtime::{RuntimeCall, RuntimeCallable};
/// fn assert_callable<C: RuntimeCallable>() {}
/// assert_callable::<RuntimeCall<u32>>();
/// ```
///
/// `Infallible` (the default `Call` from `#[tina::isolate]`) does not:
///
/// ```compile_fail
/// use tina_runtime::RuntimeCallable;
/// fn assert_callable<C: RuntimeCallable>() {}
/// assert_callable::<std::convert::Infallible>();
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a Tina runtime call channel",
    label = "this isolate's `Call` is not `RuntimeCall<...>`",
    note = "`#[tina::isolate(...)]` defaults `Call = std::convert::Infallible`, which simulator-driven and runtime-call-aware paths cannot drive. Switch the attribute to `#[tina_runtime::isolate(...)]`, or supply `call = ::tina_runtime::RuntimeCall<YourMessage>` explicitly."
)]
pub trait RuntimeCallable: runtime_callable_sealed::Sealed {}

impl<M> runtime_callable_sealed::Sealed for RuntimeCall<M> {}
impl<M> RuntimeCallable for RuntimeCall<M> {}

enum RuntimeCallKind<M> {
    Backend {
        request: CallInput,
        translator: Box<dyn FnOnce(CallOutput) -> M>,
    },
    ObservedSend {
        target_shard: ShardId,
        target_isolate: IsolateId,
        target_generation: AddressGeneration,
        message: Box<dyn Any + Send>,
        translator: Box<dyn FnOnce(SendOutcome) -> M>,
    },
    IsolateCall {
        target_shard: ShardId,
        target_isolate: IsolateId,
        target_generation: AddressGeneration,
        message: Box<dyn Any + Send>,
        timeout: Duration,
        translator: IsolateCallTranslator<M>,
    },
}

impl<M> RuntimeCall<M> {
    /// Creates a new runtime-owned call request.
    ///
    /// `translator` runs once, when the runtime delivers the call's
    /// completion back to the issuing isolate. It must produce exactly one
    /// `Message` value — this is the load-bearing rule that keeps the
    /// completion path "ordinary later message," not a second handler
    /// entry point.
    pub fn new<F>(request: CallInput, translator: F) -> Self
    where
        F: FnOnce(CallOutput) -> M + 'static,
    {
        Self {
            kind: RuntimeCallKind::Backend {
                request,
                translator: Box::new(translator),
            },
        }
    }

    /// Creates a runtime-observed send request.
    ///
    /// The runtime attempts the send and later delivers one [`SendOutcome`]
    /// through `translator`. This preserves Tina's effect-returning handler
    /// model: the current handler turn still returns a description of work,
    /// and overload feedback comes back as an ordinary later message.
    pub fn observed_send<T, R, F>(destination: Address<T, R>, message: T, translator: F) -> Self
    where
        T: Send + 'static,
        F: FnOnce(SendOutcome) -> M + 'static,
    {
        Self {
            kind: RuntimeCallKind::ObservedSend {
                target_shard: destination.shard(),
                target_isolate: destination.isolate(),
                target_generation: destination.generation(),
                message: Box::new(message),
                translator: Box::new(translator),
            },
        }
    }

    /// Creates an isolate-to-isolate call request.
    ///
    /// The destination receives `message` as an ordinary handler message. If
    /// that handler later returns [`tina::reply`], the reply becomes
    /// [`CallOutcome::Replied`] for the requester. The timeout is mandatory.
    ///
    /// Same-shard calls complete inside one shard runtime. Live local
    /// multi-shard systems route cross-shard requests and replies through
    /// bounded shard-pair paths.
    ///
    /// Keep `translator` pure: the runtime may run it even when the resulting
    /// message cannot be delivered because the requester stopped or its
    /// mailbox filled.
    pub fn isolate_call<T, R, F>(
        destination: Address<T, R>,
        message: T,
        timeout: Duration,
        translator: F,
    ) -> Self
    where
        T: Send + 'static,
        R: 'static,
        F: FnOnce(CallOutcome<R>) -> M + 'static,
    {
        Self {
            kind: RuntimeCallKind::IsolateCall {
                target_shard: destination.shard(),
                target_isolate: destination.isolate(),
                target_generation: destination.generation(),
                message: Box::new(message),
                timeout,
                translator: Box::new(move |outcome| match outcome {
                    CallOutcome::Replied(reply) => {
                        let reply = *reply.downcast::<R>().unwrap_or_else(|_| {
                            panic!(
                                "isolate call reply had the wrong type; expected {}",
                                std::any::type_name::<R>()
                            )
                        });
                        translator(CallOutcome::Replied(reply))
                    }
                    CallOutcome::Full => translator(CallOutcome::Full),
                    CallOutcome::Closed => translator(CallOutcome::Closed),
                    CallOutcome::Timeout => translator(CallOutcome::Timeout),
                }),
            },
        }
    }

    /// Creates a call that receives a plain `Result<CallOutput, CallError>`
    /// instead of matching the failure variant manually.
    pub fn map_result<F>(request: CallInput, translator: F) -> Self
    where
        F: FnOnce(Result<CallOutput, CallError>) -> M + 'static,
    {
        Self::new(request, move |output| match output {
            CallOutput::Failed(error) => translator(Err(error)),
            other => translator(Ok(other)),
        })
    }

    /// Returns a shared reference to the underlying request.
    pub fn request(&self) -> &CallInput {
        match &self.kind {
            RuntimeCallKind::Backend { request, .. } => request,
            RuntimeCallKind::ObservedSend { .. } => {
                panic!("observed send does not carry a backend CallInput")
            }
            RuntimeCallKind::IsolateCall { .. } => {
                panic!("isolate call does not carry a backend CallInput")
            }
        }
    }

    /// Splits the call into its request and translator.
    pub fn into_parts(self) -> (CallInput, Box<dyn FnOnce(CallOutput) -> M>) {
        match self.kind {
            RuntimeCallKind::Backend {
                request,
                translator,
            } => (request, translator),
            RuntimeCallKind::ObservedSend { .. } => {
                panic!("observed send does not carry backend call parts")
            }
            RuntimeCallKind::IsolateCall { .. } => {
                panic!("isolate call does not carry backend call parts")
            }
        }
    }

    /// Splits this call into the runtime action it describes.
    ///
    /// This is primarily for sibling runtime/simulator crates that need to
    /// interpret `RuntimeCall` without depending on private fields.
    #[doc(hidden)]
    pub fn into_runtime_parts(self) -> RuntimeCallParts<M> {
        match self.kind {
            RuntimeCallKind::Backend {
                request,
                translator,
            } => RuntimeCallParts::Backend {
                request,
                translator,
            },
            RuntimeCallKind::ObservedSend {
                target_shard,
                target_isolate,
                target_generation,
                message,
                translator,
            } => RuntimeCallParts::ObservedSend {
                target_shard,
                target_isolate,
                target_generation,
                message,
                translator,
            },
            RuntimeCallKind::IsolateCall {
                target_shard,
                target_isolate,
                target_generation,
                message,
                timeout,
                translator,
            } => RuntimeCallParts::IsolateCall {
                target_shard,
                target_isolate,
                target_generation,
                message,
                timeout,
                translator,
            },
        }
    }
}

/// Publicly destructurable runtime action carried by [`RuntimeCall`].
#[doc(hidden)]
pub enum RuntimeCallParts<M> {
    /// Backend-owned I/O/time request.
    Backend {
        /// Concrete backend request.
        request: CallInput,
        /// Completion translator.
        translator: Box<dyn FnOnce(CallOutput) -> M>,
    },
    /// Runtime-observed send request.
    ObservedSend {
        /// Destination shard.
        target_shard: ShardId,
        /// Destination isolate.
        target_isolate: IsolateId,
        /// Destination generation.
        target_generation: AddressGeneration,
        /// Erased message payload.
        message: Box<dyn Any + Send>,
        /// Outcome translator.
        translator: Box<dyn FnOnce(SendOutcome) -> M>,
    },
    /// Isolate-to-isolate call request.
    IsolateCall {
        /// Destination shard.
        target_shard: ShardId,
        /// Destination isolate.
        target_isolate: IsolateId,
        /// Destination generation.
        target_generation: AddressGeneration,
        /// Erased request message payload.
        message: Box<dyn Any + Send>,
        /// Mandatory caller timeout.
        timeout: Duration,
        /// Outcome translator.
        translator: IsolateCallTranslator<M>,
    },
}

impl<M> std::fmt::Debug for RuntimeCall<M> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeCall")
            .field(
                "kind",
                &match &self.kind {
                    RuntimeCallKind::Backend { request, .. } => request.kind(),
                    RuntimeCallKind::ObservedSend { .. } => crate::trace::CallKind::ObservedSend,
                    RuntimeCallKind::IsolateCall { .. } => crate::trace::CallKind::IsolateCall,
                },
            )
            .finish_non_exhaustive()
    }
}

/// Erased call shape stored by the runtime once an isolate's
/// per-`I::Message` translator has been wrapped to a `Box<dyn Any>`.
///
/// `tina-runtime` does not expose this type's fields. Downstream
/// crates produce `ErasedCall` only via the [`IntoErasedCall`] conversion
/// trait. The struct is exposed publicly only to make the trait method
/// signature visible; it is not constructible from outside this crate.
pub struct ErasedCall {
    pub(crate) kind: ErasedCallKind,
}

pub(crate) enum ErasedCallKind {
    /// Runtime-owned I/O/time call executed by a backend.
    Backend {
        /// Concrete backend request.
        request: CallInput,
        /// Completion translator erased to `Any`.
        translator: Box<dyn FnOnce(CallOutput) -> Box<dyn Any>>,
    },
    /// Runtime-observed send attempt.
    ObservedSend {
        /// Erased outbound message to attempt.
        send: crate::ErasedSend,
        /// Outcome translator erased to `Any`.
        translator: Box<dyn FnOnce(SendOutcome) -> Box<dyn Any>>,
    },
    /// Isolate-to-isolate call request.
    IsolateCall {
        /// Erased outbound request message.
        send: crate::ErasedSend,
        /// Mandatory caller timeout.
        timeout: Duration,
        /// Outcome translator erased to `Any`.
        translator: ErasedIsolateCallTranslator,
    },
}

impl std::fmt::Debug for ErasedCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ErasedCall")
            .field(
                "kind",
                &match &self.kind {
                    ErasedCallKind::Backend { request, .. } => request.kind(),
                    ErasedCallKind::ObservedSend { .. } => crate::trace::CallKind::ObservedSend,
                    ErasedCallKind::IsolateCall { .. } => crate::trace::CallKind::IsolateCall,
                },
            )
            .finish_non_exhaustive()
    }
}

/// Conversion from one isolate-level call payload into the runtime's
/// erased form.
///
/// This mirrors the existing `IntoErasedSpawn` pattern: isolates that never
/// issue call effects use [`std::convert::Infallible`], and isolates that
/// do use `RuntimeCall<I::Message>`. New runtime crates that want a
/// different programming model implement their own conversion trait —
/// `tina` does not pin the conversion shape.
pub trait IntoErasedCall<M> {
    /// Erases the call into the runtime's internal form.
    fn into_erased_call(self) -> ErasedCall;
}

impl<M> IntoErasedCall<M> for std::convert::Infallible {
    fn into_erased_call(self) -> ErasedCall {
        match self {}
    }
}

impl<M> IntoErasedCall<M> for RuntimeCall<M>
where
    M: 'static,
{
    fn into_erased_call(self) -> ErasedCall {
        match self.into_runtime_parts() {
            RuntimeCallParts::Backend {
                request,
                translator,
            } => ErasedCall {
                kind: ErasedCallKind::Backend {
                    request,
                    translator: Box::new(move |result| {
                        Box::new(translator(result)) as Box<dyn Any>
                    }),
                },
            },
            RuntimeCallParts::ObservedSend {
                target_shard,
                target_isolate,
                target_generation,
                message,
                translator,
            } => ErasedCall {
                kind: ErasedCallKind::ObservedSend {
                    send: crate::ErasedSend {
                        target_shard,
                        target_isolate,
                        target_generation,
                        message: crate::ErasedMessage::Sendable(message),
                    },
                    translator: Box::new(move |outcome| {
                        Box::new(translator(outcome)) as Box<dyn Any>
                    }),
                },
            },
            RuntimeCallParts::IsolateCall {
                target_shard,
                target_isolate,
                target_generation,
                message,
                timeout,
                translator,
            } => ErasedCall {
                kind: ErasedCallKind::IsolateCall {
                    send: crate::ErasedSend {
                        target_shard,
                        target_isolate,
                        target_generation,
                        message: crate::ErasedMessage::Sendable(message),
                    },
                    timeout,
                    translator: Box::new(move |outcome| {
                        Box::new(translator(outcome)) as Box<dyn Any>
                    }),
                },
            },
        }
    }
}

impl CallOutput {
    fn panic_wrong_shape(expected: &str, found: &Self) -> ! {
        panic!("typed runtime call helper expected {expected}, but runtime returned {found:?}");
    }

    /// Extracts the timer completion payload.
    pub fn into_timer_fired(self) -> Result<(), CallError> {
        match self {
            Self::TimerFired => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TimerFired", &other),
        }
    }

    /// Extracts the successful TCP bind result.
    pub fn into_tcp_bound(self) -> Result<(ListenerId, SocketAddr), CallError> {
        match self {
            Self::TcpBound {
                listener,
                local_addr,
            } => Ok((listener, local_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpBound", &other),
        }
    }

    /// Extracts the successful TCP accept result.
    pub fn into_tcp_accepted(self) -> Result<(StreamId, SocketAddr), CallError> {
        match self {
            Self::TcpAccepted { stream, peer_addr } => Ok((stream, peer_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpAccepted", &other),
        }
    }

    /// Extracts the successful TCP connect result.
    pub fn into_tcp_connected(self) -> Result<(StreamId, SocketAddr, SocketAddr), CallError> {
        match self {
            Self::TcpConnected {
                stream,
                local_addr,
                peer_addr,
            } => Ok((stream, local_addr, peer_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpConnected", &other),
        }
    }

    /// Extracts the successful TCP read payload.
    pub fn into_tcp_read(self) -> Result<Vec<u8>, CallError> {
        match self {
            Self::TcpRead { bytes } => Ok(bytes),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpRead", &other),
        }
    }

    /// Extracts the successful TCP write payload.
    pub fn into_tcp_wrote(self) -> Result<usize, CallError> {
        match self {
            Self::TcpWrote { count } => Ok(count),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpWrote", &other),
        }
    }

    /// Extracts the successful listener close completion.
    pub fn into_tcp_listener_closed(self) -> Result<(), CallError> {
        match self {
            Self::TcpListenerClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpListenerClosed", &other),
        }
    }

    /// Extracts the successful stream close completion.
    pub fn into_tcp_stream_closed(self) -> Result<(), CallError> {
        match self {
            Self::TcpStreamClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpStreamClosed", &other),
        }
    }

    /// Extracts the successful UDP bind result.
    pub fn into_udp_bound(self) -> Result<(UdpSocketId, SocketAddr), CallError> {
        match self {
            Self::UdpBound { socket, local_addr } => Ok((socket, local_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UdpBound", &other),
        }
    }

    /// Extracts the successful UDP send count.
    pub fn into_udp_sent(self) -> Result<usize, CallError> {
        match self {
            Self::UdpSent { count } => Ok(count),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UdpSent", &other),
        }
    }

    /// Extracts the successful UDP receive payload.
    pub fn into_udp_received(self) -> Result<(SocketAddr, Vec<u8>, bool), CallError> {
        match self {
            Self::UdpReceived {
                peer_addr,
                bytes,
                truncated,
            } => Ok((peer_addr, bytes, truncated)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UdpReceived", &other),
        }
    }

    /// Extracts the successful UDP socket close completion.
    pub fn into_udp_socket_closed(self) -> Result<(), CallError> {
        match self {
            Self::UdpSocketClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UdpSocketClosed", &other),
        }
    }

    /// Extracts the successful TLS connect result.
    pub fn into_tls_connected(self) -> Result<TlsStreamId, CallError> {
        match self {
            Self::TlsConnected { stream } => Ok(stream),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsConnected", &other),
        }
    }

    /// Extracts the successful TLS bind result.
    pub fn into_tls_bound(self) -> Result<(TlsListenerId, SocketAddr), CallError> {
        match self {
            Self::TlsBound {
                listener,
                local_addr,
            } => Ok((listener, local_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsBound", &other),
        }
    }

    /// Extracts the successful TLS accept result.
    pub fn into_tls_accepted(self) -> Result<(TlsStreamId, SocketAddr), CallError> {
        match self {
            Self::TlsAccepted { stream, peer_addr } => Ok((stream, peer_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsAccepted", &other),
        }
    }

    /// Extracts the successful TLS read payload.
    pub fn into_tls_read(self) -> Result<Vec<u8>, CallError> {
        match self {
            Self::TlsRead { bytes } => Ok(bytes),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsRead", &other),
        }
    }

    /// Extracts the successful TLS write count.
    pub fn into_tls_wrote(self) -> Result<usize, CallError> {
        match self {
            Self::TlsWrote { count } => Ok(count),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsWrote", &other),
        }
    }

    /// Extracts the successful TLS close completion.
    pub fn into_tls_closed(self) -> Result<(), CallError> {
        match self {
            Self::TlsClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsClosed", &other),
        }
    }

    /// Extracts the successful TLS listener close completion.
    pub fn into_tls_listener_closed(self) -> Result<(), CallError> {
        match self {
            Self::TlsListenerClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsListenerClosed", &other),
        }
    }

    /// Extracts the successful DNS lookup result.
    pub fn into_dns_resolved(self) -> Result<Vec<SocketAddr>, CallError> {
        match self {
            Self::DnsResolved { addrs } => Ok(addrs),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("DnsResolved", &other),
        }
    }

    /// Extracts the successful signal-wait result.
    pub fn into_signal_received(self) -> Result<String, CallError> {
        match self {
            Self::SignalReceived { name } => Ok(name),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("SignalReceived", &other),
        }
    }

    /// Extracts the successful bounded process result.
    pub fn into_process_exited(self) -> Result<ProcessRunResult, CallError> {
        match self {
            Self::ProcessExited {
                status,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
            } => Ok(ProcessRunResult {
                status,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
            }),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("ProcessExited", &other),
        }
    }

    /// Extracts the successful file open result.
    pub fn into_file_opened(self) -> Result<FileId, CallError> {
        match self {
            Self::FileOpened { file } => Ok(file),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileOpened", &other),
        }
    }

    /// Extracts the successful file read payload.
    pub fn into_file_read(self) -> Result<Vec<u8>, CallError> {
        match self {
            Self::FileRead { bytes } => Ok(bytes),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileRead", &other),
        }
    }

    /// Extracts the successful file write count.
    pub fn into_file_wrote(self) -> Result<usize, CallError> {
        match self {
            Self::FileWrote { count } => Ok(count),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileWrote", &other),
        }
    }

    /// Extracts the successful file fsync result.
    pub fn into_file_synced(self) -> Result<(), CallError> {
        match self {
            Self::FileSynced => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileSynced", &other),
        }
    }

    /// Extracts the successful file size result.
    pub fn into_file_size(self) -> Result<u64, CallError> {
        match self {
            Self::FileSize { size } => Ok(size),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileSize", &other),
        }
    }

    /// Extracts the successful file close result.
    pub fn into_file_closed(self) -> Result<(), CallError> {
        match self {
            Self::FileClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileClosed", &other),
        }
    }

    /// Extracts the successful mkdir result.
    pub fn into_directory_created(self) -> Result<(), CallError> {
        match self {
            Self::DirectoryCreated => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("DirectoryCreated", &other),
        }
    }

    /// Extracts the successful path metadata result.
    pub fn into_path_metadata(self) -> Result<PathMetadata, CallError> {
        match self {
            Self::PathMetadata { metadata } => Ok(metadata),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("PathMetadata", &other),
        }
    }

    /// Extracts the successful rename/replace result.
    pub fn into_path_renamed(self) -> Result<(), CallError> {
        match self {
            Self::PathRenamed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("PathRenamed", &other),
        }
    }

    /// Extracts the successful remove-file result.
    pub fn into_file_removed(self) -> Result<(), CallError> {
        match self {
            Self::FileRemoved => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileRemoved", &other),
        }
    }

    /// Extracts the successful read-dir result.
    pub fn into_directory_read(self) -> Result<Vec<PathBuf>, CallError> {
        match self {
            Self::DirectoryRead { entries } => Ok(entries),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("DirectoryRead", &other),
        }
    }

    /// Extracts the successful parent-directory sync result.
    pub fn into_parent_synced(self) -> Result<(), CallError> {
        match self {
            Self::ParentSynced => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("ParentSynced", &other),
        }
    }

    /// Extracts the successful snapshot commit result.
    pub fn into_snapshot_committed(self) -> Result<(), CallError> {
        match self {
            Self::SnapshotCommitted => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("SnapshotCommitted", &other),
        }
    }

    /// Extracts the successful snapshot load result.
    pub fn into_snapshot_loaded(self) -> Result<Option<SnapshotImage>, CallError> {
        match self {
            Self::SnapshotLoaded { snapshot } => Ok(snapshot),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("SnapshotLoaded", &other),
        }
    }

    /// Extracts the successful journal append result.
    pub fn into_journal_appended(self) -> Result<(), CallError> {
        match self {
            Self::JournalAppended { .. } => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("JournalAppended", &other),
        }
    }

    /// Extracts the successful journal replay result.
    pub fn into_journal_replayed(self) -> Result<JournalReplay, CallError> {
        match self {
            Self::JournalReplayed { replay } => Ok(replay),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("JournalReplayed", &other),
        }
    }
}

/// Doc-hidden carrier used by typed call helpers like [`sleep`] and
/// [`tcp_read`].
#[doc(hidden)]
pub struct TypedCall<T> {
    request: CallInput,
    decode: fn(CallOutput) -> Result<T, CallError>,
}

/// Prepared observed-send helper returned by [`send_observed`].
#[doc(hidden)]
pub struct ObservedSend<T> {
    destination: Address<T>,
    message: T,
}

/// Prepared isolate-call helper returned by [`call`].
#[doc(hidden)]
pub struct IsolateCall<T, R> {
    destination: Address<T, R>,
    message: T,
    timeout: Duration,
    marker: std::marker::PhantomData<fn() -> R>,
}

impl<T> ObservedSend<T>
where
    T: Send + 'static,
{
    fn new<R>(destination: Address<T, R>, message: T) -> Self {
        Self {
            destination: destination.with_reply::<()>(),
            message,
        }
    }

    /// Turns this prepared observed send into one ordinary later message.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(SendOutcome) -> M + 'static,
        M: 'static,
    {
        tina::Effect::Call(RuntimeCall::observed_send(
            self.destination,
            self.message,
            translator,
        ))
    }
}

/// Returns a helper that attempts one send and later reports its outcome.
///
/// This is the overload-aware companion to ordinary fire-and-forget
/// [`tina::send`]. For cross-shard sends, [`SendOutcome::Accepted`] means the
/// source shard accepted the message into bounded transport toward the target
/// shard; destination-local mailbox failure is still recorded on the
/// destination trace.
pub fn send_observed<T, R>(destination: Address<T, R>, message: T) -> ObservedSend<T>
where
    T: Send + 'static,
{
    ObservedSend::new(destination, message)
}

impl<T, R> IsolateCall<T, R>
where
    T: Send + 'static,
    R: 'static,
{
    fn new(destination: Address<T, R>, message: T, timeout: Duration) -> Self {
        Self {
            destination,
            message,
            timeout,
            marker: std::marker::PhantomData,
        }
    }

    /// Turns this prepared call into one ordinary later message.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(CallOutcome<R>) -> M + 'static,
        M: 'static,
    {
        tina::Effect::Call(RuntimeCall::isolate_call(
            self.destination,
            self.message,
            self.timeout,
            translator,
        ))
    }
}

/// Returns a helper that calls another isolate and requires a timeout.
///
/// Same-shard calls complete inside one shard runtime. Live local multi-shard
/// systems route cross-shard requests and replies through bounded shard-pair
/// paths. Keep the `.reply(...)` translator pure: it may run even when the
/// translated message is later rejected by the requester's mailbox.
///
/// ```compile_fail
/// use std::time::Duration;
///
/// use tina::{Address, IsolateId, ShardId};
/// use tina_runtime::{call, CallOutcome};
///
/// enum Request {
///     Ask,
/// }
/// struct CorrectReply;
/// struct WrongReply;
///
/// let target: Address<Request, CorrectReply> =
///     Address::new_with_generation(ShardId::new(0), IsolateId::new(1), tina::AddressGeneration::new(0));
/// let _bad = call::<Request, WrongReply>(target, Request::Ask, Duration::from_millis(1));
/// ```
pub fn call<T, R>(destination: Address<T, R>, message: T, timeout: Duration) -> IsolateCall<T, R>
where
    T: Send + 'static,
    R: 'static,
{
    IsolateCall::new(destination, message, timeout)
}

/// Returns a sleep effect that ignores the infallible timer payload and
/// delivers `message` back later.
///
/// This is the small common path for "wait, then continue" state machines.
pub fn sleep_then<I, M>(after: Duration, message: M) -> tina::Effect<I>
where
    I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
    M: 'static,
{
    sleep(after).reply(move |_| message)
}

impl<T> CallOutcome<T> {
    /// Converts a call outcome into the successful reply or a call error.
    pub fn into_result(self) -> Result<T, CallError> {
        match self {
            Self::Replied(reply) => Ok(reply),
            Self::Full => Err(CallError::TargetFull),
            Self::Closed => Err(CallError::TargetClosed),
            Self::Timeout => Err(CallError::Timeout),
        }
    }
}

impl SendOutcome {
    /// Returns whether the send was accepted by the runtime boundary.
    pub const fn is_accepted(self) -> bool {
        matches!(self, Self::Accepted)
    }

    /// Returns whether the send hit bounded backpressure.
    pub const fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Returns whether the target was closed or stale.
    pub const fn is_closed(self) -> bool {
        matches!(self, Self::Closed)
    }
}

impl<T> TypedCall<T> {
    fn new(request: CallInput, decode: fn(CallOutput) -> Result<T, CallError>) -> Self {
        Self { request, decode }
    }

    /// Turns this prepared runtime-owned call into one ordinary later message.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(Result<T, CallError>) -> M + 'static,
        T: 'static,
    {
        let decode = self.decode;
        tina::Effect::Call(RuntimeCall::new(self.request, move |output| {
            translator(decode(output))
        }))
    }
}

/// Returns a typed sleep helper that later yields `Result<(), CallError>`.
pub fn sleep(after: Duration) -> TypedCall<()> {
    TypedCall::new(CallInput::Sleep { after }, CallOutput::into_timer_fired)
}

/// Returns a typed TCP bind helper that later yields one listener id and
/// bound address.
pub fn tcp_bind(addr: SocketAddr) -> TypedCall<(ListenerId, SocketAddr)> {
    TypedCall::new(CallInput::TcpBind { addr }, CallOutput::into_tcp_bound)
}

/// Returns a typed TCP accept helper that later yields one stream id and peer
/// address.
pub fn tcp_accept(listener: ListenerId) -> TypedCall<(StreamId, SocketAddr)> {
    TypedCall::new(
        CallInput::TcpAccept { listener },
        CallOutput::into_tcp_accepted,
    )
}

/// Returns a typed TCP connect helper that later yields one stream id, local
/// address, and peer address.
pub fn tcp_connect(addr: SocketAddr) -> TypedCall<(StreamId, SocketAddr, SocketAddr)> {
    TypedCall::new(
        CallInput::TcpConnect { addr },
        CallOutput::into_tcp_connected,
    )
}

/// Returns a typed TCP read helper that later yields the bytes read.
pub fn tcp_read(stream: StreamId, max_len: usize) -> TypedCall<Vec<u8>> {
    TypedCall::new(
        CallInput::TcpRead { stream, max_len },
        CallOutput::into_tcp_read,
    )
}

/// Returns a typed TCP write helper that later yields the accepted byte count.
pub fn tcp_write(stream: StreamId, bytes: Vec<u8>) -> TypedCall<usize> {
    TypedCall::new(
        CallInput::TcpWrite { stream, bytes },
        CallOutput::into_tcp_wrote,
    )
}

/// Returns a typed listener-close helper that later yields `Result<(), CallError>`.
pub fn tcp_close_listener(listener: ListenerId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::TcpListenerClose { listener },
        CallOutput::into_tcp_listener_closed,
    )
}

/// Returns a typed stream-close helper that later yields `Result<(), CallError>`.
pub fn tcp_close_stream(stream: StreamId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::TcpStreamClose { stream },
        CallOutput::into_tcp_stream_closed,
    )
}

/// Returns a typed UDP bind helper.
pub fn udp_bind(addr: SocketAddr) -> TypedCall<(UdpSocketId, SocketAddr)> {
    TypedCall::new(CallInput::UdpBind { addr }, CallOutput::into_udp_bound)
}

/// Returns a typed UDP send helper.
pub fn udp_send_to(socket: UdpSocketId, peer: SocketAddr, bytes: Vec<u8>) -> TypedCall<usize> {
    TypedCall::new(
        CallInput::UdpSendTo {
            socket,
            peer,
            bytes,
        },
        CallOutput::into_udp_sent,
    )
}

/// Returns a typed UDP receive helper.
pub fn udp_recv_from(
    socket: UdpSocketId,
    max_len: usize,
) -> TypedCall<(SocketAddr, Vec<u8>, bool)> {
    TypedCall::new(
        CallInput::UdpRecvFrom { socket, max_len },
        CallOutput::into_udp_received,
    )
}

/// Returns a typed UDP close helper.
pub fn udp_close_socket(socket: UdpSocketId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::UdpSocketClose { socket },
        CallOutput::into_udp_socket_closed,
    )
}

/// Returns a typed TLS connect helper with explicit DER root certificates.
pub fn tls_connect(
    addr: SocketAddr,
    server_name: impl Into<String>,
    root_certificates: Vec<Vec<u8>>,
    timeout: Duration,
) -> TypedCall<TlsStreamId> {
    TypedCall::new(
        CallInput::TlsConnect {
            addr,
            server_name: server_name.into(),
            root_certificates,
            timeout,
        },
        CallOutput::into_tls_connected,
    )
}

/// Returns a typed TLS server bind helper with explicit DER cert/key.
pub fn tls_bind(
    addr: SocketAddr,
    certificate_chain: Vec<Vec<u8>>,
    private_key: Vec<u8>,
) -> TypedCall<(TlsListenerId, SocketAddr)> {
    TypedCall::new(
        CallInput::TlsBind {
            addr,
            certificate_chain,
            private_key,
        },
        CallOutput::into_tls_bound,
    )
}

/// Returns a typed TLS server accept helper.
pub fn tls_accept(
    listener: TlsListenerId,
    timeout: Duration,
) -> TypedCall<(TlsStreamId, SocketAddr)> {
    TypedCall::new(
        CallInput::TlsAccept { listener, timeout },
        CallOutput::into_tls_accepted,
    )
}

/// Returns a typed TLS listener close helper.
pub fn tls_close_listener(listener: TlsListenerId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::TlsListenerClose { listener },
        CallOutput::into_tls_listener_closed,
    )
}

/// Returns a typed TLS read helper.
pub fn tls_read(stream: TlsStreamId, max_len: usize, timeout: Duration) -> TypedCall<Vec<u8>> {
    TypedCall::new(
        CallInput::TlsRead {
            stream,
            max_len,
            timeout,
        },
        CallOutput::into_tls_read,
    )
}

/// Returns a typed TLS write helper.
pub fn tls_write(stream: TlsStreamId, bytes: Vec<u8>, timeout: Duration) -> TypedCall<usize> {
    TypedCall::new(
        CallInput::TlsWrite {
            stream,
            bytes,
            timeout,
        },
        CallOutput::into_tls_wrote,
    )
}

/// Returns a typed TLS close helper.
pub fn tls_close(stream: TlsStreamId, timeout: Duration) -> TypedCall<()> {
    TypedCall::new(
        CallInput::TlsClose { stream, timeout },
        CallOutput::into_tls_closed,
    )
}

/// Returns a typed DNS lookup helper.
pub fn dns_lookup(
    host: impl Into<String>,
    port: u16,
    timeout: Duration,
) -> TypedCall<Vec<SocketAddr>> {
    TypedCall::new(
        CallInput::DnsLookup {
            host: host.into(),
            port,
            timeout,
        },
        CallOutput::into_dns_resolved,
    )
}

/// Returns a typed signal-wait helper.
pub fn signal_wait(name: impl Into<String>, timeout: Duration) -> TypedCall<String> {
    TypedCall::new(
        CallInput::SignalWait {
            name: name.into(),
            timeout,
        },
        CallOutput::into_signal_received,
    )
}

/// Returns a typed bounded process-run helper.
pub fn process_run(
    command: impl Into<String>,
    args: Vec<String>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> TypedCall<ProcessRunResult> {
    TypedCall::new(
        CallInput::ProcessRun {
            command: command.into(),
            args,
            timeout,
            stdout_limit,
            stderr_limit,
        },
        CallOutput::into_process_exited,
    )
}

/// Returns a typed file-open helper.
pub fn file_open(path: impl Into<PathBuf>, options: FileOpenOptions) -> TypedCall<FileId> {
    TypedCall::new(
        CallInput::FileOpen {
            path: path.into(),
            options,
        },
        CallOutput::into_file_opened,
    )
}

/// Opens a file for common snapshot-style use: read/write, create if missing,
/// and truncate existing contents.
pub fn file_create(path: impl Into<PathBuf>) -> TypedCall<FileId> {
    file_open(path, FileOpenOptions::read_write_create_truncate())
}

/// Returns a typed positional file-read helper.
pub fn file_read_at(file: FileId, len: usize, offset: u64) -> TypedCall<Vec<u8>> {
    TypedCall::new(
        CallInput::FileReadAt { file, len, offset },
        CallOutput::into_file_read,
    )
}

/// Returns a typed file-read helper at offset 0.
pub fn file_read(file: FileId, len: usize) -> TypedCall<Vec<u8>> {
    file_read_at(file, len, 0)
}

/// Returns a typed positional file-write helper.
pub fn file_write_at(file: FileId, bytes: Vec<u8>, offset: u64) -> TypedCall<usize> {
    TypedCall::new(
        CallInput::FileWriteAt {
            file,
            bytes,
            offset,
        },
        CallOutput::into_file_wrote,
    )
}

/// Returns a typed file-write helper at offset 0.
///
/// The completion still reports the number of bytes written; callers that need
/// full-write semantics should branch on that count and issue another
/// runtime-owned write if needed.
pub fn file_write(file: FileId, bytes: Vec<u8>) -> TypedCall<usize> {
    file_write_at(file, bytes, 0)
}

/// Returns a typed file fsync helper.
pub fn file_fsync(file: FileId) -> TypedCall<()> {
    TypedCall::new(CallInput::FileFsync { file }, CallOutput::into_file_synced)
}

/// Returns a typed file-size helper.
pub fn file_size(file: FileId) -> TypedCall<u64> {
    TypedCall::new(CallInput::FileSize { file }, CallOutput::into_file_size)
}

/// Returns a typed file-close helper.
pub fn file_close(file: FileId) -> TypedCall<()> {
    TypedCall::new(CallInput::FileClose { file }, CallOutput::into_file_closed)
}

/// Returns a typed directory-create helper.
pub fn mkdir(path: impl Into<PathBuf>, mode: u32) -> TypedCall<()> {
    TypedCall::new(
        CallInput::Mkdir {
            path: path.into(),
            mode,
        },
        CallOutput::into_directory_created,
    )
}

/// Returns a typed path-metadata helper.
pub fn path_metadata(path: impl Into<PathBuf>) -> TypedCall<PathMetadata> {
    TypedCall::new(
        CallInput::PathMetadata { path: path.into() },
        CallOutput::into_path_metadata,
    )
}

/// Returns a typed rename-replace helper.
pub fn rename_replace(from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> TypedCall<()> {
    TypedCall::new(
        CallInput::RenameReplace {
            from: from.into(),
            to: to.into(),
        },
        CallOutput::into_path_renamed,
    )
}

/// Returns a typed remove-file helper.
pub fn remove_file(path: impl Into<PathBuf>) -> TypedCall<()> {
    TypedCall::new(
        CallInput::RemoveFile { path: path.into() },
        CallOutput::into_file_removed,
    )
}

/// Returns a typed read-directory helper.
pub fn read_dir(path: impl Into<PathBuf>) -> TypedCall<Vec<PathBuf>> {
    TypedCall::new(
        CallInput::ReadDir { path: path.into() },
        CallOutput::into_directory_read,
    )
}

/// Returns a typed parent-directory sync helper.
pub fn sync_parent(path: impl Into<PathBuf>) -> TypedCall<()> {
    TypedCall::new(
        CallInput::SyncParent { path: path.into() },
        CallOutput::into_parent_synced,
    )
}

/// Commits one snapshot with Tina's local persistence framing.
pub fn snapshot_commit(
    path: impl Into<PathBuf>,
    bytes: Vec<u8>,
    last_journal_index: u64,
) -> TypedCall<()> {
    TypedCall::new(
        CallInput::SnapshotCommit {
            path: path.into(),
            bytes,
            last_journal_index,
        },
        CallOutput::into_snapshot_committed,
    )
}

/// Loads one snapshot committed by [`snapshot_commit`].
pub fn snapshot_load(path: impl Into<PathBuf>) -> TypedCall<Option<SnapshotImage>> {
    TypedCall::new(
        CallInput::SnapshotLoad { path: path.into() },
        CallOutput::into_snapshot_loaded,
    )
}

/// Appends one domain record to a local journal.
pub fn journal_append(
    path: impl Into<PathBuf>,
    record_index: u64,
    bytes: Vec<u8>,
) -> TypedCall<()> {
    TypedCall::new(
        CallInput::JournalAppend {
            path: path.into(),
            record_index,
            bytes,
        },
        CallOutput::into_journal_appended,
    )
}

/// Replays one local journal committed by [`journal_append`].
pub fn journal_replay(path: impl Into<PathBuf>) -> TypedCall<JournalReplay> {
    TypedCall::new(
        CallInput::JournalReplay { path: path.into() },
        CallOutput::into_journal_replayed,
    )
}

// ---------------------------------------------------------------------------
// Type aliases for runtime-call replies.
//
// Lets isolate enums spell `Connected(TcpConnectReply)` instead of
// `Connected(Result<(StreamId, SocketAddr, SocketAddr), CallError>)` and
// keeps the concrete payload visible by way of the alias name.
// ---------------------------------------------------------------------------

/// The shape every runtime-owned call delivers back: success payload or
/// typed [`CallError`].
pub type CallReply<T> = Result<T, CallError>;

/// Reply delivered by [`sleep`].
pub type SleepReply = CallReply<()>;

/// Reply delivered by [`tcp_bind`].
pub type TcpBindReply = CallReply<(ListenerId, SocketAddr)>;

/// Reply delivered by [`tcp_accept`].
pub type TcpAcceptReply = CallReply<(StreamId, SocketAddr)>;

/// Reply delivered by [`tcp_connect`].
pub type TcpConnectReply = CallReply<(StreamId, SocketAddr, SocketAddr)>;

/// Reply delivered by [`tcp_read`].
pub type TcpReadReply = CallReply<Vec<u8>>;

/// Reply delivered by [`tcp_write`].
pub type TcpWriteReply = CallReply<usize>;

/// Reply delivered by [`tcp_close_listener`].
pub type TcpListenerCloseReply = CallReply<()>;

/// Reply delivered by [`tcp_close_stream`].
pub type TcpStreamCloseReply = CallReply<()>;

/// Reply delivered by [`udp_bind`].
pub type UdpBindReply = CallReply<(UdpSocketId, SocketAddr)>;

/// Reply delivered by [`udp_send_to`].
pub type UdpSendToReply = CallReply<usize>;

/// Reply delivered by [`udp_recv_from`]. The `bool` is `true` on truncated
/// datagrams.
pub type UdpRecvFromReply = CallReply<(SocketAddr, Vec<u8>, bool)>;

/// Reply delivered by [`udp_close_socket`].
pub type UdpCloseSocketReply = CallReply<()>;

/// Reply delivered by [`tls_connect`].
pub type TlsConnectReply = CallReply<TlsStreamId>;

/// Reply delivered by [`tls_bind`].
pub type TlsBindReply = CallReply<(TlsListenerId, SocketAddr)>;

/// Reply delivered by [`tls_accept`].
pub type TlsAcceptReply = CallReply<(TlsStreamId, SocketAddr)>;

/// Reply delivered by [`tls_close_listener`].
pub type TlsListenerCloseReply = CallReply<()>;

/// Reply delivered by [`tls_read`].
pub type TlsReadReply = CallReply<Vec<u8>>;

/// Reply delivered by [`tls_write`].
pub type TlsWriteReply = CallReply<usize>;

/// Reply delivered by [`tls_close`].
pub type TlsCloseReply = CallReply<()>;

/// Reply delivered by [`snapshot_commit`].
pub type SnapshotCommitReply = CallReply<()>;

/// Reply delivered by [`snapshot_load`].
pub type SnapshotLoadReply = CallReply<Option<SnapshotImage>>;

/// Reply delivered by [`journal_append`].
pub type JournalAppendReply = CallReply<()>;

/// Reply delivered by [`journal_replay`].
pub type JournalReplayReply = CallReply<JournalReplay>;

/// Reply delivered by [`signal_wait`].
pub type SignalWaitReply = CallReply<String>;

/// Reply delivered by [`dns_lookup`].
pub type DnsLookupReply = CallReply<Vec<SocketAddr>>;

/// Reply delivered by [`process_run`].
pub type ProcessRunReply = CallReply<ProcessRunResult>;

/// Reply delivered by [`file_open`] / [`file_create`].
pub type FileOpenReply = CallReply<FileId>;

/// Reply delivered by [`file_read`] / [`file_read_at`].
pub type FileReadReply = CallReply<Vec<u8>>;

/// Reply delivered by [`file_write`] / [`file_write_at`].
pub type FileWriteReply = CallReply<usize>;

/// Reply delivered by [`file_fsync`].
pub type FileFsyncReply = CallReply<()>;

/// Reply delivered by [`file_size`].
pub type FileSizeReply = CallReply<u64>;

/// Reply delivered by [`file_close`].
pub type FileCloseReply = CallReply<()>;

/// Reply delivered by [`mkdir`].
pub type MkdirReply = CallReply<()>;

/// Reply delivered by [`path_metadata`].
pub type PathMetadataReply = CallReply<PathMetadata>;

/// Reply delivered by [`rename_replace`].
pub type RenameReplaceReply = CallReply<()>;

/// Reply delivered by [`remove_file`].
pub type RemoveFileReply = CallReply<()>;

/// Reply delivered by [`read_dir`].
pub type ReadDirReply = CallReply<Vec<PathBuf>>;

/// Reply delivered by [`sync_parent`].
pub type SyncParentReply = CallReply<()>;
