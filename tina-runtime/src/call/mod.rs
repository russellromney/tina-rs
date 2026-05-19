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

// ---------------------------------------------------------------------------
// Module map (Phase 115 reorg)
// ---------------------------------------------------------------------------
//
// Rail constructors live in per-rail submodules so adding or reading one
// rail does not require scrolling past every other rail. Submodule items
// are re-exported below so existing callers (`tina_runtime::tcp_bind`,
// `tina_runtime::file_open`, ...) are unchanged.
//
//   - `mod tcp`         — `tcp_bind`/`tcp_accept`/`tcp_connect`/read/write/close.
//   - `mod udp`         — `udp_bind`/`udp_send_to`/`udp_recv_from`/close.
//   - `mod tls`         — `tls_connect`/`tls_bind`/`tls_accept`/read/write/close.
//   - `mod dns`         — `dns_lookup`.
//   - `mod signals`     — `signal_wait`.
//   - `mod process`     — `process_run`.
//   - `mod files`       — file + filesystem-path operations.
//   - `mod persistence` — snapshot / journal helpers.
//
// Shared types (`CallId`, `CallInput`, `CallOutput`, `CallError`,
// `RuntimeCall*`, the `TypedCall*` family, cancellation / pending /
// group authority helpers) stay in this file.

mod cancel;
mod dns;
mod files;
mod pending;
mod persistence;
mod process;
mod signals;
mod tcp;
mod tls;
mod udp;

pub use cancel::*;
pub use dns::*;
pub use files::*;
pub use pending::*;
pub use persistence::*;
pub use process::*;
pub use signals::*;
pub use tcp::*;
pub use tls::*;
pub use udp::*;

use std::any::Any;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tina::{Address, AddressGeneration, CallHandleShared, CancelOutcome, IsolateId, ShardId};

type ErasedReply = Box<dyn Any>;
type ErasedCallOutcome = CallOutcome<ErasedReply>;
type IsolateCallTranslator<M> = Box<dyn FnOnce(ErasedCallOutcome) -> M>;
type ErasedIsolateCallTranslator = Box<dyn FnOnce(ErasedCallOutcome) -> Box<dyn Any>>;
type CancelCallTranslator<M> = Box<dyn FnOnce(CancelOutcome) -> M>;
type ErasedCancelCallTranslator = Box<dyn FnOnce(CancelOutcome) -> Box<dyn Any>>;

