//! Runtime-owned substrate driver for `tina-runtime`.
//!
//! Tina keeps isolate scheduling, mailboxes, tracing, supervision, and call
//! outcome delivery in [`crate::Runtime`]. The driver owns only substrate
//! operations: timers, TCP resources, storage work, completion readiness, and
//! cancellation.
//!
//! ## Contract with the rest of the runtime
//!
//! - one [`RuntimeDriver::submit`] per [`CallInput`] issued by an isolate.
//! - one [`RuntimeDriver::advance`] per [`crate::Runtime::step`].
//! - the driver appends [`DriverCompletion`] values into runtime-owned scratch
//!   storage; `Runtime` translates them into ordinary later-turn messages and
//!   trace events.
//! - resource ids ([`ListenerId`], [`StreamId`]) are runtime-assigned
//!   monotonic counters, not OS file descriptors. Isolate code never sees
//!   raw fds or `Box<dyn IOSocket>` values.
//! - operations are classified by blocking shape:
//!   - inline-safe: small runtime bookkeeping such as TCP close;
//!   - driver-completion: Betelgeuse completion-shaped TCP and file calls;
//!   - storage-lane: snapshot/journal helpers that can block on local durable
//!     filesystem work and therefore use a bounded worker lane;
//!   - forbidden-in-handler: direct filesystem or socket work performed by
//!     user handlers instead of returned Tina effects.
//! - shutdown cancellation keeps completion slots alive until the backend
//!   reports that it no longer owns their raw pointers. A driver that cannot
//!   prove release returns [`DriverShutdownError`] instead of pretending
//!   shutdown was clean.
//!
use std::alloc::Global;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{
    Receiver, SyncSender, TryRecvError, TrySendError as MpscTrySendError, sync_channel,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use betelgeuse::{
    AcceptCompletion, ConnectCompletion, FsyncCompletion, IO, IOFile, IOLoop, IOLoopHandle,
    IOSocket, MkdirCompletion, OpenOptions, PReadCompletion, PWriteCompletion, RecvCompletion,
    SendCompletion, SizeCompletion, io_loop,
};

use crate::call::{
    CallError, CallId, CallInput, CallOutput, FileId, FileOpenOptions, ListenerId, PathKind,
    PathMetadata, ProcessStatus, StreamId, TlsListenerId, TlsStreamId, UdpSocketId,
};

mod process;
mod signals;

use process::{ProcessCommand, ProcessLane};
pub use signals::os_signal_capture_supported;
use signals::{OsSignalDispatcher, SignalWaitEntry};

const INITIAL_DRIVER_TIMER_CAPACITY: usize = 8;
const INITIAL_DRIVER_RESOURCE_CAPACITY: usize = 4;
const INITIAL_DRIVER_PENDING_CAPACITY: usize = 8;
pub(crate) const DEFAULT_STORAGE_LANE_CAPACITY: usize = 64;
pub(crate) const DEFAULT_DNS_LANE_CAPACITY: usize = 16;
pub(crate) const DEFAULT_TLS_LANE_CAPACITY: usize = 64;
pub(crate) const DEFAULT_PROCESS_LANE_CAPACITY: usize = 16;
pub(crate) const DEFAULT_SIGNAL_CAPACITY: usize = 64;

type TlsClientStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;
type TlsServerStream = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;

enum TlsRuntimeStream {
    Client(TlsClientStream),
    Server(TlsServerStream),
}

impl Read for TlsRuntimeStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Client(stream) => stream.read(buf),
            Self::Server(stream) => stream.read(buf),
        }
    }
}

impl Write for TlsRuntimeStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Client(stream) => stream.write(buf),
            Self::Server(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Client(stream) => stream.flush(),
            Self::Server(stream) => stream.flush(),
        }
    }
}

impl TlsRuntimeStream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Client(stream) => &stream.sock,
            Self::Server(stream) => &stream.sock,
        }
    }

    fn send_close_notify(&mut self) {
        match self {
            Self::Client(stream) => stream.conn.send_close_notify(),
            Self::Server(stream) => stream.conn.send_close_notify(),
        }
    }
}

/// Runtime-owned substrate driver.
///
/// A driver must not run user isolate code, own isolate mailboxes, or hide an
/// unbounded executor behind Tina. It owns only substrate calls submitted by
/// [`crate::Runtime`], advances them when the runtime asks, and returns typed
/// completions for the runtime to deliver on later turns.
///
/// TCP drivers treat listener accept, stream read, and stream write as
/// separate lanes. Duplicate work on one lane fails with
/// [`CallError::ResourceBusy`]. Close cancels pending work on the
/// resource and closes; the cancelled caller's continuation does not
/// fire. Per-call cancel stops the runtime from waiting on this id;
/// it does not invalidate other lanes.
pub(crate) trait RuntimeDriver: std::fmt::Debug {
    /// Submits one runtime-owned call.
    fn submit(
        &mut self,
        call_id: CallId,
        request: CallInput,
        now: Instant,
    ) -> Option<DriverCompletion>;

    /// Advances the substrate by one runtime step and appends ready
    /// completions in deterministic order.
    fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>);

    /// Returns whether substrate completions are still pending.
    fn has_pending(&self) -> bool;

    /// Cancels pending substrate operations during runtime shutdown.
    ///
    /// `deadline` bounds how long lane workers may keep draining after
    /// cancellation has been signaled. If the deadline elapses before a
    /// lane finishes, shutdown returns; the lane's residual work shows up
    /// on the next [`resource_report`](Self::resource_report) call so the
    /// terminal shutdown report can name what was left.
    ///
    /// After this returns `Ok(())`, the driver may be dropped without leaving
    /// backend-owned pointers to driver-owned completion storage. `Err` means
    /// shutdown reached a typed lifecycle failure.
    fn cancel_pending(&mut self, deadline: Instant) -> Result<(), DriverShutdownError>;

    /// Cancels one runtime-owned call by id.
    ///
    /// Cancellation removes completion delivery responsibility from the
    /// driver. It is not a promise that an already-submitted substrate side
    /// effect, such as a TCP write handed to the OS, can be undone.
    fn cancel(&mut self, call_id: CallId) -> bool;

    /// Drains call ids the close path silently cancelled. Runtime
    /// drops their tracking and records `ResourceClosed`.
    fn take_cancelled_by_close(&mut self) -> Vec<CallId> {
        Vec::new()
    }

    /// Injects one runtime-owned signal event and appends ready completions.
    fn notify_signal(&mut self, _name: &str, _completed: &mut Vec<DriverCompletion>) {}

    /// Returns the driver-owned resource inventory visible to live reports.
    fn resource_report(&self) -> DriverResourceReport {
        DriverResourceReport::default()
    }

    #[cfg(test)]
    fn io_pending_count(&self) -> usize {
        0
    }
}

/// Driver-owned resource inventory.
///
/// Three independent count vocabularies, per Phase 043 plan:
///
/// * **Table-owned resources** (`listeners`, `streams`, `tls_streams`,
///   `udp_sockets`, `files`): runtime-table ids handed back to user code.
///   `owned_resource_count()` sums these.
/// * **Worker-held resources** (`worker_held`): clones of OS handles or
///   `std::process::Child` parked inside in-flight lane work that is not
///   represented by a table id. TLS in-flight ops hold cloned
///   listener/stream `Arc`s; process calls hold a live `Child`.
/// * **Pending driver calls** (`pending_calls`): runtime-owned operations
///   waiting for completion. Includes table-owned ops (TCP read/write,
///   file ops, UDP recv), TLS ops, DNS lookups, storage jobs, process
///   calls, signal waits, and timers.
///
/// Worker-held and pending may overlap (TLS in-flight is both) but
/// table-owned never overlaps the other two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DriverResourceReport {
    pub(crate) listeners: usize,
    pub(crate) streams: usize,
    pub(crate) tls_streams: usize,
    pub(crate) udp_sockets: usize,
    pub(crate) files: usize,
    pub(crate) worker_held: usize,
    pub(crate) pending_calls: usize,
}

impl DriverResourceReport {
    pub(crate) const fn owned_resource_count(self) -> usize {
        self.listeners + self.streams + self.tls_streams + self.udp_sockets + self.files
    }

    pub(crate) const fn worker_held_resource_count(self) -> usize {
        self.worker_held
    }

    pub(crate) const fn pending_driver_call_count(self) -> usize {
        self.pending_calls
    }
}

/// Driver-level shutdown failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriverShutdownError {
    /// The backend still reports ownership of completion slots after Tina's
    /// bounded shutdown drain.
    BackendStillOwnsCompletions,
}

/// Betelgeuse-backed runtime driver.
///
/// Betelgeuse exposes a `step()`-driven, no-runtime, no-hidden-tasks I/O
/// library with caller-owned typed completion slots. That shape matches Tina's
/// explicit-stepping, runtime-owned-effects discipline.
pub(crate) struct BetelgeuseDriver {
    tcp: BetelgeuseTcp,
    storage: StorageLane,
    dns: DnsLane,
    tls: TlsLane,
    process: ProcessLane,
    signals: Vec<SignalWaitEntry>,
    signal_capacity: usize,
    timers: Vec<TimerEntry>,
    next_timer_ordinal: u64,
    os_signals: OsSignalDispatcher,
}

/// One pending timer tracked by the driver.
#[derive(Debug)]
struct TimerEntry {
    call_id: CallId,
    deadline: Instant,
    insertion_order: u64,
}

/// Runtime-owned Betelgeuse TCP state.
///
/// Owns all real socket state. Isolate code only ever sees the runtime's
/// opaque [`ListenerId`] / [`StreamId`] values.
struct BetelgeuseTcp {
    io_loop: IOLoopHandle<Global>,
    next_listener_id: u64,
    next_stream_id: u64,
    next_udp_socket_id: u64,
    next_file_id: u64,
    listeners: Vec<ListenerEntry>,
    streams: Vec<StreamEntry>,
    udp_sockets: Vec<UdpSocketEntry>,
    files: Vec<FileEntry>,
    pending: Vec<PendingOperation>,
    /// Calls cancelled by resource close. Runtime drains and traces them.
    cancelled_by_close: Vec<CallId>,
}

struct ListenerEntry {
    id: ListenerId,
    socket: Box<dyn IOSocket>,
}

struct StreamEntry {
    id: StreamId,
    socket: Box<dyn IOSocket>,
}

struct UdpSocketEntry {
    id: UdpSocketId,
    socket: UdpSocket,
}

struct FileEntry {
    id: FileId,
    file: Box<dyn IOFile>,
}

struct PendingOperation {
    call_id: CallId,
    kind: PendingKind,
    lane: PendingLane,
    /// User explicitly cancelled this op via `cancel(call_id)`. Once
    /// set, the op no longer counts as a pending driver call: the
    /// runtime stopped waiting for its result.
    user_cancelled: bool,
    /// Shutdown drain blanket-marked this op for backend pointer
    /// release. Stays counted as a pending driver call so the terminal
    /// report can name work the lane could not finish in budget.
    shutdown_marked: bool,
}

