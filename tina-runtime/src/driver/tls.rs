//! TLS lane and stream wrappers: rustls-backed TLS over Tina's runtime
//! TCP, plus bounded per-operation worker threads that perform handshake /
//! read / write / close on the substrate. `tls_lane_capacity` bounds
//! in-flight TLS work for the shard.

use super::*;

pub(super) type TlsClientStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;
pub(super) type TlsServerStream = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;

pub(super) enum TlsRuntimeStream {
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
    pub(super) fn tcp(&self) -> &TcpStream {
        match self {
            Self::Client(stream) => &stream.sock,
            Self::Server(stream) => &stream.sock,
        }
    }

    pub(super) fn send_close_notify(&mut self) {
        match self {
            Self::Client(stream) => stream.conn.send_close_notify(),
            Self::Server(stream) => stream.conn.send_close_notify(),
        }
    }

    /// The ALPN protocol negotiated during the handshake, as raw wire
    /// bytes, or `None` when no ALPN was negotiated.
    pub(super) fn alpn_protocol(&self) -> Option<Vec<u8>> {
        let raw = match self {
            Self::Client(stream) => stream.conn.alpn_protocol(),
            Self::Server(stream) => stream.conn.alpn_protocol(),
        };
        raw.map(|bytes| bytes.to_vec())
    }
}

pub(super) enum TlsLane {
    Worker(TlsWorkerLane),
}

pub(super) struct TlsWorkerLane {
    pub(super) capacity: usize,
    pub(super) completion_sender: Option<SyncSender<TlsCompletion>>,
    pub(super) completions: Receiver<TlsCompletion>,
    pub(super) handles: Vec<JoinHandle<()>>,
    pub(super) pending: Vec<TlsPending>,
    pub(super) listeners: Vec<TlsListenerEntry>,
    pub(super) streams: Vec<TlsStreamEntry>,
    pub(super) next_listener_id: u64,
    pub(super) next_stream_id: u64,
    /// Call ids cancelled by close-wins. `advance()` drains into
    /// synthetic `Failed(TargetClosed)` completions; the worker's
    /// real completion is dropped because the pending entry is gone.
    pub(super) cancelled_by_close: Vec<CallId>,
}

pub(super) struct TlsListenerEntry {
    id: TlsListenerId,
    listener: Arc<TcpListener>,
    config: Arc<rustls::ServerConfig>,
}

pub(super) struct TlsStreamEntry {
    id: TlsStreamId,
    stream: Arc<Mutex<TlsRuntimeStream>>,
}

pub(super) struct TlsPending {
    pub(super) call_id: CallId,
    pub(super) lane: TlsPendingLane,
    pub(super) deadline: Instant,
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) timed_out: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TlsPendingLane {
    Connect(CallId),
    ListenerAccept(TlsListenerId),
    Stream(TlsStreamId),
}