fn erase_isolate_call_translator<R, M, F>(translator: F) -> IsolateCallTranslator<M>
where
    R: 'static,
    F: FnOnce(CallOutcome<R>) -> M + 'static,
{
    Box::new(move |outcome| match outcome {
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
        CallOutcome::Rejected(reason) => translator(CallOutcome::Rejected(reason)),
    })
}

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
    /// were returned. `valid_prefix_len` is the byte length that can be
    /// safely retained before appending new records.
    TruncatedTail {
        /// Number of valid prefix bytes in the journal file.
        valid_prefix_len: u64,
    },
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

    /// The target isolate rejected the call without an application reply.
    Rejected(tina::CallRejectedReason),

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

    /// The target isolate rejected the call without an application reply.
    Rejected(tina::CallRejectedReason),
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
        expected_reply_type_id: std::any::TypeId,
        /// Optional caller-owned cancellation cell. Present when the
        /// effect was built via [`call_cancelable`]; the runtime stamps
        /// the assigned `CallId` here on dispatch and updates state on
        /// completion or cancellation.
        handle_shared: Option<Arc<CallHandleShared>>,
    },
    CancelCall {
        handle_shared: Arc<CallHandleShared>,
        translator: CancelCallTranslator<M>,
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
                translator: erase_isolate_call_translator::<R, _, _>(translator),
                expected_reply_type_id: std::any::TypeId::of::<R>(),
                handle_shared: None,
            },
        }
    }

    /// Like [`Self::isolate_call`] but carries a caller-owned shared
    /// cell for cancellation. The runtime stamps the assigned `CallId`
    /// on `handle_shared` at dispatch time.
    pub fn isolate_call_with_handle<T, R, F>(
        destination: Address<T, R>,
        message: T,
        timeout: Duration,
        translator: F,
        handle_shared: Arc<CallHandleShared>,
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
                translator: erase_isolate_call_translator::<R, _, _>(translator),
                expected_reply_type_id: std::any::TypeId::of::<R>(),
                handle_shared: Some(handle_shared),
            },
        }
    }

    /// Creates a cancel-call request that closes one pending isolate
    /// call's caller-side wait.
    pub fn cancel_call_with_handle<F>(handle_shared: Arc<CallHandleShared>, translator: F) -> Self
    where
        F: FnOnce(CancelOutcome) -> M + 'static,
    {
        Self {
            kind: RuntimeCallKind::CancelCall {
                handle_shared,
                translator: Box::new(translator),
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
            RuntimeCallKind::CancelCall { .. } => {
                panic!("cancel call does not carry a backend CallInput")
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
            RuntimeCallKind::CancelCall { .. } => {
                panic!("cancel call does not carry backend call parts")
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
                expected_reply_type_id,
                handle_shared,
            } => RuntimeCallParts::IsolateCall {
                target_shard,
                target_isolate,
                target_generation,
                message,
                timeout,
                translator,
                expected_reply_type_id,
                handle_shared,
            },
            RuntimeCallKind::CancelCall {
                handle_shared,
                translator,
            } => RuntimeCallParts::CancelCall {
                handle_shared,
                translator,
            },
        }
    }
}

/// Publicly destructurable runtime action carried by [`RuntimeCall`].
///
/// `IsolateCall::handle_shared` and the `CancelCall` variant carry the
/// caller-owned cancellation cell so sibling runtime crates (`tina-sim`,
/// future deterministic backends) can implement the same cancel
/// semantics without reaching into private fields.
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
        /// `TypeId::of::<R>()` for the dispatching `Address<_, R>`.
        expected_reply_type_id: std::any::TypeId,
        /// Optional caller-owned cancellation cell. Set by
        /// [`call_cancelable`].
        handle_shared: Option<Arc<CallHandleShared>>,
    },
    /// Cancel-call request: close one pending isolate call's wait.
    CancelCall {
        /// Caller-owned shared cell identifying the call.
        handle_shared: Arc<CallHandleShared>,
        /// Outcome translator.
        translator: CancelCallTranslator<M>,
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
                    RuntimeCallKind::CancelCall { .. } => crate::trace::CallKind::CancelCall,
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
        /// `TypeId::of::<R>()` for the dispatching `Address<_, R>`.
        /// Used to typecheck deferred-reply payloads before they
        /// reach the translator's downcast.
        expected_reply_type_id: std::any::TypeId,
        /// Optional caller-owned cancellation cell.
        handle_shared: Option<Arc<CallHandleShared>>,
    },
    /// Cancel-call request: close one pending isolate call's wait.
    CancelCall {
        /// Caller-owned shared cell identifying the call.
        handle_shared: Arc<CallHandleShared>,
        /// Outcome translator erased to `Any`.
        translator: ErasedCancelCallTranslator,
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
                    ErasedCallKind::CancelCall { .. } => crate::trace::CallKind::CancelCall,
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
                expected_reply_type_id,
                handle_shared,
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
                    expected_reply_type_id,
                    handle_shared,
                },
            },
            RuntimeCallParts::CancelCall {
                handle_shared,
                translator,
            } => ErasedCall {
                kind: ErasedCallKind::CancelCall {
                    handle_shared,
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

/// Prepared observed-send continuation after caller authority was captured.
#[doc(hidden)]
pub struct DeferredObservedSend<T, Q> {
    inner: ObservedSend<T>,
    request: tina::RequestContext<Q>,
}

/// Request-effect wrapper for [`DeferredObservedSend`].
#[doc(hidden)]
pub struct RequestDeferredObservedSend<T, Q> {
    inner: DeferredObservedSend<T, Q>,
}

/// Prepared isolate-call helper returned by [`call`].
#[doc(hidden)]
pub struct IsolateCall<T, R> {
    destination: Address<T, R>,
    message: T,
    timeout: Duration,
    marker: std::marker::PhantomData<fn() -> R>,
}

/// Prepared isolate-call continuation after caller authority was captured.
#[doc(hidden)]
pub struct DeferredIsolateCall<T, R, Q> {
    inner: IsolateCall<T, R>,
    request: tina::RequestContext<Q>,
}

/// Request-effect wrapper for [`DeferredIsolateCall`].
#[doc(hidden)]
pub struct RequestDeferredIsolateCall<T, R, Q> {
    inner: DeferredIsolateCall<T, R, Q>,
}

/// Prepared typed runtime-call continuation after caller authority was
/// captured.
#[doc(hidden)]
pub struct DeferredTypedCall<T, Q> {
    inner: TypedCall<T>,
    request: tina::RequestContext<Q>,
}

/// Request-effect wrapper for [`DeferredTypedCall`].
#[doc(hidden)]
pub struct RequestDeferredTypedCall<T, Q> {
    inner: DeferredTypedCall<T, Q>,
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
    #[deprecated(
        since = "0.1.0",
        note = "use `.then(...)` for ordinary continuations; use `call_ctx.defer(work).reply(...)` in handle_call when preserving caller authority"
    )]
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(SendOutcome) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Turns this prepared observed send into one ordinary later message.
    pub fn then<I, F, M>(self, translator: F) -> tina::Effect<I>
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

    /// Like [`reply`](Self::reply), but also carries the caller's
    /// [`RequestContext`] into the continuation message so a multi-turn
    /// service can still answer the original caller after the observed
    /// send resolves.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then_with_request(...)`; use `call_ctx.defer(work).reply(...)` when starting from CallContext"
    )]
    pub fn reply_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, SendOutcome) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        self.then_with_request(req, translator)
    }

    /// Alias for [`reply_with_request`](Self::reply_with_request) using the
    /// ordinary-continuation vocabulary.
    pub fn then_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, SendOutcome) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        tina::Effect::Call(RuntimeCall::observed_send(
            self.destination,
            self.message,
            move |outcome| translator(req, outcome),
        ))
    }
}

impl<T, Q> DeferredObservedSend<T, Q>
where
    T: Send + 'static,
    Q: 'static,
{
    /// Builds the continuation that carries the captured caller request.
    ///
    /// This does not reply to the caller by itself. The generated message must
    /// later consume the [`RequestContext`](tina::RequestContext) with
    /// `reply_to_request`.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, SendOutcome) -> M + 'static,
        M: 'static,
    {
        self.inner.then_with_request(self.request, translator)
    }
}

impl<T, Q> RequestDeferredObservedSend<T, Q>
where
    T: Send + 'static,
    Q: 'static,
{
    /// Builds a request effect whose continuation carries caller authority.
    pub fn reply<I, F, M>(self, translator: F) -> tina::RequestEffect<I>
    where
        I: tina::Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, SendOutcome) -> M + 'static,
        M: 'static,
    {
        tina::runtime_internal::request_effect_from_consumed_effect(self.inner.reply(translator))
    }
}

impl<T, I> tina::DeferThrough<I> for ObservedSend<T>
where
    T: Send + 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type Deferred = DeferredObservedSend<T, I::Reply>;

    fn defer_through(self, call: tina::CallContext<'_, I>) -> Self::Deferred {
        DeferredObservedSend {
            inner: self,
            request: call.into_request_context(),
        }
    }
}

