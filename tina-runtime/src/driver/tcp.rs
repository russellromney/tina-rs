//! Betelgeuse TCP/UDP/file lane: real socket and file state owned by
//! the runtime, presented to isolates only via opaque ListenerId /
//! StreamId / UdpSocketId / FileId values.

use super::*;

/// Runtime-owned Betelgeuse TCP state.
///
/// Owns all real socket state. Isolate code only ever sees the runtime's
/// opaque [`ListenerId`] / [`StreamId`] values.
pub(super) struct BetelgeuseTcp {
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

impl BetelgeuseTcp {
    pub(super) fn with_io_loop(io_loop: IOLoopHandle<Global>) -> Self {
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
    pub(super) fn take_cancelled_by_close(&mut self) -> Vec<CallId> {
        std::mem::take(&mut self.cancelled_by_close)
    }

    /// Submits one runtime-owned call. Synchronous Betelgeuse ops (bind,
    /// close) finish here and the result is returned inline; async ops
    /// (accept, recv, send) push a pending entry and return [`None`].
    pub(super) fn submit(
        &mut self,
        call_id: CallId,
        request: CallInput,
    ) -> Option<DriverCompletion> {
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
            // Phase 117 local-IPC parity: Unix-domain rails ship a typed
            // `Unsupported` from the live driver. The simulator implements
            // the full byte-stream semantics; live OS Unix sockets are an
            // honest deferral. Non-Unix platforms see the same typed answer
            // — no cfg-silent omission.
            CallInput::UnixBind { .. }
            | CallInput::UnixAccept { .. }
            | CallInput::UnixConnect { .. }
            | CallInput::UnixRead { .. }
            | CallInput::UnixWrite { .. }
            | CallInput::UnixListenerClose { .. }
            | CallInput::UnixStreamClose { .. } => Some(DriverCompletion {
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

    pub(super) fn file_has_active_pending(&self, file: FileId) -> bool {
        self.lane_has_active_pending(PendingLane::FileRead(file))
            || self.lane_has_active_pending(PendingLane::FileWrite(file))
            || self.lane_has_active_pending(PendingLane::FileFsync(file))
            || self.lane_has_active_pending(PendingLane::FileSize(file))
    }

    /// Advances Betelgeuse by one tick and harvests any pending operations
    /// whose completion slots have a result available. Returned in
    /// submission order.
    pub(super) fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
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
    pub(super) fn has_pending(&self) -> bool {
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
    pub(super) fn cancel_pending(&mut self, deadline: Instant) -> Result<(), DriverShutdownError> {
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

    pub(super) fn cancel(&mut self, call_id: CallId) -> bool {
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
    pub(super) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn resource_report(&self) -> DriverResourceReport {
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

    pub(super) fn close_all_resources(&mut self) {
        for entry in std::mem::take(&mut self.listeners) {
            entry.socket.close();
        }
        for entry in std::mem::take(&mut self.streams) {
            entry.socket.close();
        }
        self.udp_sockets.clear();
        self.files.clear();
    }

    pub(super) fn drain_cancelled_pending_for_shutdown(&mut self, deadline: Instant) {
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

    pub(super) fn do_bind(&mut self, addr: SocketAddr) -> CallOutput {
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

    pub(super) fn do_listener_close(&mut self, listener: ListenerId) -> CallOutput {
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

    pub(super) fn do_stream_close(&mut self, stream: StreamId) -> CallOutput {
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

    pub(super) fn do_udp_bind(&mut self, addr: SocketAddr) -> CallOutput {
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

    pub(super) fn do_udp_send_to(
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

    pub(super) fn do_udp_close(&mut self, socket: UdpSocketId) -> CallOutput {
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

    pub(super) fn do_file_open(
        &mut self,
        path: &std::path::Path,
        options: FileOpenOptions,
    ) -> CallOutput {
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

    pub(super) fn do_file_close(&mut self, file: FileId) -> CallOutput {
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
