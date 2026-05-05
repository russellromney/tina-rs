//! Runtime-owned substrate driver for `tina-runtime`.
//!
//! Tina keeps isolate scheduling, mailboxes, tracing, supervision, and call
//! outcome delivery in [`crate::Runtime`]. The driver owns only substrate
//! operations: timers, TCP resources, completion readiness, and cancellation.
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
//! - shutdown cancellation keeps completion slots alive until the backend
//!   reports that it no longer owns their raw pointers. A driver that cannot
//!   prove release returns [`DriverShutdownError`] instead of pretending
//!   shutdown was clean.
//!
use std::alloc::Global;
use std::net::SocketAddr;
use std::time::Instant;

use betelgeuse::{
    AcceptCompletion, ConnectCompletion, FsyncCompletion, IO, IOFile, IOLoop, IOLoopHandle,
    IOSocket, MkdirCompletion, OpenOptions, PReadCompletion, PWriteCompletion, RecvCompletion,
    SendCompletion, SizeCompletion, io_loop,
};

use crate::call::{
    CallError, CallId, CallInput, CallOutput, FileId, FileOpenOptions, ListenerId, StreamId,
};

const INITIAL_DRIVER_TIMER_CAPACITY: usize = 8;
const INITIAL_DRIVER_RESOURCE_CAPACITY: usize = 4;
const INITIAL_DRIVER_PENDING_CAPACITY: usize = 8;

/// Runtime-owned substrate driver.
///
/// A driver must not run user isolate code, own isolate mailboxes, or hide an
/// unbounded executor behind Tina. It owns only substrate calls submitted by
/// [`crate::Runtime`], advances them when the runtime asks, and returns typed
/// completions for the runtime to deliver on later turns.
///
/// TCP drivers must treat listener accept, stream read, and stream write as
/// separate pending lanes. Duplicate work on one lane fails with
/// [`CallError::ResourceBusy`]; closing a listener or stream while any relevant
/// lane is pending also fails with [`CallError::ResourceBusy`]. Per-call cancel
/// must stop requester completion and quiescence pressure without silently
/// invalidating unrelated active lanes.
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
    /// After this returns `Ok(())`, the driver may be dropped without leaving
    /// backend-owned pointers to driver-owned completion storage. `Err` means
    /// shutdown reached a typed lifecycle failure.
    fn cancel_pending(&mut self) -> Result<(), DriverShutdownError>;

    /// Cancels one runtime-owned call by id.
    ///
    /// Cancellation removes completion delivery responsibility from the
    /// driver. It is not a promise that an already-submitted substrate side
    /// effect, such as a TCP write handed to the OS, can be undone.
    fn cancel(&mut self, call_id: CallId) -> bool;

    #[cfg(test)]
    fn io_pending_count(&self) -> usize {
        0
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
    timers: Vec<TimerEntry>,
    next_timer_ordinal: u64,
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
    next_file_id: u64,
    listeners: Vec<ListenerEntry>,
    streams: Vec<StreamEntry>,
    files: Vec<FileEntry>,
    pending: Vec<PendingOperation>,
}

struct ListenerEntry {
    id: ListenerId,
    socket: Box<dyn IOSocket>,
}

struct StreamEntry {
    id: StreamId,
    socket: Box<dyn IOSocket>,
}

struct FileEntry {
    id: FileId,
    file: Box<dyn IOFile>,
}

struct PendingOperation {
    call_id: CallId,
    kind: PendingKind,
    lane: PendingLane,
    cancelled: bool,
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

impl BetelgeuseDriver {
    pub(crate) fn new() -> Self {
        let io_loop =
            io_loop(Global).expect("failed to initialise Betelgeuse IO loop for tina-runtime");
        Self::with_io_loop(io_loop)
    }

    pub(crate) fn with_io_loop(io_loop: IOLoopHandle<Global>) -> Self {
        Self {
            tcp: BetelgeuseTcp::with_io_loop(io_loop),
            timers: Vec::with_capacity(INITIAL_DRIVER_TIMER_CAPACITY),
            next_timer_ordinal: 0,
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
            other => self.tcp.submit(call_id, other),
        }
    }

    fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        self.tcp.advance(completed);
        self.harvest_timers(now, completed);
    }

    fn has_pending(&self) -> bool {
        self.tcp.has_pending() || !self.timers.is_empty()
    }

    fn cancel_pending(&mut self) -> Result<(), DriverShutdownError> {
        self.timers.clear();
        self.tcp.cancel_pending()
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let before = self.timers.len();
        self.timers.retain(|entry| entry.call_id != call_id);
        before != self.timers.len() || self.tcp.cancel(call_id)
    }

    #[cfg(test)]
    fn io_pending_count(&self) -> usize {
        self.tcp.pending_count()
    }
}