impl<T, I> tina::RequestDeferThrough<I> for ObservedSend<T>
where
    T: Send + 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type RequestDeferred = RequestDeferredObservedSend<T, I::Reply>;

    fn defer_request_through(self, call: tina::RequestCall<'_, I>) -> Self::RequestDeferred {
        RequestDeferredObservedSend {
            inner: <Self as tina::DeferThrough<I>>::defer_through(self, call.into_call_context()),
        }
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
    #[deprecated(
        since = "0.1.0",
        note = "use `.then(...)` for ordinary continuations; use `call_ctx.defer(work).reply(...)` in handle_call when preserving caller authority"
    )]
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(CallOutcome<R>) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Turns this prepared call into one ordinary later message.
    pub fn then<I, F, M>(self, translator: F) -> tina::Effect<I>
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

    /// Like [`reply`](Self::reply), but also carries the caller's
    /// [`RequestContext`] into the continuation message so a multi-turn
    /// service can still answer the original caller after the child call
    /// resolves.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then_with_request(...)`; use `call_ctx.defer(work).reply(...)` when starting from CallContext"
    )]
    pub fn reply_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, CallOutcome<R>) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        self.then_with_request(req, translator)
    }

    /// Alias for [`reply_with_request`](Self::reply_with_request) using the
    /// ordinary-continuation vocabulary.
    pub fn then_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, CallOutcome<R>) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        tina::Effect::Call(RuntimeCall::isolate_call(
            self.destination,
            self.message,
            self.timeout,
            move |outcome| translator(req, outcome),
        ))
    }
}

impl<T, R, Q> DeferredIsolateCall<T, R, Q>
where
    T: Send + 'static,
    R: 'static,
    Q: 'static,
{
    /// Builds the continuation that carries the captured caller request.
    ///
    /// This does not reply to the caller by itself. The generated message must
    /// later consume the [`RequestContext`](tina::RequestContext) with
    /// `reply_to_request`.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, CallOutcome<R>) -> M + 'static,
        M: 'static,
    {
        self.inner.then_with_request(self.request, translator)
    }
}

impl<T, R, Q> RequestDeferredIsolateCall<T, R, Q>
where
    T: Send + 'static,
    R: 'static,
    Q: 'static,
{
    /// Builds a request effect whose continuation carries caller authority.
    pub fn reply<I, F, M>(self, translator: F) -> tina::RequestEffect<I>
    where
        I: tina::Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, CallOutcome<R>) -> M + 'static,
        M: 'static,
    {
        tina::runtime_internal::request_effect_from_consumed_effect(self.inner.reply(translator))
    }
}

impl<T, R, I> tina::DeferThrough<I> for IsolateCall<T, R>
where
    T: Send + 'static,
    R: 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type Deferred = DeferredIsolateCall<T, R, I::Reply>;

    fn defer_through(self, call: tina::CallContext<'_, I>) -> Self::Deferred {
        DeferredIsolateCall {
            inner: self,
            request: call.into_request_context(),
        }
    }
}

impl<T, R, I> tina::RequestDeferThrough<I> for IsolateCall<T, R>
where
    T: Send + 'static,
    R: 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type RequestDeferred = RequestDeferredIsolateCall<T, R, I::Reply>;

    fn defer_request_through(self, call: tina::RequestCall<'_, I>) -> Self::RequestDeferred {
        RequestDeferredIsolateCall {
            inner: <Self as tina::DeferThrough<I>>::defer_through(self, call.into_call_context()),
        }
    }
}

/// Returns a helper that calls another isolate and requires a timeout.
///
/// Same-shard calls complete inside one shard runtime. Live local multi-shard
/// systems route cross-shard requests and replies through bounded shard-pair
/// paths. Keep the `.then(...)` translator pure: it may run even when the
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

/// Capability-typed call that accepts only a [`tina::CallAddress`].
///
/// `call_typed(call_address, msg, timeout)` is the preferred call entry point.
/// Passing a [`SendAddress`](tina::SendAddress) or the `.send` lane of a
/// [`ServiceHandle`](crate::ServiceHandle) is a compile error. The runtime
/// semantics are identical to [`call`]; the only difference is the boundary
/// type-check.
///
/// Negative fixture: calling a send-only address is a compile error.
///
/// ```compile_fail
/// use std::time::Duration;
/// use tina::{Address, AddressGeneration, IsolateId, SendAddress, ShardId};
/// use tina_runtime::call_typed;
///
/// enum Msg { Tick }
/// struct Reply;
///
/// let raw: Address<Msg, Reply> = Address::new_with_generation(
///     ShardId::new(0),
///     IsolateId::new(1),
///     AddressGeneration::new(0),
/// );
/// let send_only: SendAddress<Msg> = raw.send_only();
/// // Expected `CallAddress`, found `SendAddress`.
/// let _ = call_typed(send_only, Msg::Tick, Duration::from_millis(1));
/// ```
pub fn call_typed<T, R>(
    destination: tina::CallAddress<T, R>,
    message: T,
    timeout: Duration,
) -> IsolateCall<T, R>
where
    T: Send + 'static,
    R: 'static,
{
    IsolateCall::new(destination.address(), message, timeout)
}

/// Capability-typed call for split service requests.
///
/// This is the request-lane companion to [`tina::send_event`]. Passing an
/// event address here is a compile error, and request payloads are wrapped
/// into [`tina::ServiceMessage::Request`] before dispatch.
pub fn call_request<E, Q, R>(
    destination: tina::ServiceRequestAddress<E, Q, R>,
    request: Q,
    timeout: Duration,
) -> IsolateCall<tina::ServiceMessage<E, Q>, R>
where
    E: Send + 'static,
    Q: Send + 'static,
    R: 'static,
{
    IsolateCall::new(
        destination.address().address(),
        tina::ServiceMessage::Request(request),
        timeout,
    )
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
    sleep(after).then(move |_| message)
}