/// One async operation in flight against Betelgeuse.
///
/// The completion slot is heap-allocated so Betelgeuse's stored pointer
/// to the inner `CompletionInner` stays valid while the `PendingOperation`
/// itself is moved through the `pending` vector. We track the originating
/// listener/stream lane so Tina can allow full-duplex stream use while still
/// rejecting duplicate work on one lane. The runtime's `call_id` remains the
/// stable handle used for cancellation and completion delivery.
enum PendingKind {
    Accept(Box<AcceptCompletion>),
    Connect {
        completion: Box<ConnectCompletion>,
        socket: Option<Box<dyn IOSocket>>,
    },
    Read(Box<RecvCompletion>),
    Write(Box<SendCompletion>),
    UdpRecv {
        socket: UdpSocketId,
        max_len: usize,
        buffer: Vec<u8>,
    },
    FileRead(Box<PReadCompletion>),
    FileWrite(Box<PWriteCompletion>),
    FileFsync(Box<FsyncCompletion>),
    FileSize(Box<SizeCompletion>),
    Mkdir(Box<MkdirCompletion>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingLane {
    ListenerAccept(ListenerId),
    TcpConnect(CallId),
    StreamRead(StreamId),
    StreamWrite(StreamId),
    UdpRecv(UdpSocketId),
    FileRead(FileId),
    FileWrite(FileId),
    FileFsync(FileId),
    FileSize(FileId),
    Mkdir(CallId),
}

/// One completion the driver produced during [`RuntimeDriver::advance`].
#[derive(Debug)]
pub(crate) struct DriverCompletion {
    pub(crate) call_id: CallId,
    pub(crate) result: CallOutput,
}

enum StorageLane {
    Inline,
    Worker(StorageWorkerLane),
}

enum DnsLane {
    Worker(DnsWorkerLane),
}

enum TlsLane {
    Worker(TlsWorkerLane),
}

type DnsResolver = Arc<dyn Fn(&str, u16) -> CallOutput + Send + Sync + 'static>;

struct StorageWorkerLane {
    capacity: usize,
    sender: Option<SyncSender<StorageCommand>>,
    completions: Receiver<StorageCompletion>,
    handle: Option<JoinHandle<()>>,
    pending: Vec<StoragePending>,
}

struct StoragePending {
    call_id: CallId,
    cancelled: Arc<AtomicBool>,
}

struct StorageCommand {
    call_id: CallId,
    job: StorageJob,
    cancelled: Arc<AtomicBool>,
}

enum StorageJob {
    SnapshotCommit {
        path: PathBuf,
        bytes: Vec<u8>,
        last_journal_index: u64,
    },
    SnapshotLoad {
        path: PathBuf,
    },
    JournalAppend {
        path: PathBuf,
        record_index: u64,
        bytes: Vec<u8>,
    },
    JournalReplay {
        path: PathBuf,
    },
    PathMetadata {
        path: PathBuf,
    },
    RenameReplace {
        from: PathBuf,
        to: PathBuf,
    },
    RemoveFile {
        path: PathBuf,
    },
    ReadDir {
        path: PathBuf,
    },
    SyncParent {
        path: PathBuf,
    },
    #[cfg(test)]
    Park {
        started: SyncSender<()>,
        release: Receiver<()>,
    },
}

struct StorageCompletion {
    call_id: CallId,
    result: CallOutput,
}

struct DnsWorkerLane {
    capacity: usize,
    sender: Option<SyncSender<DnsCommand>>,
    completions: Receiver<DnsCompletion>,
    handle: Option<JoinHandle<()>>,
    pending: Vec<DnsPending>,
}

struct DnsPending {
    call_id: CallId,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    timed_out: bool,
}

struct DnsCommand {
    call_id: CallId,
    host: String,
    port: u16,
    cancelled: Arc<AtomicBool>,
}

struct DnsCompletion {
    call_id: CallId,
    result: CallOutput,
}

struct TlsWorkerLane {
    capacity: usize,
    sender: Option<SyncSender<TlsCommand>>,
    completions: Receiver<TlsCompletion>,
    handle: Option<JoinHandle<()>>,
    pending: Vec<TlsPending>,
    listeners: Vec<TlsListenerEntry>,
    streams: Vec<TlsStreamEntry>,
    next_listener_id: u64,
    next_stream_id: u64,
}

struct TlsListenerEntry {
    id: TlsListenerId,
    listener: Arc<TcpListener>,
    config: Arc<rustls::ServerConfig>,
}

struct TlsStreamEntry {
    id: TlsStreamId,
    stream: Arc<Mutex<TlsRuntimeStream>>,
}

struct TlsPending {
    call_id: CallId,
    lane: TlsPendingLane,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    timed_out: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsPendingLane {
    Connect(CallId),
    ListenerAccept(TlsListenerId),
    Stream(TlsStreamId),
}

enum TlsCommand {
    Bind {
        call_id: CallId,
        addr: SocketAddr,
        certificate_chain: Vec<Vec<u8>>,
        private_key: Vec<u8>,
        cancelled: Arc<AtomicBool>,
    },
    Accept {
        call_id: CallId,
        listener: Arc<TcpListener>,
        config: Arc<rustls::ServerConfig>,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
    },
    Connect {
        call_id: CallId,
        addr: SocketAddr,
        server_name: String,
        root_certificates: Vec<Vec<u8>>,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
    },
    Read {
        call_id: CallId,
        stream: Arc<Mutex<TlsRuntimeStream>>,
        max_len: usize,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
    },
    Write {
        call_id: CallId,
        stream: Arc<Mutex<TlsRuntimeStream>>,
        bytes: Vec<u8>,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
    },
    Close {
        call_id: CallId,
        stream: Arc<Mutex<TlsRuntimeStream>>,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
    },
}

struct TlsCompletion {
    call_id: CallId,
    result: TlsCompletionResult,
}

enum TlsCompletionResult {
    Bound(Box<Result<(TcpListener, rustls::ServerConfig), CallError>>),
    Accepted(Box<Result<(TlsRuntimeStream, SocketAddr), CallError>>),
    Connected(Box<Result<TlsRuntimeStream, CallError>>),
    Output(CallOutput),
}

impl BetelgeuseDriver {
    pub(crate) fn new() -> Self {
        let io_loop =
            io_loop(Global).expect("failed to initialise Betelgeuse IO loop for tina-runtime");
        Self::with_io_loop(io_loop)
    }

    pub(crate) fn with_io_loop(io_loop: IOLoopHandle<Global>) -> Self {
        Self {
            tcp: BetelgeuseTcp::with_io_loop(io_loop),
            storage: StorageLane::inline(),
            dns: DnsLane::new(DEFAULT_DNS_LANE_CAPACITY),
            tls: TlsLane::new(DEFAULT_TLS_LANE_CAPACITY),
            process: ProcessLane::new(DEFAULT_PROCESS_LANE_CAPACITY),
            signals: Vec::with_capacity(
                DEFAULT_SIGNAL_CAPACITY.min(INITIAL_DRIVER_PENDING_CAPACITY),
            ),
            signal_capacity: DEFAULT_SIGNAL_CAPACITY,
            timers: Vec::with_capacity(INITIAL_DRIVER_TIMER_CAPACITY),
            next_timer_ordinal: 0,
            os_signals: OsSignalDispatcher::install(),
        }
    }

    pub(crate) fn with_io_loop_and_capacities(
        io_loop: IOLoopHandle<Global>,
        storage_lane_capacity: usize,
        dns_lane_capacity: usize,
        tls_lane_capacity: usize,
        process_lane_capacity: usize,
        signal_capacity: usize,
    ) -> Self {
        Self {
            tcp: BetelgeuseTcp::with_io_loop(io_loop),
            storage: StorageLane::new(storage_lane_capacity),
            dns: DnsLane::new(dns_lane_capacity),
            tls: TlsLane::new(tls_lane_capacity),
            process: ProcessLane::new(process_lane_capacity),
            signals: Vec::with_capacity(signal_capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
            signal_capacity,
            timers: Vec::with_capacity(INITIAL_DRIVER_TIMER_CAPACITY),
            next_timer_ordinal: 0,
            os_signals: OsSignalDispatcher::install(),
        }
    }
}

impl RuntimeDriver for BetelgeuseDriver {
    fn submit(
        &mut self,
        call_id: CallId,
        request: CallInput,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match request {
            CallInput::Sleep { after } => {
                let insertion_order = self.next_timer_ordinal;
                self.next_timer_ordinal += 1;
                self.timers.push(TimerEntry {
                    call_id,
                    deadline: now + after,
                    insertion_order,
                });
                None
            }
            CallInput::SnapshotCommit {
                path,
                bytes,
                last_journal_index,
            } => self.storage.submit(
                call_id,
                StorageJob::SnapshotCommit {
                    path,
                    bytes,
                    last_journal_index,
                },
            ),
            CallInput::SnapshotLoad { path } => self
                .storage
                .submit(call_id, StorageJob::SnapshotLoad { path }),
            CallInput::JournalAppend {
                path,
                record_index,
                bytes,
            } => self.storage.submit(
                call_id,
                StorageJob::JournalAppend {
                    path,
                    record_index,
                    bytes,
                },
            ),
            CallInput::JournalReplay { path } => self
                .storage
                .submit(call_id, StorageJob::JournalReplay { path }),
            CallInput::PathMetadata { path } => self
                .storage
                .submit(call_id, StorageJob::PathMetadata { path }),
            CallInput::RenameReplace { from, to } => self
                .storage
                .submit(call_id, StorageJob::RenameReplace { from, to }),
            CallInput::RemoveFile { path } => self
                .storage
                .submit(call_id, StorageJob::RemoveFile { path }),
            CallInput::ReadDir { path } => {
                self.storage.submit(call_id, StorageJob::ReadDir { path })
            }
            CallInput::SyncParent { path } => self
                .storage
                .submit(call_id, StorageJob::SyncParent { path }),
            CallInput::DnsLookup {
                host,
                port,
                timeout,
            } => self.dns.submit(call_id, host, port, timeout, now),
            CallInput::TlsConnect {
                addr,
                server_name,
                root_certificates,
                timeout,
            } => {
                self.tls
                    .submit_connect(call_id, addr, server_name, root_certificates, timeout, now)
            }
            CallInput::TlsBind {
                addr,
                certificate_chain,
                private_key,
            } => self
                .tls
                .submit_bind(call_id, addr, certificate_chain, private_key, now),
            CallInput::TlsAccept { listener, timeout } => {
                self.tls.submit_accept(call_id, listener, timeout, now)
            }
            CallInput::TlsListenerClose { listener } => {
                self.tls.submit_listener_close(call_id, listener)
            }
            CallInput::TlsRead {
                stream,
                max_len,
                timeout,
            } => self.tls.submit_read(call_id, stream, max_len, timeout, now),
            CallInput::TlsWrite {
                stream,
                bytes,
                timeout,
            } => self.tls.submit_write(call_id, stream, bytes, timeout, now),
            CallInput::TlsClose { stream, timeout } => {
                self.tls.submit_close(call_id, stream, timeout, now)
            }
            CallInput::SignalWait { name, timeout } => {
                self.submit_signal_wait(call_id, name, timeout, now)
            }
            CallInput::ProcessRun {
                command,
                args,
                timeout,
                stdout_limit,
                stderr_limit,
            } => self.process.submit(
                call_id,
                ProcessCommand {
                    call_id,
                    command,
                    args,
                    timeout,
                    stdout_limit,
                    stderr_limit,
                    cancelled: Arc::new(AtomicBool::new(false)),
                },
            ),
            other => self.tcp.submit(call_id, other),
        }
    }

    fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        self.tcp.advance(completed);
        self.storage.advance(completed);
        self.dns.advance(now, completed);
        self.tls.advance(now, completed);
        self.process.advance(completed);
        self.poll_os_signals(completed);
        self.harvest_signals(now, completed);
        self.harvest_timers(now, completed);
    }

    fn take_cancelled_by_close(&mut self) -> Vec<CallId> {
        self.tcp.take_cancelled_by_close()
    }

    fn has_pending(&self) -> bool {
        self.tcp.has_pending()
            || self.storage.has_pending()
            || self.dns.has_pending()
            || self.tls.has_pending()
            || self.process.has_pending()
            || self.signals.iter().any(|entry| !entry.cancelled)
            || !self.timers.is_empty()
    }

    fn cancel_pending(&mut self, deadline: Instant) -> Result<(), DriverShutdownError> {
        self.timers.clear();
        self.signals.clear();
        self.storage.cancel_pending(deadline);
        self.dns.cancel_pending(deadline);
        self.tls.cancel_pending(deadline);
        self.process.cancel_pending(deadline);
        self.tcp.cancel_pending(deadline)
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let before = self.timers.len();
        self.timers.retain(|entry| entry.call_id != call_id);
        let signal_before = self.signals.len();
        self.signals.retain(|entry| entry.call_id != call_id);
        before != self.timers.len()
            || signal_before != self.signals.len()
            || self.storage.cancel(call_id)
            || self.dns.cancel(call_id)
            || self.tls.cancel(call_id)
            || self.process.cancel(call_id)
            || self.tcp.cancel(call_id)
    }

    #[cfg(test)]
    fn io_pending_count(&self) -> usize {
        self.tcp.pending_count()
    }

    fn notify_signal(&mut self, name: &str, completed: &mut Vec<DriverCompletion>) {
        let mut ready = Vec::new();
        let mut pending = Vec::new();
        for entry in self.signals.drain(..) {
            if !entry.cancelled && entry.name == name {
                ready.push(entry);
            } else {
                pending.push(entry);
            }
        }
        self.signals = pending;
        for entry in ready {
            completed.push(DriverCompletion {
                call_id: entry.call_id,
                result: CallOutput::SignalReceived { name: entry.name },
            });
        }
    }

    fn resource_report(&self) -> DriverResourceReport {
        let tcp = self.tcp.resource_report();
        let tls = self.tls.resource_report();
        let process_pending = self.process.physical_pending_count();
        DriverResourceReport {
            listeners: tcp.listeners + tls.listeners,
            streams: tcp.streams,
            tls_streams: tls.tls_streams,
            udp_sockets: tcp.udp_sockets,
            files: tcp.files,
            // Worker-held: TLS in-flight ops parking cloned listener/stream
            // arcs, plus process calls owning a live Child. DNS/storage
            // workers do not hold a runtime-visible OS handle, so they
            // contribute zero here per the plan's count rules.
            worker_held: tls.worker_held + process_pending,
            // Pending counts use physical entries (not filtered on the
            // user-cancel flag) so that work the runtime asked to cancel
            // but the lane has not yet drained — including work stuck on
            // a worker after a bounded shutdown drain — stays visible.
            pending_calls: tcp.pending_calls
                + self.storage.physical_pending_count()
                + self.dns.physical_pending_count()
                + tls.pending_calls
                + process_pending
                + self.signals.iter().filter(|entry| !entry.cancelled).count()
                + self.timers.len(),
        }
    }
}

impl BetelgeuseDriver {
    fn submit_signal_wait(
        &mut self,
        call_id: CallId,
        name: String,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        if timeout.is_zero() {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Timeout),
            });
        }
        if self.signals.iter().filter(|entry| !entry.cancelled).count() >= self.signal_capacity {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::SignalFull),
            });
        }
        self.signals.push(SignalWaitEntry {
            call_id,
            name,
            deadline: now + timeout,
            cancelled: false,
        });
        None
    }

    fn poll_os_signals(&mut self, completed: &mut Vec<DriverCompletion>) {
        // Convert any captured OS signal flag bits into runtime-owned
        // signal completions for parked `signal_wait` calls. Each flag
        // is consumed exactly once per delivery; subsequent SIGINT or
        // SIGTERM events set the flag again and fire the next pending
        // wait.
        if self.os_signals.consume_sigint() {
            self.notify_signal("sigint", completed);
        }
        if self.os_signals.consume_sigterm() {
            self.notify_signal("sigterm", completed);
        }
    }

    fn harvest_signals(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        let mut ready = Vec::new();
        let mut pending = Vec::new();
        for entry in self.signals.drain(..) {
            if entry.cancelled {
                continue;
            }
            if entry.deadline <= now {
                ready.push(entry);
            } else {
                pending.push(entry);
            }
        }
        self.signals = pending;
        for entry in ready {
            completed.push(DriverCompletion {
                call_id: entry.call_id,
                result: CallOutput::Failed(CallError::Timeout),
            });
        }
    }

    fn harvest_timers(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        while let Some(index) = self
            .timers
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.deadline <= now)
            .min_by(|(_, left), (_, right)| {
                left.deadline
                    .cmp(&right.deadline)
                    .then_with(|| left.insertion_order.cmp(&right.insertion_order))
            })
            .map(|(index, _)| index)
        {
            let entry = self.timers.remove(index);
            completed.push(DriverCompletion {
                call_id: entry.call_id,
                result: CallOutput::TimerFired,
            });
        }
    }
}

impl StorageLane {
    fn inline() -> Self {
        Self::Inline
    }

    fn new(capacity: usize) -> Self {
        Self::Worker(StorageWorkerLane::new(capacity))
    }

    fn submit(&mut self, call_id: CallId, job: StorageJob) -> Option<DriverCompletion> {
        match self {
            Self::Inline => Some(DriverCompletion {
                call_id,
                result: execute_storage_job(job),
            }),
            Self::Worker(lane) => lane.submit(call_id, job),
        }
    }

    fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
        if let Self::Worker(lane) = self {
            lane.advance(completed);
        }
    }

    fn has_pending(&self) -> bool {
        match self {
            Self::Inline => false,
            Self::Worker(lane) => lane.has_pending(),
        }
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        match self {
            Self::Inline => false,
            Self::Worker(lane) => lane.cancel(call_id),
        }
    }

    fn cancel_pending(&mut self, deadline: Instant) {
        if let Self::Worker(lane) = self {
            lane.cancel_pending(deadline);
        }
    }

    fn physical_pending_count(&self) -> usize {
        match self {
            Self::Inline => 0,
            Self::Worker(lane) => lane.physical_pending_count(),
        }
    }
}

impl Drop for StorageLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}

impl DnsLane {
    fn new(capacity: usize) -> Self {
        Self::Worker(DnsWorkerLane::new(capacity, Arc::new(default_dns_resolver)))
    }

    fn submit(
        &mut self,
        call_id: CallId,
        host: String,
        port: u16,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => lane.submit(call_id, host, port, timeout, now),
        }
    }

    fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        match self {
            Self::Worker(lane) => lane.advance(now, completed),
        }
    }

    fn has_pending(&self) -> bool {
        match self {
            Self::Worker(lane) => lane.has_pending(),
        }
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        match self {
            Self::Worker(lane) => lane.cancel(call_id),
        }
    }

    fn cancel_pending(&mut self, deadline: Instant) {
        match self {
            Self::Worker(lane) => lane.cancel_pending(deadline),
        }
    }

    fn physical_pending_count(&self) -> usize {
        match self {
            Self::Worker(lane) => lane.physical_pending_count(),
        }
    }
}

impl Drop for DnsLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}

impl TlsLane {
    fn new(capacity: usize) -> Self {
        Self::Worker(TlsWorkerLane::new(capacity))
    }

    fn submit_connect(
        &mut self,
        call_id: CallId,
        addr: SocketAddr,
        server_name: String,
        root_certificates: Vec<Vec<u8>>,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => {
                lane.submit_connect(call_id, addr, server_name, root_certificates, timeout, now)
            }
        }
    }

    fn submit_bind(
        &mut self,
        call_id: CallId,
        addr: SocketAddr,
        certificate_chain: Vec<Vec<u8>>,
        private_key: Vec<u8>,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => {
                lane.submit_bind(call_id, addr, certificate_chain, private_key, now)
            }
        }
    }

    fn submit_accept(
        &mut self,
        call_id: CallId,
        listener: TlsListenerId,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => lane.submit_accept(call_id, listener, timeout, now),
        }
    }

    fn submit_listener_close(
        &mut self,
        call_id: CallId,
        listener: TlsListenerId,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => lane.submit_listener_close(call_id, listener),
        }
    }

    fn submit_read(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        max_len: usize,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => lane.submit_read(call_id, stream, max_len, timeout, now),
        }
    }

    fn submit_write(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        bytes: Vec<u8>,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => lane.submit_write(call_id, stream, bytes, timeout, now),
        }
    }

    fn submit_close(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => lane.submit_close(call_id, stream, timeout, now),
        }
    }

    fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        match self {
            Self::Worker(lane) => lane.advance(now, completed),
        }
    }

    fn has_pending(&self) -> bool {
        match self {
            Self::Worker(lane) => lane.has_pending(),
        }
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        match self {
            Self::Worker(lane) => lane.cancel(call_id),
        }
    }

    fn cancel_pending(&mut self, deadline: Instant) {
        match self {
            Self::Worker(lane) => lane.cancel_pending(deadline),
        }
    }

    fn resource_report(&self) -> DriverResourceReport {
        match self {
            Self::Worker(lane) => lane.resource_report(),
        }
    }
}

impl Drop for TlsLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}

impl StorageWorkerLane {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "storage lane capacity must be > 0");
        let (sender, receiver) = sync_channel(capacity);
        let (completion_sender, completions) = sync_channel(capacity.saturating_add(1));
        let handle = thread::spawn(move || storage_worker_loop(receiver, completion_sender));
        Self {
            capacity,
            sender: Some(sender),
            completions,
            handle: Some(handle),
            pending: Vec::with_capacity(capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
        }
    }

    fn submit(&mut self, call_id: CallId, job: StorageJob) -> Option<DriverCompletion> {
        let Some(sender) = &self.sender else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::StorageClosed),
            });
        };
        if self.active_pending_count() >= self.capacity {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::StorageFull),
            });
        }
        let cancelled = Arc::new(AtomicBool::new(false));

        match sender.try_send(StorageCommand {
            call_id,
            job,
            cancelled: Arc::clone(&cancelled),
        }) {
            Ok(()) => {
                self.pending.push(StoragePending { call_id, cancelled });
                None
            }
            Err(MpscTrySendError::Full(command)) => Some(DriverCompletion {
                call_id: command.call_id,
                result: CallOutput::Failed(CallError::StorageFull),
            }),
            Err(MpscTrySendError::Disconnected(command)) => Some(DriverCompletion {
                call_id: command.call_id,
                result: CallOutput::Failed(CallError::StorageClosed),
            }),
        }
    }

    fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
        loop {
            match self.completions.try_recv() {
                Ok(completion) => self.finish_completion(completion, completed),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.sender = None;
                    break;
                }
            }
        }
    }

    fn finish_completion(
        &mut self,
        completion: StorageCompletion,
        completed: &mut Vec<DriverCompletion>,
    ) {
        let Some(index) = self
            .pending
            .iter()
            .position(|entry| entry.call_id == completion.call_id)
        else {
            return;
        };
        let pending = self.pending.remove(index);
        if pending.cancelled.load(Ordering::Acquire) {
            return;
        }
        completed.push(DriverCompletion {
            call_id: completion.call_id,
            result: completion.result,
        });
    }

    fn has_pending(&self) -> bool {
        self.active_pending_count() > 0
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let Some(pending) = self
            .pending
            .iter_mut()
            .find(|entry| entry.call_id == call_id && !entry.cancelled.load(Ordering::Acquire))
        else {
            return false;
        };
        pending.cancelled.store(true, Ordering::Release);
        true
    }

    fn cancel_pending(&mut self, deadline: Instant) {
        // Drop the command sender so the worker thread will exit on its
        // next recv. Then drain completion results until the lane has no
        // pending work or the budget elapses; whatever remains in
        // `self.pending` is stuck work that the worker has not finished
        // and will surface in `physical_pending_count`.
        self.sender = None;
        let mut sink = Vec::new();
        loop {
            self.drain_into_sink(&mut sink);
            if self.pending.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::yield_now();
        }
        sink.clear();
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn drain_into_sink(&mut self, sink: &mut Vec<DriverCompletion>) {
        while let Ok(completion) = self.completions.try_recv() {
            self.finish_completion(completion, sink);
        }
    }

    fn active_pending_count(&self) -> usize {
        self.pending
            .iter()
            .filter(|entry| !entry.cancelled.load(Ordering::Acquire))
            .count()
    }

    fn physical_pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Drop for StorageWorkerLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}

impl DnsWorkerLane {
    fn new(capacity: usize, resolver: DnsResolver) -> Self {
        assert!(capacity > 0, "DNS lane capacity must be > 0");
        let (sender, receiver) = sync_channel(capacity);
        let (completion_sender, completions) = sync_channel(capacity.saturating_add(1));
        let handle = thread::spawn(move || dns_worker_loop(receiver, completion_sender, resolver));
        Self {
            capacity,
            sender: Some(sender),
            completions,
            handle: Some(handle),
            pending: Vec::with_capacity(capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
        }
    }

    fn submit(
        &mut self,
        call_id: CallId,
        host: String,
        port: u16,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        if timeout.is_zero() {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Timeout),
            });
        }
        let Some(sender) = &self.sender else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::DnsClosed),
            });
        };
        if self.unresolved_pending_count() >= self.capacity {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::DnsFull),
            });
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        match sender.try_send(DnsCommand {
            call_id,
            host,
            port,
            cancelled: Arc::clone(&cancelled),
        }) {
            Ok(()) => {
                self.pending.push(DnsPending {
                    call_id,
                    deadline: now + timeout,
                    cancelled,
                    timed_out: false,
                });
                None
            }
            Err(MpscTrySendError::Full(command)) => Some(DriverCompletion {
                call_id: command.call_id,
                result: CallOutput::Failed(CallError::DnsFull),
            }),
            Err(MpscTrySendError::Disconnected(command)) => Some(DriverCompletion {
                call_id: command.call_id,
                result: CallOutput::Failed(CallError::DnsClosed),
            }),
        }
    }

    fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        for pending in &mut self.pending {
            if !pending.timed_out
                && !pending.cancelled.load(Ordering::Acquire)
                && now >= pending.deadline
            {
                pending.timed_out = true;
                pending.cancelled.store(true, Ordering::Release);
                completed.push(DriverCompletion {
                    call_id: pending.call_id,
                    result: CallOutput::Failed(CallError::Timeout),
                });
            }
        }

        loop {
            match self.completions.try_recv() {
                Ok(completion) => self.finish_completion(completion, completed),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.sender = None;
                    break;
                }
            }
        }
    }

    fn finish_completion(
        &mut self,
        completion: DnsCompletion,
        completed: &mut Vec<DriverCompletion>,
    ) {
        let Some(index) = self
            .pending
            .iter()
            .position(|entry| entry.call_id == completion.call_id)
        else {
            return;
        };
        let pending = self.pending.remove(index);
        if pending.cancelled.load(Ordering::Acquire) || pending.timed_out {
            return;
        }
        completed.push(DriverCompletion {
            call_id: completion.call_id,
            result: completion.result,
        });
    }

    fn has_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|entry| !entry.cancelled.load(Ordering::Acquire) && !entry.timed_out)
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let Some(pending) = self
            .pending
            .iter_mut()
            .find(|entry| entry.call_id == call_id && !entry.cancelled.load(Ordering::Acquire))
        else {
            return false;
        };
        pending.cancelled.store(true, Ordering::Release);
        true
    }

    fn cancel_pending(&mut self, deadline: Instant) {
        // Drop the command sender; the worker thread can exit when it
        // returns from the resolver. Drain completions for the budget;
        // remaining `self.pending` after the budget is stuck work the
        // worker has not finished and stays visible in
        // `physical_pending_count`.
        self.sender = None;
        let mut sink = Vec::new();
        loop {
            self.drain_into_sink(&mut sink);
            if self.pending.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::yield_now();
        }
        sink.clear();
        if self
            .handle
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn drain_into_sink(&mut self, sink: &mut Vec<DriverCompletion>) {
        while let Ok(completion) = self.completions.try_recv() {
            self.finish_completion(completion, sink);
        }
    }

    fn unresolved_pending_count(&self) -> usize {
        self.pending.len()
    }

    fn physical_pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Drop for DnsWorkerLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}