impl BetelgeuseDriver {
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

impl BetelgeuseTcp {
    fn with_io_loop(io_loop: IOLoopHandle<Global>) -> Self {
        Self {
            io_loop,
            next_listener_id: 1,
            next_stream_id: 1,
            next_file_id: 1,
            listeners: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            streams: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            files: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            pending: Vec::with_capacity(INITIAL_DRIVER_PENDING_CAPACITY),
        }
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
                            cancelled: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::SnapshotCommit {
                path,
                bytes,
                last_journal_index,
            } => Some(DriverCompletion {
                call_id,
                result: match crate::persistence::commit_snapshot(&path, bytes, last_journal_index)
                {
                    Ok(()) => CallOutput::SnapshotCommitted,
                    Err(reason) => CallOutput::Failed(reason),
                },
            }),
            CallInput::SnapshotLoad { path } => Some(DriverCompletion {
                call_id,
                result: match crate::persistence::load_snapshot(&path) {
                    Ok(snapshot) => CallOutput::SnapshotLoaded { snapshot },
                    Err(reason) => CallOutput::Failed(reason),
                },
            }),
            CallInput::JournalAppend {
                path,
                record_index,
                bytes,
            } => Some(DriverCompletion {
                call_id,
                result: match crate::persistence::append_journal_record(&path, record_index, bytes)
                {
                    Ok(()) => CallOutput::JournalAppended { record_index },
                    Err(reason) => CallOutput::Failed(reason),
                },
            }),
            CallInput::JournalReplay { path } => Some(DriverCompletion {
                call_id,
                result: match crate::persistence::replay_journal(&path) {
                    Ok(replay) => CallOutput::JournalReplayed { replay },
                    Err(reason) => CallOutput::Failed(reason),
                },
            }),
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
                            cancelled: false,
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
                            cancelled: false,
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
                            cancelled: false,
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
                            cancelled: false,
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
                            cancelled: false,
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
                            cancelled: false,
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
                            cancelled: false,
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
                            cancelled: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::Sleep { .. } => Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Unsupported),
            }),
        }
    }

    fn lane_has_active_pending(&self, lane: PendingLane) -> bool {
        self.pending
            .iter()
            .any(|op| op.lane == lane && !op.cancelled)
    }

    fn lane_has_pending(&self, lane: PendingLane) -> bool {
        self.pending.iter().any(|op| op.lane == lane)
    }

    fn stream_has_active_pending(&self, stream: StreamId) -> bool {
        self.lane_has_active_pending(PendingLane::StreamRead(stream))
            || self.lane_has_active_pending(PendingLane::StreamWrite(stream))
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
            if op.cancelled {
                if op.kind.has_result() {
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
        self.pending.iter().any(|op| !op.cancelled)
    }

    /// Cancels pending TCP operations during runtime shutdown.
    ///
    /// Tina emits requester-facing shutdown/cancel trace events from
    /// `Runtime`; TCP state only owns the substrate completion slots and
    /// resource handles.
    fn cancel_pending(&mut self) -> Result<(), DriverShutdownError> {
        for op in &mut self.pending {
            op.cancelled = true;
        }
        self.io_loop
            .cancel_pending_completions()
            .map_err(|_| DriverShutdownError::BackendStillOwnsCompletions)?;
        self.close_all_resources();
        self.drain_cancelled_pending_for_shutdown();
        if !self.pending.is_empty() || self.io_loop.pending_completion_count() != 0 {
            return Err(DriverShutdownError::BackendStillOwnsCompletions);
        }
        Ok(())
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let Some(index) = self
            .pending
            .iter()
            .position(|op| op.call_id == call_id && !op.cancelled)
        else {
            return false;
        };
        self.pending[index].cancelled = true;
        true
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn close_all_resources(&mut self) {
        for entry in std::mem::take(&mut self.listeners) {
            entry.socket.close();
        }
        for entry in std::mem::take(&mut self.streams) {
            entry.socket.close();
        }
        self.files.clear();
    }

    fn drain_cancelled_pending_for_shutdown(&mut self) {
        const SHUTDOWN_DRAIN_STEPS: usize = 64;

        for _ in 0..SHUTDOWN_DRAIN_STEPS {
            if self.pending.is_empty() {
                return;
            }

            let _ = self.io_loop.step();
            let mut index = 0;
            while index < self.pending.len() {
                if self.pending[index].kind.has_result() {
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
        if self.lane_has_active_pending(PendingLane::ListenerAccept(listener)) {
            return CallOutput::Failed(CallError::ResourceBusy);
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
        if self.stream_has_active_pending(stream) {
            return CallOutput::Failed(CallError::ResourceBusy);
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
            Self::FileRead(completion) => completion.has_result(),
            Self::FileWrite(completion) => completion.has_result(),
            Self::FileFsync(completion) => completion.has_result(),
            Self::FileSize(completion) => completion.has_result(),
            Self::Mkdir(completion) => completion.has_result(),
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
            .field("files", &self.files.len())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}