impl<T> CallOutcome<T> {
    /// Converts a call outcome into the successful reply or a call error.
    pub fn into_result(self) -> Result<T, CallError> {
        match self {
            Self::Replied(reply) => Ok(reply),
            Self::Full => Err(CallError::TargetFull),
            Self::Closed => Err(CallError::TargetClosed),
            Self::Timeout => Err(CallError::Timeout),
            Self::Rejected(reason) => Err(CallError::Rejected(reason)),
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
    pub(super) fn new(request: CallInput, decode: fn(CallOutput) -> Result<T, CallError>) -> Self {
        Self { request, decode }
    }

    /// Turns this prepared runtime-owned call into one ordinary later message.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then(...)` for ordinary continuations; use `call_ctx.defer(work).reply(...)` in handle_call when preserving caller authority"
    )]
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(Result<T, CallError>) -> M + 'static,
        T: 'static,
    {
        self.then(translator)
    }

    /// Turns this prepared runtime-owned call into one ordinary later message.
    pub fn then<I, F, M>(self, translator: F) -> tina::Effect<I>
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

    /// Like [`reply`](Self::reply), but also carries the caller's
    /// [`RequestContext`] into the continuation message.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then_with_request(...)`; use `call_ctx.defer(work).reply(...)` when starting from CallContext"
    )]
    pub fn reply_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, Result<T, CallError>) -> M + 'static,
        T: 'static,
        M: 'static,
        Q: 'static,
    {
        self.then_with_request(req, translator)
    }

    /// Alias for [`reply_with_request`](Self::reply_with_request) using the
    /// ordinary-continuation vocabulary.
    pub fn then_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, Result<T, CallError>) -> M + 'static,
        T: 'static,
        M: 'static,
        Q: 'static,
    {
        let decode = self.decode;
        tina::Effect::Call(RuntimeCall::new(self.request, move |output| {
            translator(req, decode(output))
        }))
    }
}

impl<T, Q> DeferredTypedCall<T, Q>
where
    T: 'static,
    Q: 'static,
{
    /// Builds the continuation that carries the captured caller request.
    ///
    /// This does not reply to the caller by itself. The generated message must
    /// later consume the [`RequestContext`](tina::RequestContext) with
    /// `reply_to_request`.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, Result<T, CallError>) -> M + 'static,
        M: 'static,
    {
        self.inner.then_with_request(self.request, translator)
    }
}

impl<T, Q> RequestDeferredTypedCall<T, Q>
where
    T: 'static,
    Q: 'static,
{
    /// Builds a request effect whose continuation carries caller authority.
    pub fn reply<I, F, M>(self, translator: F) -> tina::RequestEffect<I>
    where
        I: tina::Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, Result<T, CallError>) -> M + 'static,
        M: 'static,
    {
        tina::runtime_internal::request_effect_from_consumed_effect(self.inner.reply(translator))
    }
}

impl<T, I> tina::DeferThrough<I> for TypedCall<T>
where
    T: 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type Deferred = DeferredTypedCall<T, I::Reply>;

    fn defer_through(self, call: tina::CallContext<'_, I>) -> Self::Deferred {
        DeferredTypedCall {
            inner: self,
            request: call.into_request_context(),
        }
    }
}

impl<T, I> tina::RequestDeferThrough<I> for TypedCall<T>
where
    T: 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type RequestDeferred = RequestDeferredTypedCall<T, I::Reply>;

    fn defer_request_through(self, call: tina::RequestCall<'_, I>) -> Self::RequestDeferred {
        RequestDeferredTypedCall {
            inner: <Self as tina::DeferThrough<I>>::defer_through(self, call.into_call_context()),
        }
    }
}

/// Returns a typed sleep helper that later yields `Result<(), CallError>`.
///
/// The returned [`SleepCall`] wraps an internal `TypedCall<()>` so the user
/// surface keeps `.then(...)` and adds `.then_event(...)` for the common
/// "wake me later with this event" path. Sleep is the only `TypedCall<()>`
/// that gets [`SleepCall::then_event`] — non-timer `TypedCall<()>` (file,
/// process, TCP close, ...) must surface their error path with `.then(...)`.
pub fn sleep(after: Duration) -> SleepCall {
    SleepCall::new(TypedCall::new(
        CallInput::Sleep { after },
        CallOutput::into_timer_fired,
    ))
}

/// Sleep-only wrapper around `TypedCall<()>`. Adds the unit-event sugar
/// [`SleepCall::then_event`] without exposing it on every `TypedCall<()>`.
///
/// `then` and `then_with_request` forward unchanged so existing
/// `sleep(d).then(...)` paths still compile.
#[doc(hidden)]
pub struct SleepCall {
    inner: TypedCall<()>,
}

impl SleepCall {
    fn new(inner: TypedCall<()>) -> Self {
        Self { inner }
    }

    /// Ordinary "wake me later" continuation. Identical to
    /// [`TypedCall::then`] for the timer payload.
    pub fn then<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(Result<(), CallError>) -> M + 'static,
    {
        self.inner.then(translator)
    }