impl TlsWorkerLane {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "TLS lane capacity must be > 0");
        let (sender, receiver) = sync_channel(capacity);
        let (completion_sender, completions) = sync_channel(capacity.saturating_add(1));
        let handle = thread::spawn(move || tls_worker_loop(receiver, completion_sender));
        Self {
            capacity,
            sender: Some(sender),
            completions,
            handle: Some(handle),
            pending: Vec::with_capacity(capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
            listeners: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            streams: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            next_listener_id: 1,
            next_stream_id: 1,
        }
    }

    fn submit_bind(
        &mut self,
        call_id: CallId,
        addr: SocketAddr,
        certificate_chain: Vec<Vec<u8>>,
        private_key: Vec<u8>,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.submit_command(
            call_id,
            TlsPendingLane::Connect(call_id),
            Arc::clone(&cancelled),
            now,
            TlsCommand::Bind {
                call_id,
                addr,
                certificate_chain,
                private_key,
                cancelled,
            },
        )
    }

    fn submit_accept(
        &mut self,
        call_id: CallId,
        listener: TlsListenerId,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let lane = TlsPendingLane::ListenerAccept(listener);
        if self.lane_has_pending(lane) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::ResourceBusy),
            });
        }
        let Some(entry) = self.listener(listener) else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::InvalidResource),
            });
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        self.submit_command(
            call_id,
            lane,
            Arc::clone(&cancelled),
            now,
            TlsCommand::Accept {
                call_id,
                listener: entry.0,
                config: entry.1,
                timeout,
                cancelled,
            },
        )
    }

    fn submit_listener_close(
        &mut self,
        call_id: CallId,
        listener: TlsListenerId,
    ) -> Option<DriverCompletion> {
        if self.lane_has_pending(TlsPendingLane::ListenerAccept(listener)) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::ResourceBusy),
            });
        }
        let Some(index) = self.listeners.iter().position(|entry| entry.id == listener) else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::InvalidResource),
            });
        };
        self.listeners.remove(index);
        Some(DriverCompletion {
            call_id,
            result: CallOutput::TlsListenerClosed,
        })
    }

    fn submit_connect(
        &mut self,
        call_id: CallId,
        addr: SocketAddr,
        server_name: String,
        root_certificates: Vec<Vec<u8>>,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.submit_command(
            call_id,
            TlsPendingLane::Connect(call_id),
            cancelled,
            now,
            TlsCommand::Connect {
                call_id,
                addr,
                server_name,
                root_certificates,
                timeout,
                cancelled: Arc::new(AtomicBool::new(false)),
            },
        )
    }

    fn submit_close(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let lane = TlsPendingLane::Stream(stream);
        if self.lane_has_pending(lane) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::ResourceBusy),
            });
        }
        let Some(stream) = self.stream(stream) else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::InvalidResource),
            });
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        self.submit_command(
            call_id,
            lane,
            Arc::clone(&cancelled),
            now,
            TlsCommand::Close {
                call_id,
                stream,
                timeout,
                cancelled,
            },
        )
    }

    fn submit_read(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        max_len: usize,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let lane = TlsPendingLane::Stream(stream);
        if self.lane_has_pending(lane) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::ResourceBusy),
            });
        }
        let Some(stream) = self.stream(stream) else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::InvalidResource),
            });
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        self.submit_command(
            call_id,
            lane,
            Arc::clone(&cancelled),
            now,
            TlsCommand::Read {
                call_id,
                stream,
                max_len,
                timeout,
                cancelled,
            },
        )
    }

    fn submit_write(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        bytes: Vec<u8>,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let lane = TlsPendingLane::Stream(stream);
        if self.lane_has_pending(lane) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::ResourceBusy),
            });
        }
        let Some(stream) = self.stream(stream) else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::InvalidResource),
            });
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        self.submit_command(
            call_id,
            lane,
            Arc::clone(&cancelled),
            now,
            TlsCommand::Write {
                call_id,
                stream,
                bytes,
                timeout,
                cancelled,
            },
        )
    }

    fn submit_command(
        &mut self,
        call_id: CallId,
        lane: TlsPendingLane,
        cancelled: Arc<AtomicBool>,
        now: Instant,
        mut command: TlsCommand,
    ) -> Option<DriverCompletion> {
        let timeout = command.timeout();
        if command.timeout().is_zero() {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Timeout),
            });
        }
        let Some(sender) = &self.sender else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::TlsClosed),
            });
        };
        if self.pending.len() >= self.capacity {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::TlsFull),
            });
        }
        command.set_cancelled(Arc::clone(&cancelled));
        match sender.try_send(command) {
            Ok(()) => {
                self.pending.push(TlsPending {
                    call_id,
                    lane,
                    deadline: now + timeout,
                    cancelled,
                    timed_out: false,
                });
                None
            }
            Err(MpscTrySendError::Full(command)) => Some(DriverCompletion {
                call_id: command.call_id(),
                result: CallOutput::Failed(CallError::TlsFull),
            }),
            Err(MpscTrySendError::Disconnected(command)) => Some(DriverCompletion {
                call_id: command.call_id(),
                result: CallOutput::Failed(CallError::TlsClosed),
            }),
        }
    }

    fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        for pending in &mut self.pending {
            if !pending.timed_out
                && !pending.cancelled.load(Ordering::Acquire)
                && now >= pending.deadline
            {
                pending.timed_out = true;
                pending.cancelled.store(true, Ordering::Release);
                completed.push(DriverCompletion {
                    call_id: pending.call_id,
                    result: CallOutput::Failed(CallError::Timeout),
                });
            }
        }

        loop {
            match self.completions.try_recv() {
                Ok(completion) => self.finish_completion(completion, completed),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.sender = None;
                    break;
                }
            }
        }
    }

    fn finish_completion(
        &mut self,
        completion: TlsCompletion,
        completed: &mut Vec<DriverCompletion>,
    ) {
        let Some(index) = self
            .pending
            .iter()
            .position(|entry| entry.call_id == completion.call_id)
        else {
            return;
        };
        let pending = self.pending.remove(index);
        if pending.cancelled.load(Ordering::Acquire) || pending.timed_out {
            return;
        }
        let result = match completion.result {
            TlsCompletionResult::Bound(result) => match *result {
                Ok((listener, config)) => {
                    let id = TlsListenerId::new(self.next_listener_id);
                    self.next_listener_id += 1;
                    let local_addr = listener.local_addr().map_err(|_| CallError::Io);
                    let listener = Arc::new(listener);
                    self.listeners.push(TlsListenerEntry {
                        id,
                        listener,
                        config: Arc::new(config),
                    });
                    match local_addr {
                        Ok(local_addr) => CallOutput::TlsBound {
                            listener: id,
                            local_addr,
                        },
                        Err(error) => CallOutput::Failed(error),
                    }
                }
                Err(error) => CallOutput::Failed(error),
            },
            TlsCompletionResult::Accepted(result) => match *result {
                Ok((stream, peer_addr)) => {
                    let id = TlsStreamId::new(self.next_stream_id);
                    self.next_stream_id += 1;
                    self.streams.push(TlsStreamEntry {
                        id,
                        stream: Arc::new(Mutex::new(stream)),
                    });
                    CallOutput::TlsAccepted {
                        stream: id,
                        peer_addr,
                    }
                }
                Err(error) => CallOutput::Failed(error),
            },
            TlsCompletionResult::Connected(result) => match *result {
                Ok(stream) => {
                    let id = TlsStreamId::new(self.next_stream_id);
                    self.next_stream_id += 1;
                    self.streams.push(TlsStreamEntry {
                        id,
                        stream: Arc::new(Mutex::new(stream)),
                    });
                    CallOutput::TlsConnected { stream: id }
                }
                Err(error) => CallOutput::Failed(error),
            },
            TlsCompletionResult::Output(output) => {
                if matches!(output, CallOutput::TlsClosed)
                    && let TlsPendingLane::Stream(stream) = pending.lane
                {
                    self.streams.retain(|entry| entry.id != stream);
                }
                output
            }
        };
        completed.push(DriverCompletion {
            call_id: completion.call_id,
            result,
        });
    }

    fn has_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|entry| !entry.cancelled.load(Ordering::Acquire) && !entry.timed_out)
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let Some(pending) = self
            .pending
            .iter_mut()
            .find(|entry| entry.call_id == call_id && !entry.cancelled.load(Ordering::Acquire))
        else {
            return false;
        };
        pending.cancelled.store(true, Ordering::Release);
        true
    }

    fn cancel_pending(&mut self, deadline: Instant) {
        // Drop the command sender so the worker thread can exit. Drain
        // completions for the budget; remaining `self.pending` after the
        // budget is stuck work that the worker has not finished and is
        // surfaced through `resource_report`.
        self.sender = None;
        let mut sink = Vec::new();
        loop {
            self.drain_into_sink(&mut sink);
            if self.pending.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::yield_now();
        }
        sink.clear();
        if self
            .handle
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            if self.pending.is_empty() {
                self.listeners.clear();
                self.streams.clear();
            }
        }
    }

    fn drain_into_sink(&mut self, sink: &mut Vec<DriverCompletion>) {
        while let Ok(completion) = self.completions.try_recv() {
            self.finish_completion(completion, sink);
        }
    }

    fn stream(&self, stream: TlsStreamId) -> Option<Arc<Mutex<TlsRuntimeStream>>> {
        self.streams
            .iter()
            .find(|entry| entry.id == stream)
            .map(|entry| Arc::clone(&entry.stream))
    }

    fn lane_has_pending(&self, lane: TlsPendingLane) -> bool {
        self.pending
            .iter()
            .any(|entry| entry.lane == lane && !entry.cancelled.load(Ordering::Acquire))
    }

    fn listener(
        &self,
        listener: TlsListenerId,
    ) -> Option<(Arc<TcpListener>, Arc<rustls::ServerConfig>)> {
        self.listeners
            .iter()
            .find(|entry| entry.id == listener)
            .map(|entry| (Arc::clone(&entry.listener), Arc::clone(&entry.config)))
    }

    fn resource_report(&self) -> DriverResourceReport {
        // Each in-flight TLS op parks cloned `Arc<TcpListener>` /
        // `Arc<Mutex<TlsRuntimeStream>>` inside the worker thread for
        // the duration of accept/handshake/read/write/close. Count
        // physical entries so stuck-after-shutdown work stays visible.
        let physical = self.pending.len();
        DriverResourceReport {
            listeners: self.listeners.len(),
            tls_streams: self.streams.len(),
            worker_held: physical,
            pending_calls: physical,
            ..DriverResourceReport::default()
        }
    }
}

impl Drop for TlsWorkerLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}

fn storage_worker_loop(
    receiver: Receiver<StorageCommand>,
    completions: SyncSender<StorageCompletion>,
) {
    while let Ok(command) = receiver.recv() {
        if command.cancelled.load(Ordering::Acquire) {
            continue;
        }
        let completion = StorageCompletion {
            call_id: command.call_id,
            result: execute_storage_job(command.job),
        };
        if completions.send(completion).is_err() {
            break;
        }
    }
}

fn dns_worker_loop(
    receiver: Receiver<DnsCommand>,
    completions: SyncSender<DnsCompletion>,
    resolver: DnsResolver,
) {
    while let Ok(command) = receiver.recv() {
        let result = if command.cancelled.load(Ordering::Acquire) {
            CallOutput::Failed(CallError::Timeout)
        } else {
            resolver(&command.host, command.port)
        };
        if completions
            .send(DnsCompletion {
                call_id: command.call_id,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}

fn tls_worker_loop(receiver: Receiver<TlsCommand>, completions: SyncSender<TlsCompletion>) {
    while let Ok(command) = receiver.recv() {
        let call_id = command.call_id();
        let result = execute_tls_command(command);
        if completions.send(TlsCompletion { call_id, result }).is_err() {
            break;
        }
    }
}

fn execute_tls_command(command: TlsCommand) -> TlsCompletionResult {
    match command {
        TlsCommand::Bind {
            addr,
            certificate_chain,
            private_key,
            cancelled,
            ..
        } => TlsCompletionResult::Bound(Box::new(bind_tls_listener(
            addr,
            certificate_chain,
            private_key,
            &cancelled,
        ))),
        TlsCommand::Accept {
            listener,
            config,
            timeout,
            cancelled,
            ..
        } => TlsCompletionResult::Accepted(Box::new(accept_tls(
            listener, config, timeout, &cancelled,
        ))),
        TlsCommand::Connect {
            addr,
            server_name,
            root_certificates,
            timeout,
            cancelled,
            ..
        } => TlsCompletionResult::Connected(Box::new(connect_tls(
            addr,
            &server_name,
            root_certificates,
            timeout,
            &cancelled,
        ))),
        TlsCommand::Read {
            stream,
            max_len,
            timeout,
            cancelled,
            ..
        } => TlsCompletionResult::Output(read_tls(stream, max_len, timeout, &cancelled)),
        TlsCommand::Write {
            stream,
            bytes,
            timeout,
            cancelled,
            ..
        } => TlsCompletionResult::Output(write_tls(stream, &bytes, timeout, &cancelled)),
        TlsCommand::Close {
            stream,
            timeout,
            cancelled,
            ..
        } => TlsCompletionResult::Output(close_tls(stream, timeout, &cancelled)),
    }
}

fn connect_tls(
    addr: SocketAddr,
    server_name: &str,
    root_certificates: Vec<Vec<u8>>,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<TlsRuntimeStream, CallError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    if cancelled.load(Ordering::Acquire) {
        return Err(CallError::Timeout);
    }
    if timeout.is_zero() {
        return Err(CallError::Timeout);
    }
    let tcp = TcpStream::connect_timeout(&addr, timeout).map_err(|_| CallError::Io)?;
    let _ = tcp.set_read_timeout(Some(timeout));
    let _ = tcp.set_write_timeout(Some(timeout));

    let mut roots = rustls::RootCertStore::empty();
    for certificate in root_certificates {
        roots
            .add(rustls::pki_types::CertificateDer::from(certificate))
            .map_err(|_| CallError::TlsCertificate)?;
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(server_name.to_string())
        .map_err(|_| CallError::TlsName)?;
    let connection = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|_| CallError::TlsName)?;
    let mut stream = rustls::StreamOwned::new(connection, tcp);
    while stream.conn.is_handshaking() {
        if cancelled.load(Ordering::Acquire) {
            return Err(CallError::Timeout);
        }
        stream
            .conn
            .complete_io(&mut stream.sock)
            .map_err(|_| CallError::TlsHandshake)?;
    }
    Ok(TlsRuntimeStream::Client(stream))
}

fn bind_tls_listener(
    addr: SocketAddr,
    certificate_chain: Vec<Vec<u8>>,
    private_key: Vec<u8>,
    cancelled: &AtomicBool,
) -> Result<(TcpListener, rustls::ServerConfig), CallError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    if cancelled.load(Ordering::Acquire) {
        return Err(CallError::Timeout);
    }
    let certificates = certificate_chain
        .into_iter()
        .map(rustls::pki_types::CertificateDer::from)
        .collect();
    let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(private_key.into());
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(|_| CallError::TlsCertificate)?;
    let listener = TcpListener::bind(addr).map_err(|_| CallError::Io)?;
    Ok((listener, config))
}

fn accept_tls(
    listener: Arc<TcpListener>,
    config: Arc<rustls::ServerConfig>,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<(TlsRuntimeStream, SocketAddr), CallError> {
    if cancelled.load(Ordering::Acquire) || timeout.is_zero() {
        return Err(CallError::Timeout);
    }
    listener
        .set_nonblocking(true)
        .map_err(|_| CallError::TlsClosed)?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(CallError::Timeout)?;
    let (tcp, peer_addr) = loop {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(CallError::Timeout);
        }
        match listener.accept() {
            Ok(accepted) => break accepted,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::yield_now();
            }
            Err(_) => return Err(CallError::Io),
        }
    };
    tcp.set_nonblocking(false).map_err(|_| CallError::Io)?;
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(CallError::Timeout)?;
    let _ = tcp.set_read_timeout(Some(remaining));
    let _ = tcp.set_write_timeout(Some(remaining));
    let connection = rustls::ServerConnection::new(config).map_err(|_| CallError::TlsHandshake)?;
    let mut stream = rustls::StreamOwned::new(connection, tcp);
    while stream.conn.is_handshaking() {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(CallError::Timeout);
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(CallError::Timeout)?;
        let _ = stream.sock.set_read_timeout(Some(remaining));
        let _ = stream.sock.set_write_timeout(Some(remaining));
        stream
            .conn
            .complete_io(&mut stream.sock)
            .map_err(tls_handshake_error)?;
    }
    Ok((TlsRuntimeStream::Server(stream), peer_addr))
}

fn tls_handshake_error(error: std::io::Error) -> CallError {
    if matches!(
        error.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    ) {
        CallError::Timeout
    } else {
        CallError::TlsHandshake
    }
}

fn read_tls(
    stream: Arc<Mutex<TlsRuntimeStream>>,
    max_len: usize,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> CallOutput {
    if cancelled.load(Ordering::Acquire) {
        return CallOutput::Failed(CallError::Timeout);
    }
    let mut guard = match stream.lock() {
        Ok(guard) => guard,
        Err(_) => return CallOutput::Failed(CallError::TlsClosed),
    };
    let _ = guard.tcp().set_read_timeout(Some(timeout));
    let mut buffer = vec![0; max_len];
    match guard.read(&mut buffer) {
        Ok(count) => {
            buffer.truncate(count);
            CallOutput::TlsRead { bytes: buffer }
        }
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
            ) =>
        {
            CallOutput::Failed(CallError::Timeout)
        }
        Err(_) => CallOutput::Failed(CallError::Io),
    }
}

fn write_tls(
    stream: Arc<Mutex<TlsRuntimeStream>>,
    bytes: &[u8],
    timeout: Duration,
    cancelled: &AtomicBool,
) -> CallOutput {
    if cancelled.load(Ordering::Acquire) {
        return CallOutput::Failed(CallError::Timeout);
    }
    let mut guard = match stream.lock() {
        Ok(guard) => guard,
        Err(_) => return CallOutput::Failed(CallError::TlsClosed),
    };
    let _ = guard.tcp().set_write_timeout(Some(timeout));
    match guard.write(bytes) {
        Ok(count) => {
            if guard.flush().is_err() {
                return CallOutput::Failed(CallError::Io);
            }
            CallOutput::TlsWrote { count }
        }
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
            ) =>
        {
            CallOutput::Failed(CallError::Timeout)
        }
        Err(_) => CallOutput::Failed(CallError::Io),
    }
}

