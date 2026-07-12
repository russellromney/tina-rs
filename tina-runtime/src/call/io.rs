//! Closed-set vocabulary for runtime-owned call I/O.
//!
//! These enums define what a runtime can do on behalf of an isolate
//! ([`CallInput`]), what it can return ([`CallOutput`]), why it can
//! fail ([`CallError`]), and the result shapes the caller sees
//! ([`CallOutcome`], [`SendOutcome`]). They are deliberately small,
//! Debug + Clone, and back-end neutral: a different runtime (live,
//! simulator, alternate backend) can implement the same set without
//! widening the surface.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use super::types::{
    FileId, FileOpenOptions, JournalReplay, PathMetadata, ProcessStatus, SnapshotImage,
};
use super::{
    ListenerId, StreamId, TlsListenerId, TlsStreamId, UdpSocketId, UnixListenerId, UnixStreamId,
};

/// Successful owned-buffer TCP read reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpReadBufReply {
    /// Caller-owned buffer returned by the runtime.
    pub buffer: Vec<u8>,
    /// Valid bytes in `buffer[..len]`.
    pub len: usize,
}

/// Owned-buffer TCP read failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpReadBufError {
    /// The runtime failure.
    pub error: CallError,
    /// Caller-owned buffer returned by the runtime.
    pub buffer: Vec<u8>,
}

/// Successful owned-buffer write reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOwnedReply {
    /// Caller-owned bytes returned by the runtime.
    pub bytes: Vec<u8>,
    /// Bytes accepted by the stream.
    pub written: usize,
}

/// Owned-buffer write failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOwnedError {
    /// The runtime failure.
    pub error: CallError,
    /// Caller-owned bytes returned by the runtime.
    pub bytes: Vec<u8>,
}

/// TCP compatibility name for [`WriteOwnedReply`].
pub type TcpWriteOwnedReply = WriteOwnedReply;

/// TCP compatibility name for [`WriteOwnedError`].
pub type TcpWriteOwnedError = WriteOwnedError;

/// Successful owned-buffer TCP terminal write reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpWriteOwnedCloseReply {
    /// Caller-owned bytes returned by the runtime.
    pub bytes: Vec<u8>,
    /// Bytes accepted by the stream.
    pub written: usize,
    /// True when the full buffer was accepted and the stream was closed.
    ///
    /// Partial writes leave the stream open so the caller can issue another
    /// terminal write for the remaining bytes instead of truncating the wire.
    pub closed: bool,
}

/// Successful owned-buffer TLS read reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsReadBufReply {
    /// Caller-owned plaintext buffer returned by the runtime.
    pub buffer: Vec<u8>,
    /// Valid plaintext bytes in `buffer[..len]`.
    pub len: usize,
}

/// Owned-buffer TLS read failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsReadBufError {
    /// The runtime failure.
    pub error: CallError,
    /// Caller-owned plaintext buffer returned by the runtime.
    pub buffer: Vec<u8>,
}

/// Successful owned-buffer TLS write reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsWriteOwnedReply {
    /// Caller-owned plaintext bytes returned by the runtime.
    pub bytes: Vec<u8>,
    /// Plaintext bytes accepted by the TLS stream.
    pub written: usize,
}