    /// Carry caller authority into the wake continuation.
    pub fn then_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, Result<(), CallError>) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        self.inner.then_with_request(req, translator)
    }

    /// Timer-only sugar: produce the next event without reading the timer
    /// reply. Use when the handler enum has nothing to gain from a
    /// `SleepReply`-shaped field.
    ///
    /// This helper is intentionally only on `SleepCall`. A non-timer
    /// `TypedCall<()>` (file, process, TCP close, signal, bridge) must be
    /// consumed with `.then(...)` so its error path stays visible.
    ///
    /// Positive shape: the user enum has nothing timer-shaped in it.
    ///
    /// ```
    /// # use std::convert::Infallible;
    /// # use std::time::Duration;
    /// # use tina::prelude::*;
    /// # use tina_runtime::{sleep, RuntimeCall};
    /// #[derive(Debug)]
    /// enum Msg {
    ///     Wake { id: u64 },
    /// }
    /// # struct Svc;
    /// # impl Isolate for Svc {
    /// #     type Message = Msg;
    /// #     type Reply = ();
    /// #     type Send = tina::Outbound<Infallible>;
    /// #     type Spawn = Infallible;
    /// #     type SpawnObserved = Infallible;
    /// #     type Call = RuntimeCall<Msg>;
    /// #     type Fact = Infallible;
    /// #     type Shard = tina::SingleShard;
    /// #     fn handle(&mut self, _m: Msg, _ctx: &mut Context<'_, Self::Shard, ()>) -> Effect<Self> {
    /// #         tina::noop()
    /// #     }
    /// # }
    /// fn schedule(id: u64) -> Effect<Svc> {
    ///     sleep(Duration::from_millis(1)).then_event(move || Msg::Wake { id })
    /// }
    /// ```
    ///
    /// Compile-fail: `then_event` is not on `TypedCall<()>`. The TCP
    /// close path also returns `TypedCall<()>` but its error must stay
    /// visible, so `.then(...)` is the only way to consume it.
    ///
    /// ```compile_fail
    /// # use std::convert::Infallible;
    /// # use tina::prelude::*;
    /// # use tina_runtime::{tcp_close_stream, RuntimeCall, StreamId};
    /// # struct Svc;
    /// # #[derive(Debug)] enum Msg { Done }
    /// # impl Isolate for Svc {
    /// #     type Message = Msg;
    /// #     type Reply = ();
    /// #     type Send = tina::Outbound<Infallible>;
    /// #     type Spawn = Infallible;
    /// #     type SpawnObserved = Infallible;
    /// #     type Call = RuntimeCall<Msg>;
    /// #     type Shard = tina::SingleShard;
    /// #     fn handle(&mut self, _m: Msg, _ctx: &mut Context<'_, Self::Shard, ()>) -> Effect<Self> {
    /// #         tina::noop()
    /// #     }
    /// # }
    /// fn close(stream: StreamId) -> Effect<Svc> {
    ///     // tcp_close_stream returns TypedCall<()>, not SleepCall.
    ///     tcp_close_stream(stream).then_event(|| Msg::Done)
    /// }
    /// ```
    ///
    /// Compile-fail: `file_close` is a second non-timer `TypedCall<()>`
    /// that must surface its error. Same protection applies.
    ///
    /// ```compile_fail
    /// # use std::convert::Infallible;
    /// # use tina::prelude::*;
    /// # use tina_runtime::{file_close, FileId, RuntimeCall};
    /// # struct Svc;
    /// # #[derive(Debug)] enum Msg { Done }
    /// # impl Isolate for Svc {
    /// #     type Message = Msg;
    /// #     type Reply = ();
    /// #     type Send = tina::Outbound<Infallible>;
    /// #     type Spawn = Infallible;
    /// #     type SpawnObserved = Infallible;
    /// #     type Call = RuntimeCall<Msg>;
    /// #     type Shard = tina::SingleShard;
    /// #     fn handle(&mut self, _m: Msg, _ctx: &mut Context<'_, Self::Shard, ()>) -> Effect<Self> {
    /// #         tina::noop()
    /// #     }
    /// # }
    /// fn close(file: FileId) -> Effect<Svc> {
    ///     file_close(file).then_event(|| Msg::Done)
    /// }
    /// ```
    pub fn then_event<I, F, M>(self, event: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce() -> M + 'static,
        M: 'static,
    {
        self.inner.then(move |_| event())
    }
}

impl<I> tina::DeferThrough<I> for SleepCall
where
    I: tina::Isolate,
    I::Reply: 'static,
{
    type Deferred = DeferredTypedCall<(), I::Reply>;

    fn defer_through(self, call: tina::CallContext<'_, I>) -> Self::Deferred {
        <TypedCall<()> as tina::DeferThrough<I>>::defer_through(self.inner, call)
    }
}

impl<I> tina::RequestDeferThrough<I> for SleepCall
where
    I: tina::Isolate,
    I::Reply: 'static,
{
    type RequestDeferred = RequestDeferredTypedCall<(), I::Reply>;

    fn defer_request_through(self, call: tina::RequestCall<'_, I>) -> Self::RequestDeferred {
        <TypedCall<()> as tina::RequestDeferThrough<I>>::defer_request_through(self.inner, call)
    }
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

// ---------------------------------------------------------------------------
// CancelableWork: natural-key, multi-entry storage for PendingCancelableCall.
//
// PendingCancelableCallSet enforces unique key identity. CancelableWork lets
// many live calls share one natural key, which is the shape services need
// when the key is a real-world thing (job id, room name, customer id) and
// each request gets its own admission.
// ---------------------------------------------------------------------------

/// Move-only witness for one admitted [`PendingCancelableCall`]. Carries
/// the slot index and a generation so a stale completion against a reused
/// slot cannot remove a newer entry.
///
/// Compile-fail: ticket fields are private.
///
/// ```compile_fail
/// # use std::marker::PhantomData;
/// use tina_runtime::WorkTicket;
/// let _forged: WorkTicket<u32> = WorkTicket {
///     slot: 0,
///     generation: 0,
///     _key: PhantomData,
/// };
/// ```
pub struct WorkTicket<K> {
    slot: usize,
    generation: u64,
    _key: std::marker::PhantomData<fn(K) -> K>,
}

impl<K> WorkTicket<K> {
    fn new(slot: usize, generation: u64) -> Self {
        Self {
            slot,
            generation,
            _key: std::marker::PhantomData,
        }
    }

    /// Slot index this ticket points at. Crate-internal helpers can use
    /// this for diagnostics; user code does not need it.
    #[doc(hidden)]
    pub fn slot_index(&self) -> usize {
        self.slot
    }
}

impl<K> std::fmt::Debug for WorkTicket<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkTicket")
            .field("slot", &self.slot)
            .field("generation", &self.generation)
            .finish()
    }
}

struct CancelableWorkEntry<K, Q, R> {
    token: PendingCancelableCall<K, Q, R>,
}