fn close_tls(
    stream: Arc<Mutex<TlsRuntimeStream>>,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> CallOutput {
    if cancelled.load(Ordering::Acquire) {
        return CallOutput::Failed(CallError::Timeout);
    }
    let mut guard = match stream.lock() {
        Ok(guard) => guard,
        Err(_) => return CallOutput::Failed(CallError::TlsClosed),
    };
    let _ = guard.tcp().set_write_timeout(Some(timeout));
    guard.send_close_notify();
    match guard.flush() {
        Ok(()) => CallOutput::TlsClosed,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
            ) =>
        {
            CallOutput::Failed(CallError::Timeout)
        }
        Err(_) => CallOutput::Failed(CallError::Io),
    }
}

fn default_dns_resolver(host: &str, port: u16) -> CallOutput {
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<SocketAddr> = addrs.collect();
            if addrs.is_empty() {
                CallOutput::Failed(CallError::Io)
            } else {
                CallOutput::DnsResolved { addrs }
            }
        }
        Err(_) => CallOutput::Failed(CallError::Io),
    }
}

fn execute_storage_job(job: StorageJob) -> CallOutput {
    match job {
        StorageJob::SnapshotCommit {
            path,
            bytes,
            last_journal_index,
        } => match crate::persistence::commit_snapshot(&path, bytes, last_journal_index) {
            Ok(()) => CallOutput::SnapshotCommitted,
            Err(reason) => CallOutput::Failed(reason),
        },
        StorageJob::SnapshotLoad { path } => match crate::persistence::load_snapshot(&path) {
            Ok(snapshot) => CallOutput::SnapshotLoaded { snapshot },
            Err(reason) => CallOutput::Failed(reason),
        },
        StorageJob::JournalAppend {
            path,
            record_index,
            bytes,
        } => match crate::persistence::append_journal_record(&path, record_index, bytes) {
            Ok(()) => CallOutput::JournalAppended { record_index },
            Err(reason) => CallOutput::Failed(reason),
        },
        StorageJob::JournalReplay { path } => match crate::persistence::replay_journal(&path) {
            Ok(replay) => CallOutput::JournalReplayed { replay },
            Err(reason) => CallOutput::Failed(reason),
        },
        StorageJob::PathMetadata { path } => path_metadata_output(&path),
        StorageJob::RenameReplace { from, to } => rename_replace_output(&from, &to),
        StorageJob::RemoveFile { path } => match std::fs::remove_file(&path) {
            Ok(()) => CallOutput::FileRemoved,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                CallOutput::Failed(CallError::NotFound)
            }
            Err(_) => CallOutput::Failed(CallError::Io),
        },
        StorageJob::ReadDir { path } => read_dir_output(&path),
        StorageJob::SyncParent { path } => {
            let parent = path_parent_or_current(&path);
            match crate::persistence::sync_parent_directory(parent) {
                Ok(()) => CallOutput::ParentSynced,
                Err(error) => CallOutput::Failed(error),
            }
        }
        #[cfg(test)]
        StorageJob::Park { started, release } => {
            let _ = started.send(());
            let _ = release.recv();
            CallOutput::DirectoryCreated
        }
    }
}

fn path_metadata_output(path: &std::path::Path) -> CallOutput {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let kind = if metadata.is_file() {
                PathKind::File
            } else if metadata.is_dir() {
                PathKind::Directory
            } else {
                PathKind::Other
            };
            let len = matches!(kind, PathKind::File).then_some(metadata.len());
            CallOutput::PathMetadata {
                metadata: PathMetadata { kind, len },
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => CallOutput::PathMetadata {
            metadata: PathMetadata::missing(),
        },
        Err(_) => CallOutput::Failed(CallError::Io),
    }
}

fn rename_replace_output(from: &std::path::Path, to: &std::path::Path) -> CallOutput {
    #[cfg(not(unix))]
    if to.exists() {
        return CallOutput::Failed(CallError::Unsupported);
    }

    match std::fs::rename(from, to) {
        Ok(()) => CallOutput::PathRenamed,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            CallOutput::Failed(CallError::NotFound)
        }
        Err(_) => CallOutput::Failed(CallError::Io),
    }
}

fn read_dir_output(path: &std::path::Path) -> CallOutput {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return CallOutput::Failed(CallError::NotFound);
        }
        Err(_) => return CallOutput::Failed(CallError::Io),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return CallOutput::Failed(CallError::Io);
        };
        paths.push(entry.path());
    }
    paths.sort();
    CallOutput::DirectoryRead { entries: paths }
}

fn path_parent_or_current(path: &std::path::Path) -> &std::path::Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
}

impl BetelgeuseTcp {
    fn with_io_loop(io_loop: IOLoopHandle<Global>) -> Self {
        Self {
            io_loop,
            next_listener_id: 1,
            next_stream_id: 1,
            next_udp_socket_id: 1,
            next_file_id: 1,
            listeners: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            streams: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            udp_sockets: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            files: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            pending: Vec::with_capacity(INITIAL_DRIVER_PENDING_CAPACITY),
            cancelled_by_close: Vec::new(),
        }
    }

    /// Drains calls cancelled by resource close.
    fn take_cancelled_by_close(&mut self) -> Vec<CallId> {
        std::mem::take(&mut self.cancelled_by_close)
    }