/// Owned-buffer TLS write failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsWriteOwnedError {
    /// The runtime failure.
    pub error: CallError,
    /// Caller-owned plaintext bytes returned by the runtime.
    pub bytes: Vec<u8>,
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

    /// Read up to `max_len` bytes from a stream into caller-owned storage.
    ///
    /// The runtime returns the same buffer and reports the valid prefix in
    /// `buffer[..len]`. EOF remains `len == 0`.
    TcpReadBuf {
        /// The stream to read from.
        stream: StreamId,
        /// Reusable caller-owned storage.
        buffer: Vec<u8>,
        /// The maximum number of bytes the runtime may deliver.
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

    /// Write caller-owned bytes to a stream and return them with the accepted
    /// byte count.
    TcpWriteOwned {
        /// The stream to write to.
        stream: StreamId,
        /// The payload to write.
        bytes: Vec<u8>,
        /// First byte in `bytes` to submit.
        start: usize,
    },

    /// Write caller-owned bytes to a stream and close the stream only if the
    /// full buffer is accepted.
    ///
    /// Partial writes return the bytes and accepted count with
    /// `closed == false`; the caller must retry the remaining bytes or close
    /// deliberately.
    TcpWriteOwnedClose {
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
        /// ALPN protocols to offer, in preference order, as raw wire bytes
        /// (e.g. `b"h2"`). Empty means no ALPN extension is offered. When
        /// non-empty and the peer negotiates none, the connect fails with
        /// [`CallError::TlsAlpnMismatch`].
        alpn_protocols: Vec<Vec<u8>>,
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
        /// ALPN protocols the server will accept, in preference order, as
        /// raw wire bytes. Empty means the server does not negotiate ALPN.
        alpn_protocols: Vec<Vec<u8>>,
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

    /// Read decrypted bytes from a TLS stream into caller-owned storage.
    TlsReadBuf {
        /// TLS stream to read from.
        stream: TlsStreamId,
        /// Reusable caller-owned plaintext storage.
        buffer: Vec<u8>,
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

    /// Write caller-owned plaintext bytes to a TLS stream and return them with
    /// the accepted plaintext byte count.
    TlsWriteOwned {
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

    /// Write bytes at `offset` and return the caller-owned buffer.
    FileWriteAtOwned {
        /// The file to write.
        file: FileId,
        /// Caller-owned bytes to write.
        bytes: Vec<u8>,
        /// File offset.
        offset: u64,
        /// First byte in `bytes` to submit.
        start: usize,
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

    /// Bind a Unix-domain listener at `path`.
    UnixBind {
        /// Filesystem path the listener should bind to.
        path: PathBuf,
    },

    /// Accept one inbound Unix stream on a previously-bound listener.
    UnixAccept {
        /// The Unix listener to accept on.
        listener: UnixListenerId,
    },

    /// Connect one outbound Unix stream to `path`.
    UnixConnect {
        /// The peer path to connect to.
        path: PathBuf,
    },

    /// Read up to `max_len` bytes from a Unix stream.
    UnixRead {
        /// The Unix stream to read from.
        stream: UnixStreamId,
        /// The maximum number of bytes the runtime may deliver.
        max_len: usize,
    },

    /// Write `bytes` to a Unix stream. Partial writes are surfaced
    /// through [`CallOutput::UnixWrote`].
    UnixWrite {
        /// The Unix stream to write to.
        stream: UnixStreamId,
        /// The payload to write.
        bytes: Vec<u8>,
    },

    /// Write `bytes` to a Unix stream and return the caller-owned buffer.
    UnixWriteOwned {
        /// The Unix stream to write to.
        stream: UnixStreamId,
        /// Caller-owned payload.
        bytes: Vec<u8>,
        /// First byte in `bytes` to submit.
        start: usize,
    },

    /// Close a Unix listener and release its resources. The runtime
    /// removes the underlying socket file when the listener is closed.
    UnixListenerClose {
        /// The Unix listener to close.
        listener: UnixListenerId,
    },

    /// Close a Unix stream and release its resources.
    UnixStreamClose {
        /// The Unix stream to close.
        stream: UnixStreamId,
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
            Self::TcpRead { .. } | Self::TcpReadBuf { .. } => crate::trace::CallKind::TcpRead,
            Self::TcpWrite { .. } | Self::TcpWriteOwned { .. } => crate::trace::CallKind::TcpWrite,
            Self::TcpWriteOwnedClose { .. } => crate::trace::CallKind::TcpWriteClose,
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
            Self::TlsRead { .. } | Self::TlsReadBuf { .. } => crate::trace::CallKind::TlsRead,
            Self::TlsWrite { .. } | Self::TlsWriteOwned { .. } => crate::trace::CallKind::TlsWrite,
            Self::TlsClose { .. } => crate::trace::CallKind::TlsClose,
            Self::DnsLookup { .. } => crate::trace::CallKind::DnsLookup,
            Self::SignalWait { .. } => crate::trace::CallKind::SignalWait,
            Self::ProcessRun { .. } => crate::trace::CallKind::ProcessRun,
            Self::FileOpen { .. } => crate::trace::CallKind::FileOpen,
            Self::FileReadAt { .. } => crate::trace::CallKind::FileReadAt,
            Self::FileWriteAt { .. } | Self::FileWriteAtOwned { .. } => {
                crate::trace::CallKind::FileWriteAt
            }
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
            Self::UnixBind { .. } => crate::trace::CallKind::UnixBind,
            Self::UnixAccept { .. } => crate::trace::CallKind::UnixAccept,
            Self::UnixConnect { .. } => crate::trace::CallKind::UnixConnect,
            Self::UnixRead { .. } => crate::trace::CallKind::UnixRead,
            Self::UnixWrite { .. } | Self::UnixWriteOwned { .. } => {
                crate::trace::CallKind::UnixWrite
            }
            Self::UnixListenerClose { .. } => crate::trace::CallKind::UnixListenerClose,
            Self::UnixStreamClose { .. } => crate::trace::CallKind::UnixStreamClose,
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

    /// A reusable-buffer TCP read completed.
    TcpReadBuf {
        /// Caller-owned buffer returned by the runtime.
        buffer: Vec<u8>,
        /// Valid bytes in `buffer[..len]`.
        len: usize,
    },

    /// A reusable-buffer TCP read failed and returned its buffer.
    TcpReadBufFailed {
        /// Caller-owned buffer returned by the runtime.
        buffer: Vec<u8>,
        /// Runtime failure.
        error: CallError,
    },

    /// A write moved `count` bytes onto the stream. The issuing isolate is
    /// responsible for issuing another write if `count` is less than the
    /// requested length.
    TcpWrote {
        /// The number of bytes the runtime accepted.
        count: usize,
    },

    /// A reusable-buffer TCP write completed.
    TcpWroteOwned {
        /// Caller-owned bytes returned by the runtime.
        bytes: Vec<u8>,
        /// Bytes accepted by the stream.
        count: usize,
    },

    /// A reusable-buffer TCP terminal write completed.
    TcpWroteOwnedClose {
        /// Caller-owned bytes returned by the runtime.
        bytes: Vec<u8>,
        /// Bytes accepted by the stream.
        count: usize,
        /// Whether the runtime closed the stream after the write.
        closed: bool,
    },

    /// A reusable-buffer TCP write failed and returned its bytes.
    TcpWroteOwnedFailed {
        /// Caller-owned bytes returned by the runtime.
        bytes: Vec<u8>,
        /// Runtime failure.
        error: CallError,
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
        /// ALPN protocol the peer selected (raw wire bytes), or `None`
        /// when no ALPN was offered/negotiated.
        selected_alpn: Option<Vec<u8>>,
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
        /// ALPN protocol negotiated with the client (raw wire bytes), or
        /// `None` when no ALPN was offered/negotiated.
        selected_alpn: Option<Vec<u8>>,
    },

    /// A TLS stream read decrypted bytes.
    TlsRead {
        /// Decrypted bytes.
        bytes: Vec<u8>,
    },

    /// A reusable-buffer TLS read completed.
    TlsReadBuf {
        /// Caller-owned plaintext buffer returned by the runtime.
        buffer: Vec<u8>,
        /// Valid plaintext bytes in `buffer[..len]`.
        len: usize,
    },

    /// A reusable-buffer TLS read failed and returned its buffer.
    TlsReadBufFailed {
        /// Caller-owned plaintext buffer returned by the runtime.
        buffer: Vec<u8>,
        /// Runtime failure.
        error: CallError,
    },

    /// A TLS stream wrote plaintext bytes.
    TlsWrote {
        /// Plaintext byte count accepted by the TLS stream.
        count: usize,
    },

    /// A reusable-buffer TLS write completed.
    TlsWroteOwned {
        /// Caller-owned plaintext bytes returned by the runtime.
        bytes: Vec<u8>,
        /// Plaintext byte count accepted by the TLS stream.
        count: usize,
    },

    /// A reusable-buffer TLS write failed and returned its bytes.
    ///
    /// The returned bytes are the caller-owned plaintext allocation. The error
    /// still decides retry safety; for TLS, some plaintext may already have
    /// been accepted by the connection state before a later transport failure.
    TlsWroteOwnedFailed {
        /// Caller-owned plaintext bytes returned by the runtime.
        bytes: Vec<u8>,
        /// Runtime failure.
        error: CallError,
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

    /// A positional file write completed and returned its buffer.
    FileWroteOwned {
        /// Caller-owned bytes returned by the runtime.
        bytes: Vec<u8>,
        /// The number of bytes written.
        count: usize,
    },

    /// A positional file write failed and returned its buffer.
    FileWroteOwnedFailed {
        /// Caller-owned bytes returned by the runtime.
        bytes: Vec<u8>,
        /// Runtime failure.
        error: CallError,
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

    /// A Unix listener was bound and is ready to accept.
    UnixBound {
        /// Runtime-assigned Unix listener id.
        listener: UnixListenerId,
        /// Bound socket path.
        path: PathBuf,
    },

    /// A Unix stream was accepted.
    UnixAccepted {
        /// Runtime-assigned Unix stream id.
        stream: UnixStreamId,
    },

    /// A Unix stream connect completed.
    UnixConnected {
        /// Runtime-assigned Unix stream id.
        stream: UnixStreamId,
    },

    /// A Unix stream read returned bytes. An empty `bytes` vector means
    /// end of stream.
    UnixRead {
        /// The bytes the runtime read from the stream.
        bytes: Vec<u8>,
    },

    /// A Unix stream write moved `count` bytes onto the stream.
    UnixWrote {
        /// Bytes accepted by the runtime.
        count: usize,
    },

    /// A Unix stream write completed and returned its buffer.
    UnixWroteOwned {
        /// Caller-owned bytes returned by the runtime.
        bytes: Vec<u8>,
        /// Bytes accepted by the runtime.
        count: usize,
    },

    /// A Unix stream write failed and returned its buffer.
    UnixWroteOwnedFailed {
        /// Caller-owned bytes returned by the runtime.
        bytes: Vec<u8>,
        /// Runtime failure.
        error: CallError,
    },

    /// A Unix listener was closed and its resources released.
    UnixListenerClosed,

    /// A Unix stream was closed and its resources released.
    UnixStreamClosed,

    /// The runtime could not complete the call. The trace already records
    /// the failure with a richer reason; this variant is what the issuing
    /// isolate observes in its own vocabulary.
    Failed(CallError),
}

/// Why a runtime-owned call failed before it could complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallError {
    /// A runtime completion violated its own call contract.
    InvariantViolation,

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

    /// ALPN was offered on a TLS connect/accept but no protocol was
    /// negotiated (the peer selected none, or selected one that was not
    /// offered). Distinct from `TlsHandshake` so an ALPN mismatch is not
    /// confused with a cert/name/handshake failure.
    TlsAlpnMismatch,

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

    /// The bounded runtime timer lane was full when the runtime tried to arm a
    /// sleep. Mirrors the other `*Full` overload outcomes: the caller sees the
    /// timer refused instead of the lane growing without bound.
    TimerFull,
}

/// User-visible outcome for an observed send.
///
/// Ordinary [`tina::send`] stays fire-and-forget. This outcome is only
/// produced by [`crate::send_observed`], for code that needs explicit overload
/// policy in normal isolate message flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SendOutcome {
    /// The target address belongs to another runtime/system incarnation.
    ForeignSystem {
        /// Incarnation owned by the routing runtime.
        expected: tina::SystemIncarnation,
        /// Incarnation carried by the target address.
        actual: tina::SystemIncarnation,
    },

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
            crate::trace::SendRejectedReason::ForeignSystem { expected, actual } => {
                Self::ForeignSystem { expected, actual }
            }
            crate::trace::SendRejectedReason::Full => Self::Full,
            crate::trace::SendRejectedReason::Closed => Self::Closed,
        }
    }
}