/// Many cancelable requests grouped by key.
///
/// Use this when the user's job is "this key can have several in-flight
/// requests at once" — for example, every retry attempt of the same job
/// id, or several callers racing to fill the same cache key with
/// caller-owned cancellation. For the "one active request per key"
/// shape, reach for [`PendingCancelableCallSet`] instead.
///
/// Bounded fixed-capacity storage for [`PendingCancelableCall`] tokens
/// grouped by natural key. Multiple live entries may share one key, up
/// to the optional per-key cap configured at construction time.
///
/// Visible truth that stays on the surface:
///
/// - admission is `admit(token)` and returns `Full` / `KeyFull` typed
///   errors; caller authority comes back inside the rejected token.
/// - completion is `remove_by_ticket(WorkTicket)` — the ticket names this
///   exact attempt, so an older worker-return cannot remove a newer
///   admit that reused the same key.
/// - drain replies to every parked caller on stop and reports closed
///   capacity through the count surface.
pub struct CancelableWork<K, Q, R> {
    capacity: usize,
    per_key_limit: Option<usize>,
    slots: Vec<Option<CancelableWorkEntry<K, Q, R>>>,
    generations: Vec<u64>,
    high_water: usize,
    full_rejects: u64,
    key_full_rejects: u64,
    capacity_name: String,
    capacity_mode: tina::capacity::CapacityMode,
}

impl<K, Q, R> std::fmt::Debug for CancelableWork<K, Q, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self.slots.iter().filter(|s| s.is_some()).count();
        f.debug_struct("CancelableWork")
            .field("capacity", &self.capacity)
            .field("per_key_limit", &self.per_key_limit)
            .field("len", &live)
            .field("high_water", &self.high_water)
            .field("full_rejects", &self.full_rejects)
            .field("key_full_rejects", &self.key_full_rejects)
            .finish()
    }
}

/// Why [`CancelableWork::admit`] could not store the pending token.
#[derive(Debug)]
pub enum AdmitWorkError<K, Q, R> {
    /// Global capacity exhausted.
    Full {
        /// Pending token returned unchanged. Recover caller authority
        /// from this value if the handler wants to answer the original
        /// caller immediately.
        token: PendingCancelableCall<K, Q, R>,
    },
    /// Per-key capacity exhausted.
    KeyFull {
        /// Pending token returned unchanged.
        token: PendingCancelableCall<K, Q, R>,
    },
}

/// Snapshot row returned by [`CancelableWork::snapshot`].
#[derive(Debug, Clone)]
pub struct CancelableWorkSnapshot<K> {
    /// Natural key.
    pub key: K,
    /// Number of live entries for this key.
    pub entries: usize,
}

fn mint_cancelable_work_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

impl<K, Q, R> CancelableWork<K, Q, R>
where
    K: PartialEq + 'static,
    Q: 'static,
    R: 'static,
{
    /// Build an empty work set with one global capacity and no per-key cap.
    pub fn with_capacity(capacity: usize) -> Self {
        Self::build(capacity, None)
    }

    /// Build an empty work set with both global and per-key caps.
    pub fn with_key_limit(capacity: usize, per_key: usize) -> Self {
        assert!(per_key > 0, "CancelableWork per-key limit must be positive");
        Self::build(capacity, Some(per_key))
    }

    fn build(capacity: usize, per_key_limit: Option<usize>) -> Self {
        assert!(capacity > 0, "CancelableWork capacity must be positive");
        let mut slots = Vec::with_capacity(capacity);
        let mut generations = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(None);
            generations.push(0);
        }
        let seq = mint_cancelable_work_seq();
        Self {
            capacity,
            per_key_limit,
            slots,
            generations,
            high_water: 0,
            full_rejects: 0,
            key_full_rejects: 0,
            capacity_name: format!("cancelable_work.{seq}"),
            capacity_mode: tina::capacity::CapacityMode::Fixed,
        }
    }

    /// Override the capacity-report name.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.capacity_name = name.into();
        self
    }

    /// Mark the cap as `Tuning`. Cap stays hard.
    pub fn with_capacity_mode(mut self, mode: tina::capacity::CapacityMode) -> Self {
        self.capacity_mode = mode;
        self
    }

    /// Name carried in [`Self::capacity_report`].
    pub fn capacity_name(&self) -> &str {
        &self.capacity_name
    }

    /// Configured global capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Configured per-key cap, if any.
    pub fn per_key_limit(&self) -> Option<usize> {
        self.per_key_limit
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// True when no entries are stored.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(|s| s.is_none())
    }

    /// Highest live count observed since construction.
    pub fn high_water(&self) -> usize {
        self.high_water
    }

    /// Cumulative global-cap rejections.
    pub fn full_rejects(&self) -> u64 {
        self.full_rejects
    }

    /// Cumulative per-key-cap rejections.
    pub fn key_full_rejects(&self) -> u64 {
        self.key_full_rejects
    }

    /// Count-surface snapshot for capacity dashboards.
    pub fn capacity_report(&self) -> tina::capacity::CapacitySurfaceReport {
        tina::capacity::CapacitySurfaceReport::count(
            self.capacity_name.clone(),
            self.capacity_mode.clone(),
            self.capacity,
            self.len(),
            self.high_water,
            self.full_rejects,
        )
    }

    fn count_for_key(&self, key: &K) -> usize {
        self.slots
            .iter()
            .filter(|s| s.as_ref().is_some_and(|e| e.token.key() == key))
            .count()
    }

    /// Admit `token` under its own natural key. Returns a [`WorkTicket`]
    /// on success; on rejection, the pending token is returned through
    /// the error so the handler can recover caller authority and answer
    /// immediately.
    pub fn admit(
        &mut self,
        token: PendingCancelableCall<K, Q, R>,
    ) -> Result<WorkTicket<K>, AdmitWorkError<K, Q, R>> {
        if let Some(limit) = self.per_key_limit {
            if self.count_for_key(token.key()) >= limit {
                self.key_full_rejects += 1;
                return Err(AdmitWorkError::KeyFull { token });
            }
        }
        if self.len() >= self.capacity {
            self.full_rejects += 1;
            return Err(AdmitWorkError::Full { token });
        }

        let idx = self
            .slots
            .iter()
            .position(|s| s.is_none())
            .expect("admission checked capacity above");
        self.generations[idx] = self.generations[idx].wrapping_add(1);
        let generation = self.generations[idx];
        self.slots[idx] = Some(CancelableWorkEntry { token });
        let cur = self.len();
        if cur > self.high_water {
            self.high_water = cur;
        }
        Ok(WorkTicket::new(idx, generation))
    }

    /// Remove and return the pending token named by `ticket`. Returns
    /// `None` when the ticket is stale or the slot is empty (both
    /// "completion arrived after we already removed it" outcomes).
    pub fn take(&mut self, ticket: WorkTicket<K>) -> Option<PendingCancelableCall<K, Q, R>> {
        if ticket.slot >= self.slots.len() {
            return None;
        }
        if self.generations[ticket.slot] != ticket.generation {
            return None;
        }
        self.slots[ticket.slot].take().map(|entry| entry.token)
    }

    /// Drain every stored token, freeing all capacity.
    pub fn drain(&mut self) -> impl Iterator<Item = PendingCancelableCall<K, Q, R>> + '_ {
        self.slots
            .iter_mut()
            .filter_map(|s| s.take())
            .map(|entry| entry.token)
    }
}