pub(super) enum TlsCommand {
    Bind {
        call_id: CallId,
        addr: SocketAddr,
        certificate_chain: Vec<Vec<u8>>,
        private_key: Vec<u8>,
        alpn_protocols: Vec<Vec<u8>>,
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
        alpn_protocols: Vec<Vec<u8>>,
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

pub(super) struct TlsCompletion {
    pub(super) call_id: CallId,
    pub(super) result: TlsCompletionResult,
}

pub(super) enum TlsCompletionResult {
    Bound(Box<Result<(TcpListener, rustls::ServerConfig), CallError>>),
    Accepted(Box<Result<(TlsRuntimeStream, SocketAddr), CallError>>),
    Connected(Box<Result<TlsRuntimeStream, CallError>>),
    Output(CallOutput),
}

impl TlsLane {
    pub(super) fn new(capacity: usize) -> Self {
        Self::Worker(TlsWorkerLane::new(capacity))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn submit_connect(
        &mut self,
        call_id: CallId,
        addr: SocketAddr,
        server_name: String,
        root_certificates: Vec<Vec<u8>>,
        alpn_protocols: Vec<Vec<u8>>,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => lane.submit_connect(
                call_id,
                addr,
                server_name,
                root_certificates,
                alpn_protocols,
                timeout,
                now,
            ),
        }
    }

    pub(super) fn submit_bind(
        &mut self,
        call_id: CallId,
        addr: SocketAddr,
        certificate_chain: Vec<Vec<u8>>,
        private_key: Vec<u8>,
        alpn_protocols: Vec<Vec<u8>>,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => lane.submit_bind(
                call_id,
                addr,
                certificate_chain,
                private_key,
                alpn_protocols,
                now,
            ),
        }
    }

    pub(super) fn submit_accept(
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

    pub(super) fn submit_listener_close(
        &mut self,
        call_id: CallId,
        listener: TlsListenerId,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => lane.submit_listener_close(call_id, listener),
        }
    }

    pub(super) fn submit_read(
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

    pub(super) fn submit_write(
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

    pub(super) fn submit_close(
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

    pub(super) fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        match self {
            Self::Worker(lane) => lane.advance(now, completed),
        }
    }

    pub(super) fn has_pending(&self) -> bool {
        match self {
            Self::Worker(lane) => lane.has_pending(),
        }
    }

    pub(super) fn cancel(&mut self, call_id: CallId) -> bool {
        match self {
            Self::Worker(lane) => lane.cancel(call_id),
        }
    }

    pub(super) fn cancel_pending(&mut self, deadline: Instant) {
        match self {
            Self::Worker(lane) => lane.cancel_pending(deadline),
        }
    }

    pub(super) fn resource_report(&self) -> DriverResourceReport {
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
impl TlsWorkerLane {
    pub(super) fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "TLS lane capacity must be > 0");
        let (completion_sender, completions) = sync_channel(capacity.saturating_add(1));
        Self {
            capacity,
            completion_sender: Some(completion_sender),
            completions,
            handles: Vec::with_capacity(capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
            pending: Vec::with_capacity(capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
            listeners: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            streams: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            next_listener_id: 1,
            next_stream_id: 1,
            cancelled_by_close: Vec::new(),
        }
    }

    pub(super) fn submit_bind(
        &mut self,
        call_id: CallId,
        addr: SocketAddr,
        certificate_chain: Vec<Vec<u8>>,
        private_key: Vec<u8>,
        alpn_protocols: Vec<Vec<u8>>,
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
                alpn_protocols,
                cancelled,
            },
        )
    }

    pub(super) fn submit_accept(
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

    pub(super) fn submit_listener_close(
        &mut self,
        call_id: CallId,
        listener: TlsListenerId,
    ) -> Option<DriverCompletion> {
        let Some(index) = self.listeners.iter().position(|entry| entry.id == listener) else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::InvalidResource),
            });
        };
        // Close wins: cancel any pending accept on this listener.
        // Mirrors `do_listener_close` on the TCP lane.
        for pending in self.pending.iter_mut() {
            if matches!(pending.lane, TlsPendingLane::ListenerAccept(l) if l == listener)
                && !pending.cancelled.load(Ordering::Acquire)
            {
                pending.cancelled.store(true, Ordering::Release);
                pending.timed_out = true;
                self.cancelled_by_close.push(pending.call_id);
            }
        }
        self.listeners.remove(index);
        Some(DriverCompletion {
            call_id,
            result: CallOutput::TlsListenerClosed,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn submit_connect(
        &mut self,
        call_id: CallId,
        addr: SocketAddr,
        server_name: String,
        root_certificates: Vec<Vec<u8>>,
        alpn_protocols: Vec<Vec<u8>>,
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
                alpn_protocols,
                timeout,
                cancelled: Arc::new(AtomicBool::new(false)),
            },
        )
    }

    pub(super) fn submit_close(
        &mut self,
        call_id: CallId,
        stream_id: TlsStreamId,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let lane = TlsPendingLane::Stream(stream_id);
        let Some(stream_arc) = self.stream(stream_id) else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::InvalidResource),
            });
        };
        // Close wins: cancel any pending read/write on this stream.
        for pending in self.pending.iter_mut() {
            if pending.lane == lane && !pending.cancelled.load(Ordering::Acquire) {
                pending.cancelled.store(true, Ordering::Release);
                pending.timed_out = true;
                self.cancelled_by_close.push(pending.call_id);
            }
        }
        // Free the lane slot now. The worker still flushes through
        // its cloned `Arc<Mutex<TlsRuntimeStream>>`; further
        // read/write/close submissions on this id see
        // `InvalidResource`.
        self.streams.retain(|entry| entry.id != stream_id);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.submit_command(
            call_id,
            lane,
            Arc::clone(&cancelled),
            now,
            TlsCommand::Close {
                call_id,
                stream: stream_arc,
                timeout,
                cancelled,
            },
        )
    }

    pub(super) fn submit_read(
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

    pub(super) fn submit_write(
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

    pub(super) fn submit_command(
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
        let Some(completion_sender) = &self.completion_sender else {
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
        let completion_sender = completion_sender.clone();
        let spawn = thread::Builder::new()
            .name(format!("tina-tls-{call_id:?}"))
            .spawn(move || {
                let call_id = command.call_id();
                let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute_tls_command(command)
                })) {
                    Ok(result) => result,
                    Err(_) => TlsCompletionResult::Output(CallOutput::Failed(CallError::Io)),
                };
                let _ = completion_sender.send(TlsCompletion { call_id, result });
            });
        match spawn {
            Ok(handle) => {
                self.pending.push(TlsPending {
                    call_id,
                    lane,
                    deadline: now + timeout,
                    cancelled,
                    timed_out: false,
                });
                self.handles.push(handle);
                None
            }
            Err(_) => Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Io),
            }),
        }
    }

    pub(super) fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        // Drain close-wins cancellations into synthetic completions.
        while let Some(call_id) = self.cancelled_by_close.pop() {
            if let Some(idx) = self.pending.iter().position(|p| p.call_id == call_id) {
                self.pending.remove(idx);
            }
            completed.push(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::TargetClosed),
            });
        }

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
                    self.completion_sender = None;
                    break;
                }
            }
        }
        self.reap_finished_workers();
    }

    pub(super) fn finish_completion(
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
                    let selected_alpn = stream.alpn_protocol();
                    let id = TlsStreamId::new(self.next_stream_id);
                    self.next_stream_id += 1;
                    self.streams.push(TlsStreamEntry {
                        id,
                        stream: Arc::new(Mutex::new(stream)),
                    });
                    CallOutput::TlsAccepted {
                        stream: id,
                        peer_addr,
                        selected_alpn,
                    }
                }
                Err(error) => CallOutput::Failed(error),
            },
            TlsCompletionResult::Connected(result) => match *result {
                Ok(stream) => {
                    let selected_alpn = stream.alpn_protocol();
                    let id = TlsStreamId::new(self.next_stream_id);
                    self.next_stream_id += 1;
                    self.streams.push(TlsStreamEntry {
                        id,
                        stream: Arc::new(Mutex::new(stream)),
                    });
                    CallOutput::TlsConnected {
                        stream: id,
                        selected_alpn,
                    }
                }
                Err(error) => CallOutput::Failed(error),
            },
            // Streams are removed at `submit_close` time, not here.
            TlsCompletionResult::Output(output) => output,
        };
        completed.push(DriverCompletion {
            call_id: completion.call_id,
            result,
        });
    }

    pub(super) fn has_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|entry| !entry.cancelled.load(Ordering::Acquire) && !entry.timed_out)
    }

    pub(super) fn cancel(&mut self, call_id: CallId) -> bool {
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

    pub(super) fn cancel_pending(&mut self, deadline: Instant) {
        // Stop accepting new TLS work. Existing worker threads own
        // cloned resources until they complete or the shutdown budget
        // expires; remaining `self.pending` after the budget is stuck
        // work and is surfaced through `resource_report`.
        self.completion_sender = None;
        let mut sink = Vec::new();
        loop {
            self.drain_into_sink(&mut sink);
            self.reap_finished_workers();
            if self.pending.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        sink.clear();
        if self.pending.is_empty() {
            self.listeners.clear();
            self.streams.clear();
        }
    }

    pub(super) fn drain_into_sink(&mut self, sink: &mut Vec<DriverCompletion>) {
        while let Ok(completion) = self.completions.try_recv() {
            self.finish_completion(completion, sink);
        }
        self.reap_finished_workers();
    }

    pub(super) fn reap_finished_workers(&mut self) {
        let mut index = 0;
        while index < self.handles.len() {
            if self.handles[index].is_finished() {
                let handle = self.handles.swap_remove(index);
                let _ = handle.join();
            } else {
                index += 1;
            }
        }
    }

    pub(super) fn stream(&self, stream: TlsStreamId) -> Option<Arc<Mutex<TlsRuntimeStream>>> {
        self.streams
            .iter()
            .find(|entry| entry.id == stream)
            .map(|entry| Arc::clone(&entry.stream))
    }

    pub(super) fn lane_has_pending(&self, lane: TlsPendingLane) -> bool {
        self.pending
            .iter()
            .any(|entry| entry.lane == lane && !entry.cancelled.load(Ordering::Acquire))
    }

    pub(super) fn listener(
        &self,
        listener: TlsListenerId,
    ) -> Option<(Arc<TcpListener>, Arc<rustls::ServerConfig>)> {
        self.listeners
            .iter()
            .find(|entry| entry.id == listener)
            .map(|entry| (Arc::clone(&entry.listener), Arc::clone(&entry.config)))
    }

    pub(super) fn resource_report(&self) -> DriverResourceReport {
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

pub(super) fn execute_tls_command(command: TlsCommand) -> TlsCompletionResult {
    match command {
        TlsCommand::Bind {
            addr,
            certificate_chain,
            private_key,
            alpn_protocols,
            cancelled,
            ..
        } => TlsCompletionResult::Bound(Box::new(bind_tls_listener(
            addr,
            certificate_chain,
            private_key,
            alpn_protocols,
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
            alpn_protocols,
            timeout,
            cancelled,
            ..
        } => TlsCompletionResult::Connected(Box::new(connect_tls(
            addr,
            &server_name,
            root_certificates,
            alpn_protocols,
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

pub(super) fn connect_tls(
    addr: SocketAddr,
    server_name: &str,
    root_certificates: Vec<Vec<u8>>,
    alpn_protocols: Vec<Vec<u8>>,
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
    // `set_*_timeout` only fails for `Some(zero)`/`None`; never here.
    let _ = tcp.set_read_timeout(Some(timeout));
    let _ = tcp.set_write_timeout(Some(timeout));

    let mut roots = rustls::RootCertStore::empty();
    for certificate in root_certificates {
        roots
            .add(rustls::pki_types::CertificateDer::from(certificate))
            .map_err(|_| CallError::TlsCertificate)?;
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let alpn_offered = !alpn_protocols.is_empty();
    config.alpn_protocols = alpn_protocols;
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
    // If we offered ALPN but the peer negotiated none, fail closed with a
    // typed mismatch rather than silently proceeding on a protocol the
    // caller did not ask for.
    if alpn_offered && stream.conn.alpn_protocol().is_none() {
        return Err(CallError::TlsAlpnMismatch);
    }
    Ok(TlsRuntimeStream::Client(stream))
}

pub(super) fn bind_tls_listener(
    addr: SocketAddr,
    certificate_chain: Vec<Vec<u8>>,
    private_key: Vec<u8>,
    alpn_protocols: Vec<Vec<u8>>,
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
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(|_| CallError::TlsCertificate)?;
    config.alpn_protocols = alpn_protocols;
    let listener = TcpListener::bind(addr).map_err(|_| CallError::Io)?;
    Ok((listener, config))
}

pub(super) fn accept_tls(
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
    // Idle accept poll. `thread::yield_now()` would burn CPU on a
    // bound-but-quiet listener; sleep a millisecond between polls
    // so an HTTPS listener that nobody is calling stays cheap.
    // 1ms is well below the typical accept-deadline grain
    // (250ms in dev, 100ms in pressure) and short enough that
    // accept latency stays imperceptible.
    let poll_interval = Duration::from_millis(1);
    let (tcp, peer_addr) = loop {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(CallError::Timeout);
        }
        match listener.accept() {
            Ok(accepted) => break accepted,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(poll_interval);
            }
            Err(_) => return Err(CallError::Io),
        }
    };
    tcp.set_nonblocking(false).map_err(|_| CallError::Io)?;
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(CallError::Timeout)?;
    // Infallible; see `connect_tls`.
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

pub(super) fn tls_handshake_error(error: std::io::Error) -> CallError {
    if matches!(
        error.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    ) {
        CallError::Timeout
    } else {
        CallError::TlsHandshake
    }
}

pub(super) fn read_tls(
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

pub(super) fn write_tls(
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

pub(super) fn close_tls(
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

impl TlsCommand {
    pub(super) fn call_id(&self) -> CallId {
        match self {
            Self::Bind { call_id, .. }
            | Self::Accept { call_id, .. }
            | Self::Connect { call_id, .. }
            | Self::Read { call_id, .. }
            | Self::Write { call_id, .. }
            | Self::Close { call_id, .. } => *call_id,
        }
    }

    pub(super) fn timeout(&self) -> Duration {
        match self {
            Self::Bind { .. } => Duration::from_secs(86_400),
            Self::Accept { timeout, .. }
            | Self::Connect { timeout, .. }
            | Self::Read { timeout, .. }
            | Self::Write { timeout, .. }
            | Self::Close { timeout, .. } => *timeout,
        }
    }

    pub(super) fn set_cancelled(&mut self, cancelled: Arc<AtomicBool>) {
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
