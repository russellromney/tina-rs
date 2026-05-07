//! TLS lane and stream wrappers: rustls-backed TLS over Tina's runtime
//! TCP, plus the worker thread that performs handshake / read / write /
//! close on the substrate.

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
}

pub(super) enum TlsLane {
    Worker(TlsWorkerLane),
}

pub(super) struct TlsWorkerLane {
    pub(super) capacity: usize,
    pub(super) sender: Option<SyncSender<TlsCommand>>,
    pub(super) completions: Receiver<TlsCompletion>,
    pub(super) handle: Option<JoinHandle<()>>,
    pub(super) pending: Vec<TlsPending>,
    pub(super) listeners: Vec<TlsListenerEntry>,
    pub(super) streams: Vec<TlsStreamEntry>,
    pub(super) next_listener_id: u64,
    pub(super) next_stream_id: u64,
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

    pub(super) fn submit_connect(
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

    pub(super) fn submit_bind(
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

    pub(super) fn submit_bind(
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

    pub(super) fn submit_connect(
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

    pub(super) fn submit_close(
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

    pub(super) fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
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

    pub(super) fn drain_into_sink(&mut self, sink: &mut Vec<DriverCompletion>) {
        while let Ok(completion) = self.completions.try_recv() {
            self.finish_completion(completion, sink);
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

pub(super) fn tls_worker_loop(
    receiver: Receiver<TlsCommand>,
    completions: SyncSender<TlsCompletion>,
) {
    while let Ok(command) = receiver.recv() {
        let call_id = command.call_id();
        let result = execute_tls_command(command);
        if completions.send(TlsCompletion { call_id, result }).is_err() {
            break;
        }
    }
}

pub(super) fn execute_tls_command(command: TlsCommand) -> TlsCompletionResult {
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

pub(super) fn connect_tls(
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

pub(super) fn bind_tls_listener(
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