impl<K, Q, R> CancelableWork<K, Q, R>
where
    K: PartialEq + Clone + 'static,
    Q: 'static,
    R: 'static,
{
    /// Per-key snapshot. Empty keys are omitted.
    pub fn snapshot(&self) -> Vec<CancelableWorkSnapshot<K>> {
        let mut out: Vec<CancelableWorkSnapshot<K>> = Vec::new();
        for slot in self.slots.iter() {
            if let Some(entry) = slot.as_ref() {
                let key = entry.token.key();
                if let Some(row) = out.iter_mut().find(|row| &row.key == key) {
                    row.entries += 1;
                } else {
                    out.push(CancelableWorkSnapshot {
                        key: key.clone(),
                        entries: 1,
                    });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod pending_cancelable_call_set_tests {
    use std::any::TypeId;
    use std::sync::Arc;

    use super::*;

    fn token<K>(key: K, slot_id: u64) -> PendingCancelableCall<K, &'static str, ()> {
        let deferred_shared = Arc::new(tina::DeferredSlotShared::new(
            slot_id,
            TypeId::of::<&'static str>(),
        ));
        let deferred = tina::runtime_internal::deferred_from_handle(
            tina::runtime_internal::handle_from_shared(deferred_shared),
        );
        let request = tina::runtime_internal::request_context_from_deferred(deferred);
        let call_shared = Arc::new(tina::CallHandleShared::new(TypeId::of::<()>()));
        let handle = tina::runtime_internal::call_handle_from_shared(call_shared);

        PendingCancelableCall {
            key,
            ticket: PendingCancelableTicket(slot_id),
            request,
            handle,
        }
    }

    #[test]
    fn pending_call_set_cancelable_insert_full_duplicate() {
        let mut set = PendingCancelableCallSet::with_capacity(2);
        let first = set.try_insert(token(1, 1)).expect("first insert");
        let second = set.try_insert(token(2, 2)).expect("second insert");

        assert_ne!(first, second);
        assert!(set.is_full());
        assert_eq!(set.len(), 2);

        match set.try_insert(token(1, 3)) {
            Err(PendingCancelableInsertError::DuplicateKey { token }) => {
                assert_eq!(*token.key(), 1);
                assert_eq!(token.into_request_context().slot_id(), 3);
            }
            _ => panic!("expected DuplicateKey"),
        }

        match set.try_insert(token(3, 4)) {
            Err(PendingCancelableInsertError::Full { token }) => {
                assert_eq!(*token.key(), 3);
                assert_eq!(token.into_request_context().slot_id(), 4);
            }
            _ => panic!("expected Full"),
        }

        assert_eq!(set.len(), 2);
    }

    #[test]
    fn pending_call_set_cancelable_remove_requires_exact_ticket() {
        let mut set = PendingCancelableCallSet::with_capacity(2);
        let old_ticket = set.try_insert(token(7, 10)).expect("insert old");

        assert_eq!(
            set.remove(&99, old_ticket).unwrap_err(),
            PendingCancelableRemoveError::MissingKey
        );

        let removed = set.remove(&7, old_ticket).expect("remove old");
        assert_eq!(removed.into_request_context().slot_id(), 10);

        let new_ticket = set.try_insert(token(7, 11)).expect("reuse key");
        assert_ne!(old_ticket, new_ticket);

        assert_eq!(
            set.remove(&7, old_ticket).unwrap_err(),
            PendingCancelableRemoveError::StaleTicket,
            "old completion must not remove newer token under reused key",
        );

        let removed = set.remove(&7, new_ticket).expect("remove new");
        assert_eq!(removed.into_request_context().slot_id(), 11);
        assert!(set.is_empty());
    }

    #[test]
    fn pending_call_set_cancelable_fill_cancel_refill_shape() {
        let mut set = PendingCancelableCallSet::with_capacity(2);
        let a = set.try_insert(token(1, 1)).expect("insert a");
        let b = set.try_insert(token(2, 2)).expect("insert b");
        assert!(set.is_full());

        let _ = set.remove(&1, a).expect("cancel a");
        let _ = set.remove(&2, b).expect("cancel b");
        assert!(set.is_empty());

        set.try_insert(token(3, 3)).expect("refill a");
        set.try_insert(token(4, 4)).expect("refill b");
        assert!(set.is_full());
    }

    #[test]
    fn pending_call_set_cancelable_drain_returns_all_tokens_for_settlement() {
        let mut set = PendingCancelableCallSet::with_capacity(3);
        set.try_insert(token(1, 1)).expect("insert 1");
        set.try_insert(token(2, 2)).expect("insert 2");

        let slots: Vec<_> = set
            .drain()
            .map(|token| token.into_request_context().slot_id())
            .collect();

        assert_eq!(slots, [1, 2]);
        assert!(set.is_empty());
        set.try_insert(token(3, 3)).expect("refill after drain");
    }

    #[test]
    fn pending_call_set_cancelable_key_need_not_clone() {
        #[derive(Debug, PartialEq)]
        struct Key(u8);

        let mut set = PendingCancelableCallSet::with_capacity(1);
        let ticket = set.try_insert(token(Key(1), 1)).expect("insert");
        let removed = set.remove(&Key(1), ticket).expect("remove");

        assert_eq!(removed.into_request_context().slot_id(), 1);
    }

    #[test]
    #[should_panic(expected = "capacity > 0")]
    fn pending_call_set_cancelable_zero_capacity_panics() {
        let _set: PendingCancelableCallSet<u64, (), ()> =
            PendingCancelableCallSet::with_capacity(0);
    }
}

#[cfg(test)]
mod cancelable_work_tests {
    use std::any::TypeId;
    use std::sync::Arc;

    use super::*;

    fn token<K>(key: K, slot_id: u64) -> PendingCancelableCall<K, &'static str, ()> {
        let deferred_shared = Arc::new(tina::DeferredSlotShared::new(
            slot_id,
            TypeId::of::<&'static str>(),
        ));
        let deferred = tina::runtime_internal::deferred_from_handle(
            tina::runtime_internal::handle_from_shared(deferred_shared),
        );
        let request = tina::runtime_internal::request_context_from_deferred(deferred);
        let call_shared = Arc::new(tina::CallHandleShared::new(TypeId::of::<()>()));
        let handle = tina::runtime_internal::call_handle_from_shared(call_shared);

        PendingCancelableCall {
            key,
            ticket: PendingCancelableTicket(slot_id),
            request,
            handle,
        }
    }

    #[test]
    fn admit_two_entries_same_natural_key() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(4);
        let t1 = work.admit(token(7, 10)).unwrap();
        let t2 = work.admit(token(7, 20)).unwrap();
        assert_eq!(work.len(), 2);
        let removed = work.take(t1).unwrap();
        assert_eq!(removed.key(), &7);
        assert_eq!(work.len(), 1);
        let removed = work.take(t2).unwrap();
        assert_eq!(removed.key(), &7);
        assert!(work.is_empty());
    }

    #[test]
    fn global_full_returns_token_back() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(1);
        work.admit(token(1, 10)).unwrap();
        match work.admit(token(2, 20)) {
            Err(AdmitWorkError::Full { token: rejected }) => {
                assert_eq!(rejected.key(), &2);
                assert_eq!(work.full_rejects(), 1);
            }
            other => panic!("expected Full, got {other:?}"),
        }
    }

    #[test]
    fn per_key_full_returns_token_back() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_key_limit(8, 2);
        work.admit(token(1, 10)).unwrap();
        work.admit(token(1, 11)).unwrap();
        match work.admit(token(1, 12)) {
            Err(AdmitWorkError::KeyFull { token: rejected }) => {
                assert_eq!(rejected.key(), &1);
                assert_eq!(work.key_full_rejects(), 1);
            }
            other => panic!("expected KeyFull, got {other:?}"),
        }
        // Different key still admits.
        work.admit(token(2, 13)).unwrap();
    }

    #[test]
    fn drain_releases_every_entry() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(4);
        work.admit(token(1, 10)).unwrap();
        work.admit(token(1, 11)).unwrap();
        work.admit(token(2, 20)).unwrap();
        let drained: Vec<_> = work.drain().collect();
        assert_eq!(drained.len(), 3);
        assert!(work.is_empty());
    }

    #[test]
    fn stale_completion_cannot_remove_newer_ticket() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(1);
        let t1 = work.admit(token(1, 10)).unwrap();
        // Take by ticket. Slot empties.
        let _ = work.take(t1);
        // Admit a new entry into the same slot.
        let _t2 = work.admit(token(2, 20)).unwrap();
        // t1 is already moved; we can't construct a stale ticket
        // explicitly because WorkTicket::new is private. The compile-
        // fail proof covers the forge and double-use paths.
        assert_eq!(work.len(), 1);
    }

    #[test]
    fn snapshot_groups_entries_by_key() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(4);
        work.admit(token(1, 10)).unwrap();
        work.admit(token(1, 11)).unwrap();
        work.admit(token(2, 20)).unwrap();
        let mut snap: Vec<(u32, usize)> = work
            .snapshot()
            .into_iter()
            .map(|s| (s.key, s.entries))
            .collect();
        snap.sort();
        assert_eq!(snap, vec![(1, 2), (2, 1)]);
    }

    #[test]
    fn capacity_report_tracks_high_water_and_full() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(2).named("work.set");
        let t1 = work.admit(token(1, 10)).unwrap();
        work.admit(token(2, 11)).unwrap();
        let _ = work.take(t1);
        work.admit(token(3, 12)).unwrap();
        // Over cap.
        let _ = work.admit(token(4, 13)).unwrap_err();
        let report = work.capacity_report();
        assert_eq!(report.name, "work.set");
        assert_eq!(report.max_messages, Some(2));
        assert_eq!(report.high_water_messages, 2);
        assert_eq!(report.full_count, 1);
    }

    #[test]
    fn fill_drain_refill() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(2);
        work.admit(token(1, 10)).unwrap();
        work.admit(token(2, 11)).unwrap();
        let _: Vec<_> = work.drain().collect();
        work.admit(token(3, 12)).unwrap();
        work.admit(token(4, 13)).unwrap();
        assert_eq!(work.len(), 2);
    }

    #[test]
    #[should_panic(expected = "CancelableWork capacity must be positive")]
    fn zero_capacity_panics() {
        let _: CancelableWork<u32, &'static str, ()> = CancelableWork::with_capacity(0);
    }
}