    /// Submits one runtime-owned call. Synchronous Betelgeuse ops (bind,
    /// close) finish here and the result is returned inline; async ops
    /// (accept, recv, send) push a pending entry and return [`None`].
    fn submit(&mut self, call_id: CallId, request: CallInput) -> Option<DriverCompletion> {
        match request {
            CallInput::TcpBind { addr } => Some(DriverCompletion {
                call_id,
                result: self.do_bind(addr),
            }),
            CallInput::TcpListenerClose { listener } => Some(DriverCompletion {
                call_id,
                result: self.do_listener_close(listener),
            }),
            CallInput::TcpStreamClose { stream } => Some(DriverCompletion {
                call_id,
                result: self.do_stream_close(stream),
            }),
            CallInput::UdpBind { addr } => Some(DriverCompletion {
                call_id,
                result: self.do_udp_bind(addr),
            }),
            CallInput::UdpSendTo {
                socket,
                peer,
                bytes,
            } => Some(DriverCompletion {
                call_id,
                result: self.do_udp_send_to(socket, peer, &bytes),
            }),
            CallInput::UdpSocketClose { socket } => Some(DriverCompletion {
                call_id,
                result: self.do_udp_close(socket),
            }),
            CallInput::DnsLookup { .. } => Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Unsupported),
            }),
            CallInput::SignalWait { .. } => Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Unsupported),
            }),
            CallInput::ProcessRun { .. } => Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Unsupported),
            }),
            CallInput::TlsConnect { .. }
            | CallInput::TlsBind { .. }
            | CallInput::TlsAccept { .. }
            | CallInput::TlsListenerClose { .. }
            | CallInput::TlsRead { .. }
            | CallInput::TlsWrite { .. }
            | CallInput::TlsClose { .. } => Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Unsupported),
            }),
            CallInput::PathMetadata { .. }
            | CallInput::RenameReplace { .. }
            | CallInput::RemoveFile { .. }
            | CallInput::ReadDir { .. }
            | CallInput::SyncParent { .. } => Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Unsupported),
            }),
            CallInput::UdpRecvFrom { socket, max_len } => {
                let lane = PendingLane::UdpRecv(socket);
                if self.lane_has_pending(lane) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::ResourceBusy),
                    });
                }
                if !self.udp_sockets.iter().any(|entry| entry.id == socket) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::InvalidResource),
                    });
                }
                self.pending.push(PendingOperation {
                    call_id,
                    kind: PendingKind::UdpRecv {
                        socket,
                        max_len,
                        buffer: vec![0; max_len.saturating_add(1)],
                    },
                    lane,
                    user_cancelled: false,
                    shutdown_marked: false,
                });
                None
            }
            CallInput::FileOpen { path, options } => Some(DriverCompletion {
                call_id,
                result: self.do_file_open(&path, options),
            }),
            CallInput::FileClose { file } => Some(DriverCompletion {
                call_id,
                result: self.do_file_close(file),
            }),
            CallInput::Mkdir { path, mode } => {
                let lane = PendingLane::Mkdir(call_id);
                match self.arm_mkdir(&path, mode) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            lane,
                            user_cancelled: false,
                            shutdown_marked: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::FileReadAt { file, len, offset } => {
                let lane = PendingLane::FileRead(file);
                if self.lane_has_pending(lane) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::ResourceBusy),
                    });
                }
                match self.arm_file_read(file, len, offset) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            lane,
                            user_cancelled: false,
                            shutdown_marked: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::FileWriteAt {
                file,
                bytes,
                offset,
            } => {
                let lane = PendingLane::FileWrite(file);
                if self.lane_has_pending(lane) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::ResourceBusy),
                    });
                }
                match self.arm_file_write(file, bytes, offset) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            lane,
                            user_cancelled: false,
                            shutdown_marked: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::FileFsync { file } => {
                let lane = PendingLane::FileFsync(file);
                if self.lane_has_pending(lane) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::ResourceBusy),
                    });
                }
                match self.arm_file_fsync(file) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            lane,
                            user_cancelled: false,
                            shutdown_marked: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::FileSize { file } => {
                let lane = PendingLane::FileSize(file);
                if self.lane_has_pending(lane) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::ResourceBusy),
                    });
                }
                match self.arm_file_size(file) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            lane,
                            user_cancelled: false,
                            shutdown_marked: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::TcpAccept { listener } => {
                let lane = PendingLane::ListenerAccept(listener);
                if self.lane_has_pending(lane) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::ResourceBusy),
                    });
                }
                match self.arm_accept(listener) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            lane,
                            user_cancelled: false,
                            shutdown_marked: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::TcpConnect { addr } => {
                let lane = PendingLane::TcpConnect(call_id);
                match self.arm_connect(addr) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            lane,
                            user_cancelled: false,
                            shutdown_marked: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::TcpRead { stream, max_len } => {
                let lane = PendingLane::StreamRead(stream);
                if self.lane_has_pending(lane) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::ResourceBusy),
                    });
                }
                match self.arm_read(stream, max_len) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            lane,
                            user_cancelled: false,
                            shutdown_marked: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::TcpWrite { stream, bytes } => {
                let lane = PendingLane::StreamWrite(stream);
                if self.lane_has_pending(lane) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::ResourceBusy),
                    });
                }
                match self.arm_write(stream, bytes) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            lane,
                            user_cancelled: false,
                            shutdown_marked: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::SnapshotCommit { .. }
            | CallInput::SnapshotLoad { .. }
            | CallInput::JournalAppend { .. }
            | CallInput::JournalReplay { .. }
            | CallInput::Sleep { .. } => Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Unsupported),
            }),
        }
    }

    fn lane_has_active_pending(&self, lane: PendingLane) -> bool {
        self.pending
            .iter()
            .any(|op| op.lane == lane && !op.user_cancelled && !op.shutdown_marked)
    }

    fn lane_has_pending(&self, lane: PendingLane) -> bool {
        self.pending.iter().any(|op| op.lane == lane)
    }

    fn file_has_active_pending(&self, file: FileId) -> bool {
        self.lane_has_active_pending(PendingLane::FileRead(file))
            || self.lane_has_active_pending(PendingLane::FileWrite(file))
            || self.lane_has_active_pending(PendingLane::FileFsync(file))
            || self.lane_has_active_pending(PendingLane::FileSize(file))
    }

    /// Advances Betelgeuse by one tick and harvests any pending operations
    /// whose completion slots have a result available. Returned in
    /// submission order.
    fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
        // One substrate tick. Errors here are non-fatal: pending ops still
        // hold their slots and will be checked anyway.
        let _ = self.io_loop.step();

        // Drain in submission order so completion ordering is stable
        // relative to submission ordering whenever Betelgeuse permits it.
        let mut index = 0;
        while index < self.pending.len() {
            let mut op = self.pending.remove(index);
            if op.user_cancelled || op.shutdown_marked {
                if op.kind.has_result() || op.kind.drops_on_cancel() {
                    continue;
                }
                self.pending.insert(index, op);
                index += 1;
                continue;
            }

            let result = self.try_complete(&mut op);
            match result {
                Some(result) => {
                    completed.push(DriverCompletion {
                        call_id: op.call_id,
                        result,
                    });
                }
                None => {
                    self.pending.insert(index, op);
                    index += 1;
                }
            }
        }
    }

    /// Returns whether TCP has any pending operations. Tests use
    /// this to decide whether stepping further can produce more
    /// completions.
    fn has_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|op| !op.user_cancelled && !op.shutdown_marked)
    }

    /// Cancels pending TCP operations during runtime shutdown.
    ///
    /// Tina emits requester-facing shutdown/cancel trace events from
    /// `Runtime`; TCP state only owns the substrate completion slots and
    /// resource handles. Sets `shutdown_marked` rather than the
    /// user-cancellation flag so stuck work surfaces in the terminal
    /// report's `pending_driver_call_count` while user-cancelled work
    /// stays correctly excluded.
    fn cancel_pending(&mut self, deadline: Instant) -> Result<(), DriverShutdownError> {
        for op in &mut self.pending {
            op.shutdown_marked = true;
        }
        let cancel_result = self
            .io_loop
            .cancel_pending_completions()
            .map_err(|_| DriverShutdownError::BackendStillOwnsCompletions);
        self.close_all_resources();
        self.drain_cancelled_pending_for_shutdown(deadline);
        if cancel_result.is_err()
            || !self.pending.is_empty()
            || self.io_loop.pending_completion_count() != 0
        {
            return Err(DriverShutdownError::BackendStillOwnsCompletions);
        }
        Ok(())
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let Some(index) = self
            .pending
            .iter()
            .position(|op| op.call_id == call_id && !op.user_cancelled)
        else {
            return false;
        };
        self.pending[index].user_cancelled = true;
        true
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn resource_report(&self) -> DriverResourceReport {
        DriverResourceReport {
            listeners: self.listeners.len(),
            streams: self.streams.len(),
            udp_sockets: self.udp_sockets.len(),
            files: self.files.len(),
            // Count work the runtime still waits on. User-cancelled work
            // does not count; shutdown-stuck work does.
            pending_calls: self.pending.iter().filter(|op| !op.user_cancelled).count(),
            ..DriverResourceReport::default()
        }
    }

    fn close_all_resources(&mut self) {
        for entry in std::mem::take(&mut self.listeners) {
            entry.socket.close();
        }
        for entry in std::mem::take(&mut self.streams) {
            entry.socket.close();
        }
        self.udp_sockets.clear();
        self.files.clear();
    }

    fn drain_cancelled_pending_for_shutdown(&mut self, deadline: Instant) {
        // Step the Betelgeuse IO loop until either all pending entries
        // drain or the per-shard shutdown budget elapses. Each step gives
        // the backend a chance to release ownership of completion slots.
        // The loop yields between steps so a stalled backend cannot pin
        // shutdown forever.
        loop {
            if self.pending.is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }

            let _ = self.io_loop.step();
            let mut index = 0;
            while index < self.pending.len() {
                if self.pending[index].kind.has_result()
                    || self.pending[index].kind.drops_on_cancel()
                {
                    self.pending.remove(index);
                } else {
                    index += 1;
                }
            }
        }
    }

    fn try_complete(&mut self, op: &mut PendingOperation) -> Option<CallOutput> {
        match &mut op.kind {
            PendingKind::Accept(completion) => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("accept completion advertised a result");
                match result {
                    Ok(socket) => {
                        let peer_addr = match socket.peer_addr() {
                            Ok(addr) => addr,
                            Err(_) => return Some(CallOutput::Failed(CallError::Io)),
                        };
                        let stream_id = StreamId::new(self.next_stream_id);
                        self.next_stream_id += 1;
                        self.streams.push(StreamEntry {
                            id: stream_id,
                            socket,
                        });
                        Some(CallOutput::TcpAccepted {
                            stream: stream_id,
                            peer_addr,
                        })
                    }
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            PendingKind::Connect { completion, socket } => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("connect completion advertised a result");
                match result {
                    Ok(()) => {
                        let socket = socket.take().expect("connected socket available");
                        let local_addr = match socket.local_addr() {
                            Ok(addr) => addr,
                            Err(_) => return Some(CallOutput::Failed(CallError::Io)),
                        };
                        let peer_addr = match socket.peer_addr() {
                            Ok(addr) => addr,
                            Err(_) => return Some(CallOutput::Failed(CallError::Io)),
                        };
                        let stream_id = StreamId::new(self.next_stream_id);
                        self.next_stream_id += 1;
                        self.streams.push(StreamEntry {
                            id: stream_id,
                            socket,
                        });
                        Some(CallOutput::TcpConnected {
                            stream: stream_id,
                            local_addr,
                            peer_addr,
                        })
                    }
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            PendingKind::Read(completion) => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("recv completion advertised a result");
                match result {
                    Ok(bytes) => Some(CallOutput::TcpRead { bytes }),
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            PendingKind::Write(completion) => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("send completion advertised a result");
                match result {
                    Ok(count) => Some(CallOutput::TcpWrote { count }),
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            PendingKind::UdpRecv {
                socket,
                max_len,
                buffer,
            } => {
                let entry = self.udp_sockets.iter().find(|entry| entry.id == *socket)?;
                match entry.socket.recv_from(buffer) {
                    Ok((count, peer_addr)) => {
                        let truncated = count > *max_len;
                        let delivered = count.min(*max_len);
                        Some(CallOutput::UdpReceived {
                            peer_addr,
                            bytes: buffer[..delivered].to_vec(),
                            truncated,
                        })
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => None,
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            PendingKind::FileRead(completion) => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("pread completion advertised a result");
                match result {
                    Ok(bytes) => Some(CallOutput::FileRead { bytes }),
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            PendingKind::FileWrite(completion) => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("pwrite completion advertised a result");
                match result {
                    Ok(count) => Some(CallOutput::FileWrote { count }),
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            PendingKind::FileFsync(completion) => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("fsync completion advertised a result");
                match result {
                    Ok(()) => Some(CallOutput::FileSynced),
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            PendingKind::FileSize(completion) => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("size completion advertised a result");
                match result {
                    Ok(size) => Some(CallOutput::FileSize { size }),
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            PendingKind::Mkdir(completion) => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("mkdir completion advertised a result");
                match result {
                    Ok(()) => Some(CallOutput::DirectoryCreated),
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
        }
    }

    fn do_bind(&mut self, addr: SocketAddr) -> CallOutput {
        let socket = match self.io_loop.socket() {
            Ok(socket) => socket,
            Err(_) => return CallOutput::Failed(CallError::Io),
        };
        if socket.bind(addr).is_err() {
            return CallOutput::Failed(CallError::Io);
        }
        let local_addr = match socket.local_addr() {
            Ok(addr) => addr,
            Err(_) => return CallOutput::Failed(CallError::Io),
        };

        let id = ListenerId::new(self.next_listener_id);
        self.next_listener_id += 1;
        self.listeners.push(ListenerEntry { id, socket });
        CallOutput::TcpBound {
            listener: id,
            local_addr,
        }
    }

    fn do_listener_close(&mut self, listener: ListenerId) -> CallOutput {
        // Close wins: cancel any pending accept on this listener.
        for op in self.pending.iter_mut() {
            if matches!(op.lane, PendingLane::ListenerAccept(l) if l == listener)
                && !op.user_cancelled
            {
                op.user_cancelled = true;
                self.cancelled_by_close.push(op.call_id);
            }
        }

        match self.listeners.iter().position(|entry| entry.id == listener) {
            Some(index) => {
                let entry = self.listeners.remove(index);
                entry.socket.close();
                CallOutput::TcpListenerClosed
            }
            None => CallOutput::Failed(CallError::InvalidResource),
        }
    }

    fn do_stream_close(&mut self, stream: StreamId) -> CallOutput {
        // Close wins: cancel any pending read or write on this stream.
        for op in self.pending.iter_mut() {
            let on_this_stream = match op.lane {
                PendingLane::StreamRead(s) | PendingLane::StreamWrite(s) => s == stream,
                _ => false,
            };
            if on_this_stream && !op.user_cancelled {
                op.user_cancelled = true;
                self.cancelled_by_close.push(op.call_id);
            }
        }

        match self.streams.iter().position(|entry| entry.id == stream) {
            Some(index) => {
                let entry = self.streams.remove(index);
                entry.socket.close();
                CallOutput::TcpStreamClosed
            }
            None => CallOutput::Failed(CallError::InvalidResource),
        }
    }

    fn do_udp_bind(&mut self, addr: SocketAddr) -> CallOutput {
        let socket = match UdpSocket::bind(addr) {
            Ok(socket) => socket,
            Err(_) => return CallOutput::Failed(CallError::Io),
        };
        if socket.set_nonblocking(true).is_err() {
            return CallOutput::Failed(CallError::Io);
        }
        let local_addr = match socket.local_addr() {
            Ok(addr) => addr,
            Err(_) => return CallOutput::Failed(CallError::Io),
        };

        let id = UdpSocketId::new(self.next_udp_socket_id);
        self.next_udp_socket_id += 1;
        self.udp_sockets.push(UdpSocketEntry { id, socket });
        CallOutput::UdpBound {
            socket: id,
            local_addr,
        }
    }

    fn do_udp_send_to(
        &mut self,
        socket: UdpSocketId,
        peer: SocketAddr,
        bytes: &[u8],
    ) -> CallOutput {
        let Some(entry) = self.udp_sockets.iter().find(|entry| entry.id == socket) else {
            return CallOutput::Failed(CallError::InvalidResource);
        };
        match entry.socket.send_to(bytes, peer) {
            Ok(count) => CallOutput::UdpSent { count },
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                CallOutput::Failed(CallError::ResourceBusy)
            }
            Err(_) => CallOutput::Failed(CallError::Io),
        }
    }

    fn do_udp_close(&mut self, socket: UdpSocketId) -> CallOutput {
        // Close wins: cancel any pending recv on this socket.
        for op in self.pending.iter_mut() {
            if matches!(op.lane, PendingLane::UdpRecv(s) if s == socket) && !op.user_cancelled {
                op.user_cancelled = true;
                self.cancelled_by_close.push(op.call_id);
            }
        }
        match self.udp_sockets.iter().position(|entry| entry.id == socket) {
            Some(index) => {
                self.udp_sockets.remove(index);
                CallOutput::UdpSocketClosed
            }
            None => CallOutput::Failed(CallError::InvalidResource),
        }
    }

    fn do_file_open(&mut self, path: &std::path::Path, options: FileOpenOptions) -> CallOutput {
        let options = OpenOptions {
            read: options.read,
            write: options.write,
            create: options.create,
            truncate: options.truncate,
        };
        let file = match self.io_loop.open(path, options) {
            Ok(file) => file,
            Err(_) => return CallOutput::Failed(CallError::Io),
        };
        let id = FileId::new(self.next_file_id);
        self.next_file_id += 1;
        self.files.push(FileEntry { id, file });
        CallOutput::FileOpened { file: id }
    }

    fn do_file_close(&mut self, file: FileId) -> CallOutput {
        if self.file_has_active_pending(file) {
            return CallOutput::Failed(CallError::ResourceBusy);
        }

        match self.files.iter().position(|entry| entry.id == file) {
            Some(index) => {
                self.files.remove(index);
                CallOutput::FileClosed
            }
            None => CallOutput::Failed(CallError::InvalidResource),
        }
    }

    fn arm_accept(&mut self, listener: ListenerId) -> Result<PendingKind, CallOutput> {
        let entry = self
            .listeners
            .iter()
            .find(|entry| entry.id == listener)
            .ok_or(CallOutput::Failed(CallError::InvalidResource))?;
        let mut completion = Box::new(AcceptCompletion::new());
        if entry.socket.accept(&mut completion).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::Accept(completion))
    }

    fn arm_connect(&mut self, addr: SocketAddr) -> Result<PendingKind, CallOutput> {
        let socket = self
            .io_loop
            .socket()
            .map_err(|_| CallOutput::Failed(CallError::Io))?;
        let mut completion = Box::new(ConnectCompletion::new());
        if socket.connect(&mut completion, addr).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::Connect {
            completion,
            socket: Some(socket),
        })
    }

    fn arm_read(&mut self, stream: StreamId, max_len: usize) -> Result<PendingKind, CallOutput> {
        let entry = self
            .streams
            .iter()
            .find(|entry| entry.id == stream)
            .ok_or(CallOutput::Failed(CallError::InvalidResource))?;
        let mut completion = Box::new(RecvCompletion::new());
        if entry.socket.recv(&mut completion, max_len).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::Read(completion))
    }

    fn arm_write(&mut self, stream: StreamId, bytes: Vec<u8>) -> Result<PendingKind, CallOutput> {
        let entry = self
            .streams
            .iter()
            .find(|entry| entry.id == stream)
            .ok_or(CallOutput::Failed(CallError::InvalidResource))?;
        let mut completion = Box::new(SendCompletion::new());
        if entry.socket.send(&mut completion, bytes).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::Write(completion))
    }

    fn arm_file_read(
        &mut self,
        file: FileId,
        len: usize,
        offset: u64,
    ) -> Result<PendingKind, CallOutput> {
        let entry = self
            .files
            .iter()
            .find(|entry| entry.id == file)
            .ok_or(CallOutput::Failed(CallError::InvalidResource))?;
        let mut completion = Box::new(PReadCompletion::new());
        if entry.file.pread(&mut completion, len, offset).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::FileRead(completion))
    }

    fn arm_file_write(
        &mut self,
        file: FileId,
        bytes: Vec<u8>,
        offset: u64,
    ) -> Result<PendingKind, CallOutput> {
        let entry = self
            .files
            .iter()
            .find(|entry| entry.id == file)
            .ok_or(CallOutput::Failed(CallError::InvalidResource))?;
        let mut completion = Box::new(PWriteCompletion::new());
        if entry.file.pwrite(&mut completion, bytes, offset).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::FileWrite(completion))
    }

    fn arm_file_fsync(&mut self, file: FileId) -> Result<PendingKind, CallOutput> {
        let entry = self
            .files
            .iter()
            .find(|entry| entry.id == file)
            .ok_or(CallOutput::Failed(CallError::InvalidResource))?;
        let mut completion = Box::new(FsyncCompletion::new());
        if entry.file.fsync(&mut completion).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::FileFsync(completion))
    }

    fn arm_file_size(&mut self, file: FileId) -> Result<PendingKind, CallOutput> {
        let entry = self
            .files
            .iter()
            .find(|entry| entry.id == file)
            .ok_or(CallOutput::Failed(CallError::InvalidResource))?;
        let mut completion = Box::new(SizeCompletion::new());
        if entry.file.size(&mut completion).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::FileSize(completion))
    }

    fn arm_mkdir(&mut self, path: &std::path::Path, mode: u32) -> Result<PendingKind, CallOutput> {
        let mut completion = Box::new(MkdirCompletion::new());
        if self.io_loop.mkdir(&mut completion, path, mode).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::Mkdir(completion))
    }
}

impl PendingKind {
    fn has_result(&self) -> bool {
        match self {
            Self::Accept(completion) => completion.has_result(),
            Self::Connect { completion, .. } => completion.has_result(),
            Self::Read(completion) => completion.has_result(),
            Self::Write(completion) => completion.has_result(),
            Self::UdpRecv { .. } => false,
            Self::FileRead(completion) => completion.has_result(),
            Self::FileWrite(completion) => completion.has_result(),
            Self::FileFsync(completion) => completion.has_result(),
            Self::FileSize(completion) => completion.has_result(),
            Self::Mkdir(completion) => completion.has_result(),
        }
    }

    fn drops_on_cancel(&self) -> bool {
        matches!(self, Self::UdpRecv { .. })
    }
}

impl TlsCommand {
    fn call_id(&self) -> CallId {
        match self {
            Self::Bind { call_id, .. }
            | Self::Accept { call_id, .. }
            | Self::Connect { call_id, .. }
            | Self::Read { call_id, .. }
            | Self::Write { call_id, .. }
            | Self::Close { call_id, .. } => *call_id,
        }
    }

    fn timeout(&self) -> Duration {
        match self {
            Self::Bind { .. } => Duration::from_secs(86_400),
            Self::Accept { timeout, .. }
            | Self::Connect { timeout, .. }
            | Self::Read { timeout, .. }
            | Self::Write { timeout, .. }
            | Self::Close { timeout, .. } => *timeout,
        }
    }

    fn set_cancelled(&mut self, cancelled: Arc<AtomicBool>) {
        match self {
            Self::Bind {
                cancelled: slot, ..
            }
            | Self::Accept {
                cancelled: slot, ..
            }
            | Self::Connect {
                cancelled: slot, ..
            }
            | Self::Read {
                cancelled: slot, ..
            }
            | Self::Write {
                cancelled: slot, ..
            }
            | Self::Close {
                cancelled: slot, ..
            } => *slot = cancelled,
        }
    }
}

impl std::fmt::Debug for BetelgeuseDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BetelgeuseDriver")
            .field("tcp", &self.tcp)
            .field("timers", &self.timers.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for BetelgeuseTcp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BetelgeuseTcp")
            .field("listeners", &self.listeners.len())
            .field("streams", &self.streams.len())
            .field("udp_sockets", &self.udp_sockets.len())
            .field("files", &self.files.len())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_driver_storage_completes_inline_without_pending_lane() {
        let io_loop =
            io_loop(Global).expect("failed to initialise Betelgeuse IO loop for driver test");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let completion = driver
            .submit(
                CallId::new(30),
                CallInput::SnapshotLoad {
                    path: PathBuf::from("missing-snapshot"),
                },
                Instant::now(),
            )
            .expect("explicit driver returns storage completion inline");

        assert_eq!(completion.call_id, CallId::new(30));
        assert!(matches!(
            completion.result,
            CallOutput::SnapshotLoaded { snapshot: None }
        ));
        assert!(!driver.has_pending());
    }

    #[test]
    fn storage_lane_rejects_full_without_sleep_as_proof() {
        let mut lane = StorageLane::new(1);
        let (started_tx, started_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);

        assert!(
            lane.submit(
                CallId::new(1),
                StorageJob::Park {
                    started: started_tx,
                    release: release_rx,
                },
            )
            .is_none()
        );
        started_rx.recv().expect("parked storage job started");

        let full = lane
            .submit(
                CallId::new(2),
                StorageJob::SnapshotLoad {
                    path: PathBuf::from("full"),
                },
            )
            .expect("second active storage job rejected");
        assert_eq!(full.call_id, CallId::new(2));
        assert!(matches!(
            full.result,
            CallOutput::Failed(CallError::StorageFull)
        ));

        release_tx.send(()).expect("release parked storage job");
        lane.cancel_pending(Instant::now());
    }

    #[test]
    fn storage_lane_cancellation_swallows_late_completion() {
        let mut lane = StorageLane::new(1);
        let (started_tx, started_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);

        assert!(
            lane.submit(
                CallId::new(7),
                StorageJob::Park {
                    started: started_tx,
                    release: release_rx,
                },
            )
            .is_none()
        );
        started_rx.recv().expect("parked storage job started");
        assert!(lane.cancel(CallId::new(7)));
        assert!(!lane.has_pending());

        release_tx.send(()).expect("release parked storage job");
        let mut completed = Vec::new();
        for _ in 0..64 {
            lane.advance(&mut completed);
            if !completed.is_empty() {
                break;
            }
            thread::yield_now();
        }
        assert!(completed.is_empty());
    }

    #[test]
    fn dns_lane_resolves_with_injected_resolver() {
        let (done_tx, done_rx) = sync_channel(1);
        let addr: SocketAddr = "127.0.0.1:4040".parse().expect("valid test address");
        let mut lane = DnsWorkerLane::new(
            1,
            Arc::new(move |host, port| {
                assert_eq!(host, "llama.test");
                assert_eq!(port, 4040);
                done_tx.send(()).expect("resolver completion observed");
                CallOutput::DnsResolved { addrs: vec![addr] }
            }),
        );
        let now = Instant::now();
        assert!(
            lane.submit(
                CallId::new(1),
                "llama.test".to_string(),
                4040,
                Duration::from_secs(1),
                now,
            )
            .is_none()
        );
        done_rx.recv().expect("resolver ran");

        let mut completed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            lane.advance(now, &mut completed);
            if !completed.is_empty() {
                break;
            }
            thread::yield_now();
        }

        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].call_id, CallId::new(1));
        assert!(matches!(
            &completed[0].result,
            CallOutput::DnsResolved { addrs } if addrs == &vec![addr]
        ));
        assert!(!lane.has_pending());
    }

    #[test]
    fn dns_lane_timeout_tombstones_and_keeps_capacity_until_late_completion() {
        use std::sync::{Condvar, Mutex};

        let started = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let finished = Arc::new((Mutex::new(false), Condvar::new()));
        let started_for_resolver = Arc::clone(&started);
        let release_for_resolver = Arc::clone(&release);
        let finished_for_resolver = Arc::clone(&finished);
        let mut lane = DnsWorkerLane::new(
            1,
            Arc::new(move |_, _| {
                let (started_lock, started_cv) = &*started_for_resolver;
                *started_lock.lock().expect("started lock") = true;
                started_cv.notify_one();

                let (release_lock, release_cv) = &*release_for_resolver;
                let mut released = release_lock.lock().expect("release lock");
                while !*released {
                    released = release_cv.wait(released).expect("release wait");
                }
                let (finished_lock, finished_cv) = &*finished_for_resolver;
                *finished_lock.lock().expect("finished lock") = true;
                finished_cv.notify_one();
                CallOutput::DnsResolved {
                    addrs: vec!["127.0.0.1:55".parse().expect("valid test address")],
                }
            }),
        );
        let now = Instant::now();
        assert!(
            lane.submit(
                CallId::new(1),
                "slow.test".to_string(),
                55,
                Duration::from_millis(1),
                now,
            )
            .is_none()
        );

        let (started_lock, started_cv) = &*started;
        let mut started_guard = started_lock.lock().expect("started lock");
        while !*started_guard {
            started_guard = started_cv.wait(started_guard).expect("started wait");
        }
        drop(started_guard);

        let mut completed = Vec::new();
        lane.advance(now + Duration::from_millis(2), &mut completed);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].call_id, CallId::new(1));
        assert!(matches!(
            completed[0].result,
            CallOutput::Failed(CallError::Timeout)
        ));
        assert!(!lane.has_pending());

        let full = lane
            .submit(
                CallId::new(2),
                "other.test".to_string(),
                55,
                Duration::from_secs(1),
                now,
            )
            .expect("timed-out started work still occupies bounded DNS lane");
        assert!(matches!(
            full.result,
            CallOutput::Failed(CallError::DnsFull)
        ));

        let (release_lock, release_cv) = &*release;
        *release_lock.lock().expect("release lock") = true;
        release_cv.notify_one();

        let (finished_lock, finished_cv) = &*finished;
        let mut finished_guard = finished_lock.lock().expect("finished lock");
        while !*finished_guard {
            finished_guard = finished_cv.wait(finished_guard).expect("finished wait");
        }
        drop(finished_guard);

        let mut late = Vec::new();
        for _ in 0..1024 {
            lane.advance(now + Duration::from_millis(3), &mut late);
            if lane.unresolved_pending_count() == 0 {
                break;
            }
            thread::yield_now();
        }
        assert!(late.is_empty());
        assert_eq!(lane.unresolved_pending_count(), 0);
    }

    #[test]
    fn tls_lane_connects_writes_reads_and_closes_against_local_rustls_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("generate local cert");
        let cert_der = certified.cert.der().to_vec();
        let key_der = certified.key_pair.serialize_der();
        let server_cert = rustls::pki_types::CertificateDer::from(cert_der.clone());
        let server_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
        );
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![server_cert], server_key)
            .expect("server config");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind tls test listener");
        let addr = listener.local_addr().expect("local addr");
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().expect("accept tls client");
            let connection =
                rustls::ServerConnection::new(Arc::new(server_config)).expect("server conn");
            let mut stream = rustls::StreamOwned::new(connection, tcp);
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).expect("read request");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").expect("write response");
            stream.flush().expect("flush response");
        });

        let mut lane = TlsWorkerLane::new(4);
        assert!(
            lane.submit_connect(
                CallId::new(1),
                addr,
                "localhost".to_string(),
                vec![cert_der],
                Duration::from_secs(1),
                Instant::now(),
            )
            .is_none()
        );
        let stream = wait_for_tls_completion(&mut lane, CallId::new(1), |output| match output {
            CallOutput::TlsConnected { stream } => Some(stream),
            other => panic!("unexpected TLS connect output: {other:?}"),
        });

        assert!(
            lane.submit_write(
                CallId::new(2),
                stream,
                b"ping".to_vec(),
                Duration::from_secs(1),
                Instant::now(),
            )
            .is_none()
        );
        let wrote = wait_for_tls_completion(&mut lane, CallId::new(2), |output| match output {
            CallOutput::TlsWrote { count } => Some(count),
            other => panic!("unexpected TLS write output: {other:?}"),
        });
        assert_eq!(wrote, 4);

        assert!(
            lane.submit_read(
                CallId::new(3),
                stream,
                4,
                Duration::from_secs(1),
                Instant::now()
            )
            .is_none()
        );
        let read = wait_for_tls_completion(&mut lane, CallId::new(3), |output| match output {
            CallOutput::TlsRead { bytes } => Some(bytes),
            other => panic!("unexpected TLS read output: {other:?}"),
        });
        assert_eq!(read, b"pong");

        assert!(
            lane.submit_close(
                CallId::new(4),
                stream,
                Duration::from_secs(1),
                Instant::now()
            )
            .is_none()
        );
        wait_for_tls_completion(&mut lane, CallId::new(4), |output| match output {
            CallOutput::TlsClosed => Some(()),
            other => panic!("unexpected TLS close output: {other:?}"),
        });
        server.join().expect("server thread");
    }

    #[test]
    fn tls_lane_deadline_tombstones_queued_work_until_late_completion() {
        let (command_sender, _command_receiver) = sync_channel(1);
        let (completion_sender, completions) = sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let now = Instant::now();
        let mut lane = TlsWorkerLane {
            capacity: 1,
            sender: Some(command_sender),
            completions,
            handle: None,
            pending: vec![TlsPending {
                call_id: CallId::new(42),
                lane: TlsPendingLane::Connect(CallId::new(42)),
                deadline: now + Duration::from_millis(1),
                cancelled: Arc::clone(&cancelled),
                timed_out: false,
            }],
            listeners: Vec::new(),
            streams: Vec::new(),
            next_listener_id: 1,
            next_stream_id: 1,
        };

        let mut completed = Vec::new();
        lane.advance(now + Duration::from_millis(2), &mut completed);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].call_id, CallId::new(42));
        assert!(matches!(
            completed[0].result,
            CallOutput::Failed(CallError::Timeout)
        ));
        assert!(cancelled.load(Ordering::Acquire));
        assert!(!lane.has_pending());
        assert_eq!(lane.pending.len(), 1);

        completion_sender
            .send(TlsCompletion {
                call_id: CallId::new(42),
                result: TlsCompletionResult::Output(CallOutput::TlsClosed),
            })
            .expect("send late TLS completion");
        completed.clear();
        lane.advance(now + Duration::from_millis(3), &mut completed);
        assert!(completed.is_empty());
        assert!(lane.pending.is_empty());
    }

    fn wait_for_tls_completion<T>(
        lane: &mut TlsWorkerLane,
        call_id: CallId,
        map: impl Fn(CallOutput) -> Option<T>,
    ) -> T {
        let mut completed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            lane.advance(Instant::now(), &mut completed);
            if let Some(index) = completed
                .iter()
                .position(|completion| completion.call_id == call_id)
            {
                let completion = completed.remove(index);
                return map(completion.result).expect("mapped TLS completion");
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("TLS completion {call_id:?} did not arrive");
    }

    #[test]
    fn storage_lane_shutdown_skips_buffered_work_that_never_started() {
        let mut lane = StorageLane::new(2);
        let (first_started_tx, first_started_rx) = sync_channel(1);
        let (first_release_tx, first_release_rx) = sync_channel(1);
        let (queued_started_tx, queued_started_rx) = sync_channel(1);
        let (_queued_release_tx, queued_release_rx) = sync_channel(1);

        assert!(
            lane.submit(
                CallId::new(11),
                StorageJob::Park {
                    started: first_started_tx,
                    release: first_release_rx,
                },
            )
            .is_none()
        );
        first_started_rx
            .recv()
            .expect("first parked storage job started");
        assert!(
            lane.submit(
                CallId::new(12),
                StorageJob::Park {
                    started: queued_started_tx,
                    release: queued_release_rx,
                },
            )
            .is_none()
        );

        lane.cancel(CallId::new(12));
        first_release_tx
            .send(())
            .expect("release first parked storage job");
        lane.cancel_pending(Instant::now());
        assert!(
            queued_started_rx.try_recv().is_err(),
            "queued storage work must not start after shutdown cancellation"
        );
    }

    #[test]
    fn udp_recv_lane_rejects_duplicate_and_close_cancels_pending_recv() {
        let io_loop =
            io_loop(Global).expect("failed to initialise Betelgeuse IO loop for driver test");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let bound = driver
            .submit(
                CallId::new(1),
                CallInput::UdpBind {
                    addr: "127.0.0.1:0".parse().expect("loopback addr"),
                },
                Instant::now(),
            )
            .expect("udp bind completes inline");
        let socket = match bound.result {
            CallOutput::UdpBound { socket, .. } => socket,
            other => panic!("unexpected udp bind output {other:?}"),
        };

        assert!(
            driver
                .submit(
                    CallId::new(2),
                    CallInput::UdpRecvFrom { socket, max_len: 8 },
                    Instant::now(),
                )
                .is_none()
        );

        let duplicate = driver
            .submit(
                CallId::new(3),
                CallInput::UdpRecvFrom { socket, max_len: 8 },
                Instant::now(),
            )
            .expect("duplicate udp recv rejected inline");
        assert!(matches!(
            duplicate.result,
            CallOutput::Failed(CallError::ResourceBusy)
        ));

        // Close wins. Pending recv is cancelled.
        let closed = driver
            .submit(
                CallId::new(4),
                CallInput::UdpSocketClose { socket },
                Instant::now(),
            )
            .expect("udp close completes inline even with a pending recv");
        assert!(matches!(closed.result, CallOutput::UdpSocketClosed));
        assert!(
            !driver.has_pending(),
            "the pending recv was cancelled by the close and is no longer counted as in-flight"
        );
    }

    // -------------------------------------------------------------------
    // Phase 043 Rock 2: resource-accounting count rules.
    //
    // One narrow test per lane verifies how each lane contributes to the
    // three independent vocabularies in DriverResourceReport:
    // table-owned, worker-held, and pending. Tests live at the
    // BetelgeuseDriver level where possible so the aggregator at
    // BetelgeuseDriver::resource_report is exercised end to end.
    // -------------------------------------------------------------------

    #[test]
    fn fresh_driver_reports_all_zero_counts() {
        let io_loop = io_loop(Global).expect("init io loop");
        let driver = BetelgeuseDriver::with_io_loop(io_loop);
        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 0);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 0);
    }

    #[test]
    fn tcp_listener_counts_as_table_owned_only() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let bound = driver
            .submit(
                CallId::new(1),
                CallInput::TcpBind {
                    addr: "127.0.0.1:0".parse().expect("loopback"),
                },
                Instant::now(),
            )
            .expect("bind completes inline");
        let listener = match bound.result {
            CallOutput::TcpBound { listener, .. } => listener,
            other => panic!("unexpected bind result: {other:?}"),
        };

        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 1);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 0);

        // A pending accept on that listener increments pending only;
        // no extra worker-held resource is created (the listener
        // table id already covers the accept's working state).
        assert!(
            driver
                .submit(
                    CallId::new(2),
                    CallInput::TcpAccept { listener },
                    Instant::now(),
                )
                .is_none()
        );
        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 1);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 1);

        assert!(driver.cancel(CallId::new(2)));
        // After Phase 043, the pending count tracks physical entries so
        // cancelled-but-not-yet-drained ops stay visible until the
        // backend releases the completion slot. After a shutdown drain
        // the count must reach zero again.
        let _ = driver.cancel_pending(Instant::now() + Duration::from_millis(100));
        assert_eq!(driver.resource_report().pending_driver_call_count(), 0);
    }

    #[test]
    fn udp_recv_pending_increments_pending_only() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let bound = driver
            .submit(
                CallId::new(1),
                CallInput::UdpBind {
                    addr: "127.0.0.1:0".parse().expect("loopback"),
                },
                Instant::now(),
            )
            .expect("udp bind inline");
        let socket = match bound.result {
            CallOutput::UdpBound { socket, .. } => socket,
            other => panic!("unexpected udp bind: {other:?}"),
        };

        assert!(
            driver
                .submit(
                    CallId::new(2),
                    CallInput::UdpRecvFrom { socket, max_len: 8 },
                    Instant::now(),
                )
                .is_none()
        );
        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 1);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 1);
    }

    #[test]
    fn signal_wait_counts_as_pending_only() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        assert!(
            driver
                .submit(
                    CallId::new(1),
                    CallInput::SignalWait {
                        name: "sigterm".to_string(),
                        timeout: Duration::from_secs(60),
                    },
                    Instant::now(),
                )
                .is_none()
        );
        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 0);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 1);
    }

    #[test]
    fn timer_pending_counts_as_pending_only() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        assert!(
            driver
                .submit(
                    CallId::new(1),
                    CallInput::Sleep {
                        after: Duration::from_secs(60),
                    },
                    Instant::now(),
                )
                .is_none()
        );
        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 0);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 1);
    }

    #[test]
    fn storage_park_counts_as_pending_only() {
        let mut lane = StorageLane::new(1);
        let (started_tx, started_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        assert!(
            lane.submit(
                CallId::new(7),
                StorageJob::Park {
                    started: started_tx,
                    release: release_rx,
                },
            )
            .is_none()
        );
        started_rx.recv().expect("park job started");
        // Storage contributes to pending_calls but not worker_held: the
        // worker thread does the OS work but the runtime does not see a
        // separate handle.
        assert_eq!(lane.physical_pending_count(), 1);

        release_tx.send(()).expect("release park job");
        lane.cancel_pending(Instant::now());
    }

    #[test]
    fn dns_pending_counts_as_pending_only() {
        let (started_tx, started_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let mut lane = DnsWorkerLane::new(
            1,
            Arc::new(move |_, _| {
                if let Some(tx) = started_tx.lock().expect("started lock").take() {
                    let _ = tx.send(());
                }
                if let Some(rx) = release_rx.lock().expect("release lock").take() {
                    let _ = rx.recv();
                }
                CallOutput::DnsResolved { addrs: vec![] }
            }),
        );
        assert!(
            lane.submit(
                CallId::new(1),
                "blocking.test".to_string(),
                4040,
                Duration::from_secs(60),
                Instant::now(),
            )
            .is_none()
        );
        started_rx.recv().expect("resolver started");
        // DNS contributes to pending_calls only.
        assert_eq!(lane.unresolved_pending_count(), 1);

        release_tx.send(()).expect("release dns");
        lane.cancel_pending(Instant::now());
    }

    #[test]
    fn process_pending_contributes_to_worker_held_and_pending() {
        let mut lane = ProcessLane::new(1);
        // Pick a long-running process so the call stays pending while we
        // measure. Use a Unix sleep; on non-Unix the test is skipped.
        if cfg!(not(unix)) {
            return;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        assert!(
            lane.submit(
                CallId::new(1),
                ProcessCommand {
                    call_id: CallId::new(1),
                    command: "/bin/sleep".to_string(),
                    args: vec!["5".to_string()],
                    timeout: Duration::from_secs(10),
                    stdout_limit: 1024,
                    stderr_limit: 1024,
                    cancelled: Arc::clone(&cancelled),
                },
            )
            .is_none()
        );
        // Process call is both pending and worker-held: the worker thread
        // owns a live std::process::Child while the call is in flight.
        assert_eq!(lane.physical_pending_count(), 1);

        cancelled.store(true, Ordering::Release);
        lane.cancel_pending(Instant::now());
    }

    #[test]
    fn tls_pending_op_contributes_to_worker_held_and_pending() {
        // Drive a TLS connect to a non-listening port so the lane stays
        // in flight long enough to observe the count. The connect will
        // eventually fail, but during the in-flight window the lane
        // reports both worker-held and pending.
        let mut lane = TlsWorkerLane::new(2);
        let unreachable: SocketAddr = "127.0.0.1:1".parse().expect("loopback addr");
        assert!(
            lane.submit_connect(
                CallId::new(1),
                unreachable,
                "localhost".to_string(),
                vec![],
                Duration::from_secs(2),
                Instant::now(),
            )
            .is_none()
        );
        let report = lane.resource_report();
        assert_eq!(
            report.worker_held_resource_count(),
            report.pending_driver_call_count(),
            "TLS in-flight ops contribute equally to worker_held and pending"
        );
        assert!(report.pending_driver_call_count() >= 1);

        // Drain so the lane drop does not race with the live connect.
        let mut completed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            lane.advance(Instant::now(), &mut completed);
            if !completed.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    // -------------------------------------------------------------------
    // Phase 043 Rock 4: OS signal capture.
    //
    // The dispatcher converts process-wide SIGINT/SIGTERM flag bits set
    // by signal-hook into runtime-owned signal completions. On non-Unix
    // the dispatcher is a no-op and `os_signal_capture_supported()`
    // returns false.
    // -------------------------------------------------------------------

    #[test]
    fn os_signal_capture_supported_matches_target() {
        assert_eq!(os_signal_capture_supported(), cfg!(unix));
    }

    // Tests touching the process-global OS signal state must run
    // serially so they do not steal each other's flag bits.
    #[cfg(unix)]
    fn os_signal_test_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(unix)]
    #[test]
    fn dispatcher_consumes_sigint_flag_once_per_delivery() {
        // Private dispatcher: not affected by parallel tests touching
        // the global signal-hook flag.
        let dispatcher = OsSignalDispatcher::private_for_test();
        dispatcher.sigint.store(true, Ordering::Release);
        assert!(dispatcher.consume_sigint());
        assert!(!dispatcher.consume_sigint());
    }

    #[cfg(unix)]
    #[test]
    fn betelgeuse_driver_fires_sigint_signal_when_dispatcher_flag_set() {
        // Use a private (non-shared) dispatcher so this test cannot race
        // with other drivers in parallel test runs. The wiring from
        // dispatcher → poll_os_signals → notify_signal is the same.
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        driver.os_signals = OsSignalDispatcher::private_for_test();
        assert!(
            driver
                .submit(
                    CallId::new(1),
                    CallInput::SignalWait {
                        name: "sigint".to_string(),
                        timeout: Duration::from_secs(60),
                    },
                    Instant::now(),
                )
                .is_none()
        );
        driver.os_signals.sigint.store(true, Ordering::Release);
        let mut completed = Vec::new();
        driver.advance(Instant::now(), &mut completed);
        assert!(
            completed.iter().any(|c| matches!(
                &c.result,
                CallOutput::SignalReceived { name } if name == "sigint"
            )),
            "advance must deliver a SignalReceived completion when the dispatcher fires"
        );
    }

    #[cfg(unix)]
    #[test]
    fn raised_sigint_reaches_runtime_owned_signal_wait() {
        let _guard = os_signal_test_lock();
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let _ = driver.os_signals.consume_sigint();
        assert!(
            driver
                .submit(
                    CallId::new(1),
                    CallInput::SignalWait {
                        name: "sigint".to_string(),
                        timeout: Duration::from_secs(5),
                    },
                    Instant::now(),
                )
                .is_none()
        );
        // Use signal-hook's raise helper so the real OS signal path fires
        // the registered flag handler. Cargo's test runner survives
        // because our handler only sets the flag and does not terminate.
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).expect("raise SIGINT to self");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut completed = Vec::new();
        while Instant::now() < deadline {
            driver.advance(Instant::now(), &mut completed);
            if completed.iter().any(|c| {
                matches!(
                    &c.result,
                    CallOutput::SignalReceived { name } if name == "sigint"
                )
            }) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("raised SIGINT did not reach the runtime-owned signal wait");
    }

    // -------------------------------------------------------------------
    // Phase 043 Rock 3: bounded shutdown drain.
    //
    // Each lane's `cancel_pending(deadline)` must return inside the
    // budget even when the worker is stuck, surfacing remaining work via
    // the pending count rather than blocking shutdown forever.
    // -------------------------------------------------------------------

    #[test]
    fn storage_lane_shutdown_returns_within_budget_when_worker_stuck() {
        let mut lane = StorageLane::new(1);
        let (started_tx, started_rx) = sync_channel(1);
        // Park job that will not be released — simulates a stuck worker.
        let (_release_tx, release_rx) = sync_channel(1);
        assert!(
            lane.submit(
                CallId::new(1),
                StorageJob::Park {
                    started: started_tx,
                    release: release_rx,
                },
            )
            .is_none()
        );
        started_rx.recv().expect("park job started");

        let budget = Duration::from_millis(50);
        let started = Instant::now();
        lane.cancel_pending(Instant::now() + budget);
        let elapsed = started.elapsed();

        // Bounded: must not exceed budget by an order of magnitude even
        // under load on a busy CI machine.
        assert!(
            elapsed < budget * 10,
            "shutdown took {elapsed:?}, expected to return near {budget:?}"
        );
        // Stuck work stays visible in the physical count.
        assert!(
            lane.physical_pending_count() >= 1,
            "stuck park job must remain visible after bounded shutdown"
        );
    }

    #[test]
    fn betelgeuse_tcp_shutdown_returns_within_budget() {
        // Even with an in-flight TCP accept, bounded shutdown must
        // return promptly. The backend may not release the completion
        // slot inside the budget, but the call must not hang.
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let bound = driver
            .submit(
                CallId::new(1),
                CallInput::TcpBind {
                    addr: "127.0.0.1:0".parse().expect("loopback"),
                },
                Instant::now(),
            )
            .expect("bind inline");
        let listener = match bound.result {
            CallOutput::TcpBound { listener, .. } => listener,
            other => panic!("unexpected bind: {other:?}"),
        };
        assert!(
            driver
                .submit(
                    CallId::new(2),
                    CallInput::TcpAccept { listener },
                    Instant::now(),
                )
                .is_none()
        );

        let budget = Duration::from_millis(50);
        let started = Instant::now();
        let _ = driver.cancel_pending(Instant::now() + budget);
        let elapsed = started.elapsed();
        assert!(
            elapsed < budget * 10,
            "TCP shutdown took {elapsed:?}, expected to return near {budget:?}"
        );
    }

    #[test]
    fn cancel_drains_do_not_double_count() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        assert!(
            driver
                .submit(
                    CallId::new(1),
                    CallInput::Sleep {
                        after: Duration::from_secs(60),
                    },
                    Instant::now(),
                )
                .is_none()
        );
        assert_eq!(driver.resource_report().pending_driver_call_count(), 1);
        assert!(driver.cancel(CallId::new(1)));
        // Once a pending op is cancelled it must not contribute to the
        // pending count, and the cancelled signal/timer entry must drop
        // out cleanly without double counting on advance.
        let report = driver.resource_report();
        assert_eq!(report.pending_driver_call_count(), 0);
        let mut completed = Vec::new();
        driver.advance(Instant::now(), &mut completed);
        assert_eq!(driver.resource_report().pending_driver_call_count(), 0);
    }
}
