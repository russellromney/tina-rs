//! TLS over Tina's Betelgeuse TCP rail, with rustls in sans-I/O mode.
//!
//! The runtime owns a rustls [`rustls::Connection`] per [`TlsStreamId`] and
//! drives it on the shard thread as Betelgeuse harvests TCP completions — no
//! worker thread, no second socket stack, server and client can share one
//! runtime. Each TLS op owns its underlying Betelgeuse socket exclusively and
//! serializes its own TCP ops: at most one `recv` *or* `send` in flight at a
//! time (Hard Constraint 6). A single `tls_*` call may issue an interleaved
//! *sequence* of recv/send, never two at once — that is what keeps the
//! one-pending-op rule from re-creating the old handshake deadlock.
//!
//! `tls_lane_capacity` bounds the shard-total count of in-flight TLS ops; a
//! submission past it fails with [`CallError::TlsFull`].

use super::*;

/// Max ciphertext bytes pulled from the socket in one pump turn. Mirrors a
/// `tcp_read` `max_len` so one pump turn cannot process unbounded records and
/// starve the shard. One TLS record is at most ~16 KiB + overhead.
const TLS_CIPHERTEXT_CHUNK: usize = 16 * 1024;

/// Bound on rustls' internal plaintext buffer. Keeps `tls_write` from growing
/// unbounded against a slow peer: the op drives encrypt+flush incrementally and
/// completes only once all ciphertext is on the wire. A peer that never drains
/// surfaces as the whole-op timeout, not unbounded memory.
const TLS_PLAINTEXT_BUFFER_LIMIT: usize = 64 * 1024;

/// The on-shard TLS lane. Holds a clone of the runtime's Betelgeuse loop and
/// owns every TLS socket exclusively (the isolate only ever sees the opaque
/// [`TlsStreamId`] / [`TlsListenerId`]).
pub(super) struct TlsLane {
    io_loop: IOLoopHandle<Global>,
    /// Shard-total cap on in-flight TLS ops (`tls_lane_capacity`).
    capacity: usize,
    next_listener_id: u64,
    next_stream_id: u64,
    listeners: Vec<TlsListenerEntry>,
    /// Established streams between ops. Each owns its rustls connection,
    /// socket, ciphertext buffers, and at most one in-flight Betelgeuse op.
    streams: Vec<TlsStreamEntry>,
    pending: Vec<TlsPending>,
    /// Call ids cancelled by close-wins. `advance` drains them into synthetic
    /// `Failed(TargetClosed)` completions exactly once; the op then tombstones
    /// until its backend completion slot (if any) is released.
    cancelled_by_close: Vec<CallId>,
}

struct TlsListenerEntry {
    id: TlsListenerId,
    socket: Box<dyn IOSocket>,
    config: Arc<rustls::ServerConfig>,
}

struct TlsStreamEntry {
    id: TlsStreamId,
    io: TlsIo,
}

/// One TLS connection's byte machine: the rustls connection, its owned socket,
/// the bounded inbound/outbound ciphertext buffers, and the single in-flight
/// Betelgeuse op (Hard Constraint 6: never two at once).
struct TlsIo {
    conn: rustls::Connection,
    socket: Box<dyn IOSocket>,
    /// Ciphertext recv'd from the socket but not yet accepted by `read_tls`.
    inbound: Vec<u8>,
    /// Ciphertext drained from rustls awaiting `send`. Front is consumed as
    /// sends complete.
    outbound: Vec<u8>,
    socket_op: SocketOp,
    /// The socket reported clean TCP EOF (recv returned 0 bytes).
    read_eof: bool,
    /// The TCP EOF has been signalled into rustls (`read_tls` of an empty
    /// reader sets `has_seen_eof`). Signalled at most once.
    eof_signaled: bool,
}

/// The single Betelgeuse op a [`TlsIo`] may have in flight.
enum SocketOp {
    Idle,
    Connect(Box<ConnectCompletion>),
    Recv(Box<RecvCompletion>),
    Send(Box<SendCompletion>),
}

impl SocketOp {
    /// True while the backend still holds a pointer to this op's completion
    /// slot — the box must not be dropped until the backend releases it.
    fn live(&self) -> bool {
        match self {
            Self::Idle => false,
            Self::Connect(c) => !c.has_result(),
            Self::Recv(c) => !c.has_result(),
            Self::Send(c) => !c.has_result(),
        }
    }
}

/// Which isolate-facing lane a pending op occupies. Mirrors the TCP rail:
/// duplicate work on one stream/listener lane is rejected with `ResourceBusy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsLaneKey {
    Connect(CallId),
    Accept(TlsListenerId),
    Stream(TlsStreamId),
}

struct TlsPending {
    call_id: CallId,
    lane: TlsLaneKey,
    /// Whole-op deadline, computed once at submit; every internal TCP op runs
    /// against this single deadline (Hole 3 #1: timeout spans the whole op).
    deadline: Instant,
    /// Set by user-cancel, timeout, or close-wins. A cancelled op never pumps
    /// again and never delivers a real result; it lingers only to keep a live
    /// backend completion slot alive (tombstone).
    cancelled: bool,
    state: TlsOpState,
}

/// The per-op state machine. Connect/Accept/Close own their [`TlsIo`] in the
/// pending entry; Read/Write drive the [`TlsIo`] living in `streams`.
enum TlsOpState {
    Connect {
        io: TlsIo,
        alpn_offered: bool,
    },
    Accept {
        config: Arc<rustls::ServerConfig>,
        accept: Box<AcceptCompletion>,
        io: Option<TlsIo>,
        peer_addr: Option<SocketAddr>,
    },
    Read {
        stream: TlsStreamId,
        max_len: usize,
    },
    ReadBuf {
        stream: TlsStreamId,
        buffer: Vec<u8>,
        max_len: usize,
    },
    Write {
        stream: TlsStreamId,
        plaintext: Vec<u8>,
        fed: usize,
    },
    WriteOwned {
        stream: TlsStreamId,
        plaintext: Vec<u8>,
        fed: usize,
    },
    Close {
        io: TlsIo,
        sent_close_notify: bool,
    },
}

/// One step of pumping a [`TlsIo`] toward a goal.
enum PumpStep {
    /// A socket op is in flight (or more data is needed): call again next turn.
    Pending,
    /// The goal is met; carry the typed output back to the op handler.
    Done,
    /// The op failed with this typed error.
    Failed(CallError),
}

impl TlsIo {
    fn new(conn: rustls::Connection, socket: Box<dyn IOSocket>) -> Self {
        let mut io = Self {
            conn,
            socket,
            inbound: Vec::new(),
            outbound: Vec::new(),
            socket_op: SocketOp::Idle,
            read_eof: false,
            eof_signaled: false,
        };
        io.conn.set_buffer_limit(Some(TLS_PLAINTEXT_BUFFER_LIMIT));
        io
    }

    /// Harvest the single in-flight Betelgeuse op if it has a result. Returns
    /// `Ok(true)` when an op completed (state advanced), `Ok(false)` when the
    /// op is still in flight, `Err` on a socket error.
    fn harvest(&mut self) -> Result<bool, CallError> {
        match &mut self.socket_op {
            SocketOp::Idle => Ok(false),
            SocketOp::Connect(c) => {
                if !c.has_result() {
                    return Ok(false);
                }
                let result = c
                    .take_result()
                    .expect("connect completion advertised a result");
                self.socket_op = SocketOp::Idle;
                result.map(|()| true).map_err(|_| CallError::Io)
            }
            SocketOp::Recv(c) => {
                if !c.has_result() {
                    return Ok(false);
                }
                let result = c
                    .take_result()
                    .expect("recv completion advertised a result");
                self.socket_op = SocketOp::Idle;
                match result {
                    Ok(bytes) if bytes.is_empty() => {
                        self.read_eof = true;
                        Ok(true)
                    }
                    Ok(bytes) => {
                        self.inbound.extend_from_slice(&bytes);
                        Ok(true)
                    }
                    Err(_) => Err(CallError::Io),
                }
            }
            SocketOp::Send(c) => {
                if !c.has_result() {
                    return Ok(false);
                }
                let result = c
                    .take_result()
                    .expect("send completion advertised a result");
                self.socket_op = SocketOp::Idle;
                match result {
                    Ok(count) => {
                        let count = count.min(self.outbound.len());
                        self.outbound.drain(..count);
                        Ok(true)
                    }
                    Err(_) => Err(CallError::Io),
                }
            }
        }
    }

    /// Feed buffered inbound ciphertext into rustls and process records. Marks
    /// rustls' `has_seen_eof` exactly once when the socket has hit TCP EOF so
    /// `reader()` can distinguish a clean `close_notify` from truncation.
    fn ingest(&mut self) -> Result<(), CallError> {
        loop {
            if self.conn.wants_read() && !self.inbound.is_empty() {
                let mut src: &[u8] = &self.inbound;
                let read = match self.conn.read_tls(&mut src) {
                    // rustls' input buffer is full: drain plaintext first.
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => return Err(CallError::Io),
                };
                self.inbound.drain(..read);
            } else if self.conn.wants_read() && self.read_eof && !self.eof_signaled {
                let _ = self.conn.read_tls(&mut std::io::empty());
                self.eof_signaled = true;
            } else {
                break;
            }
            let was_handshaking = self.conn.is_handshaking();
            if self.conn.process_new_packets().is_err() {
                return Err(if was_handshaking {
                    CallError::TlsHandshake
                } else {
                    CallError::Io
                });
            }
        }
        Ok(())
    }

    /// Pull all pending TLS output (handshake messages, alerts, encrypted
    /// records, `close_notify`) into the outbound ciphertext buffer.
    fn egress(&mut self) {
        while self.conn.wants_write() {
            if self.conn.write_tls(&mut self.outbound).is_err() {
                break;
            }
        }
    }

    /// Arm one `send` for the front of the outbound buffer. Caller guarantees
    /// `socket_op` is Idle and outbound is non-empty (Hard Constraint 6).
    fn arm_send(&mut self) -> Result<(), CallError> {
        let end = self.outbound.len().min(TLS_CIPHERTEXT_CHUNK);
        let chunk = self.outbound[..end].to_vec();
        let mut completion = Box::new(SendCompletion::new());
        if self.socket.send(&mut completion, chunk).is_err() {
            return Err(CallError::Io);
        }
        self.socket_op = SocketOp::Send(completion);
        Ok(())
    }

    /// Arm one bounded `recv`. Caller guarantees `socket_op` is Idle.
    fn arm_recv(&mut self) -> Result<(), CallError> {
        let mut completion = Box::new(RecvCompletion::new());
        if self
            .socket
            .recv(&mut completion, TLS_CIPHERTEXT_CHUNK)
            .is_err()
        {
            return Err(CallError::Io);
        }
        self.socket_op = SocketOp::Recv(completion);
        Ok(())
    }

    /// Flush outbound ciphertext (one send) then, if nothing is queued and the
    /// connection still wants input, arm one recv. Returns whether a socket op
    /// is now in flight.
    fn flush_or_fill(&mut self, want_more_input: bool) -> Result<bool, CallError> {
        if !matches!(self.socket_op, SocketOp::Idle) {
            return Ok(true);
        }
        if !self.outbound.is_empty() {
            self.arm_send()?;
            return Ok(true);
        }
        if want_more_input && !self.read_eof {
            self.arm_recv()?;
            return Ok(true);
        }
        Ok(false)
    }
}

impl TlsLane {
    pub(super) fn new(capacity: usize, io_loop: IOLoopHandle<Global>) -> Self {
        assert!(capacity > 0, "TLS lane capacity must be > 0");
        Self {
            io_loop,
            capacity,
            next_listener_id: 1,
            next_stream_id: 1,
            listeners: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            streams: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
            pending: Vec::with_capacity(INITIAL_DRIVER_PENDING_CAPACITY),
            cancelled_by_close: Vec::new(),
        }
    }

    // --- submission ------------------------------------------------------

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
        if let Some(failure) = self.reject_if_full_or_zero(call_id, timeout) {
            return Some(failure);
        }
        let alpn_offered = !alpn_protocols.is_empty();
        let conn = match build_client_connection(&server_name, root_certificates, alpn_protocols) {
            Ok(conn) => conn,
            Err(error) => {
                return Some(DriverCompletion {
                    call_id,
                    result: CallOutput::Failed(error),
                });
            }
        };
        let socket = match self.io_loop.socket() {
            Ok(socket) => socket,
            Err(_) => {
                return Some(DriverCompletion {
                    call_id,
                    result: CallOutput::Failed(CallError::Io),
                });
            }
        };
        let mut completion = Box::new(ConnectCompletion::new());
        if socket.connect(&mut completion, addr).is_err() {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Io),
            });
        }
        let mut io = TlsIo::new(rustls::Connection::Client(conn), socket);
        io.socket_op = SocketOp::Connect(completion);
        self.pending.push(TlsPending {
            call_id,
            lane: TlsLaneKey::Connect(call_id),
            deadline: deadline_after(now, timeout),
            cancelled: false,
            state: TlsOpState::Connect { io, alpn_offered },
        });
        None
    }

    pub(super) fn submit_bind(
        &mut self,
        call_id: CallId,
        addr: SocketAddr,
        certificate_chain: Vec<Vec<u8>>,
        private_key: Vec<u8>,
        alpn_protocols: Vec<Vec<u8>>,
        _now: Instant,
    ) -> Option<DriverCompletion> {
        let config = match build_server_config(certificate_chain, private_key, alpn_protocols) {
            Ok(config) => Arc::new(config),
            Err(error) => {
                return Some(DriverCompletion {
                    call_id,
                    result: CallOutput::Failed(error),
                });
            }
        };
        let socket = match self.io_loop.socket() {
            Ok(socket) => socket,
            Err(_) => {
                return Some(DriverCompletion {
                    call_id,
                    result: CallOutput::Failed(CallError::Io),
                });
            }
        };
        if socket.bind(addr).is_err() {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Io),
            });
        }
        let local_addr = match socket.local_addr() {
            Ok(addr) => addr,
            Err(_) => {
                return Some(DriverCompletion {
                    call_id,
                    result: CallOutput::Failed(CallError::Io),
                });
            }
        };
        let id = TlsListenerId::new(self.next_listener_id);
        self.next_listener_id += 1;
        self.listeners.push(TlsListenerEntry { id, socket, config });
        Some(DriverCompletion {
            call_id,
            result: CallOutput::TlsBound {
                listener: id,
                local_addr,
            },
        })
    }

    pub(super) fn submit_accept(
        &mut self,
        call_id: CallId,
        listener: TlsListenerId,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let lane = TlsLaneKey::Accept(listener);
        if self.lane_has_active(lane) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::ResourceBusy),
            });
        }
        if let Some(failure) = self.reject_if_full_or_zero(call_id, timeout) {
            return Some(failure);
        }
        let Some(entry) = self.listeners.iter().find(|entry| entry.id == listener) else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::InvalidResource),
            });
        };
        let config = Arc::clone(&entry.config);
        let mut accept = Box::new(AcceptCompletion::new());
        if entry.socket.accept(&mut accept).is_err() {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Io),
            });
        }
        self.pending.push(TlsPending {
            call_id,
            lane,
            deadline: deadline_after(now, timeout),
            cancelled: false,
            state: TlsOpState::Accept {
                config,
                accept,
                io: None,
                peer_addr: None,
            },
        });
        None
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
        // Close wins: cancel any pending accept on this listener. The accept's
        // backend completion slot is kept alive (tombstone) until released.
        for pending in self.pending.iter_mut() {
            if pending.lane == TlsLaneKey::Accept(listener) && !pending.cancelled {
                pending.cancelled = true;
                self.cancelled_by_close.push(pending.call_id);
            }
        }
        let entry = self.listeners.remove(index);
        entry.socket.close();
        Some(DriverCompletion {
            call_id,
            result: CallOutput::TlsListenerClosed,
        })
    }

    pub(super) fn submit_read(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        max_len: usize,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let lane = TlsLaneKey::Stream(stream);
        if self.lane_has_active(lane) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::ResourceBusy),
            });
        }
        if !self.streams.iter().any(|entry| entry.id == stream) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::InvalidResource),
            });
        }
        if let Some(failure) = self.reject_if_full_or_zero(call_id, timeout) {
            return Some(failure);
        }
        self.pending.push(TlsPending {
            call_id,
            lane,
            deadline: deadline_after(now, timeout),
            cancelled: false,
            state: TlsOpState::Read { stream, max_len },
        });
        None
    }

    pub(super) fn submit_read_buf(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        buffer: Vec<u8>,
        max_len: usize,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let lane = TlsLaneKey::Stream(stream);
        if self.lane_has_active(lane) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::TlsReadBufFailed {
                    buffer,
                    error: CallError::ResourceBusy,
                },
            });
        }
        if !self.streams.iter().any(|entry| entry.id == stream) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::TlsReadBufFailed {
                    buffer,
                    error: CallError::InvalidResource,
                },
            });
        }
        if let Some(failure) = self.reject_if_full_or_zero(call_id, timeout) {
            let error = match failure.result {
                CallOutput::Failed(error) => error,
                other => {
                    return Some(DriverCompletion {
                        call_id,
                        result: other,
                    });
                }
            };
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::TlsReadBufFailed { buffer, error },
            });
        }
        self.pending.push(TlsPending {
            call_id,
            lane,
            deadline: deadline_after(now, timeout),
            cancelled: false,
            state: TlsOpState::ReadBuf {
                stream,
                buffer,
                max_len,
            },
        });
        None
    }

    pub(super) fn submit_write(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        bytes: Vec<u8>,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let lane = TlsLaneKey::Stream(stream);
        if self.lane_has_active(lane) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::ResourceBusy),
            });
        }
        if !self.streams.iter().any(|entry| entry.id == stream) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::InvalidResource),
            });
        }
        if let Some(failure) = self.reject_if_full_or_zero(call_id, timeout) {
            return Some(failure);
        }
        self.pending.push(TlsPending {
            call_id,
            lane,
            deadline: deadline_after(now, timeout),
            cancelled: false,
            state: TlsOpState::Write {
                stream,
                plaintext: bytes,
                fed: 0,
            },
        });
        None
    }

    pub(super) fn submit_write_owned(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        bytes: Vec<u8>,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let lane = TlsLaneKey::Stream(stream);
        if self.lane_has_active(lane) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::TlsWroteOwnedFailed {
                    bytes,
                    error: CallError::ResourceBusy,
                },
            });
        }
        if !self.streams.iter().any(|entry| entry.id == stream) {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::TlsWroteOwnedFailed {
                    bytes,
                    error: CallError::InvalidResource,
                },
            });
        }
        if let Some(failure) = self.reject_if_full_or_zero(call_id, timeout) {
            let error = match failure.result {
                CallOutput::Failed(error) => error,
                other => {
                    return Some(DriverCompletion {
                        call_id,
                        result: other,
                    });
                }
            };
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::TlsWroteOwnedFailed { bytes, error },
            });
        }
        self.pending.push(TlsPending {
            call_id,
            lane,
            deadline: deadline_after(now, timeout),
            cancelled: false,
            state: TlsOpState::WriteOwned {
                stream,
                plaintext: bytes,
                fed: 0,
            },
        });
        None
    }

    pub(super) fn submit_close(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        let Some(index) = self.streams.iter().position(|entry| entry.id == stream) else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::InvalidResource),
            });
        };
        // Close is teardown, not new in-flight work, so it is exempt from the
        // lane capacity cap: a shard at capacity must still be able to drain a
        // stream (a `TlsFull` here would deadlock cleanup, and closing in fact
        // frees the stream's own in-flight op). A zero timeout is still a
        // typed `Timeout`.
        if timeout.is_zero() {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Timeout),
            });
        }
        // Close wins: cancel any pending read/write on this stream. Its
        // backend completion slot lives in the stream's `TlsIo`, which the
        // close op takes ownership of below — so removing the read/write
        // pending entry never drops a live slot.
        for pending in self.pending.iter_mut() {
            if pending.lane == TlsLaneKey::Stream(stream) && !pending.cancelled {
                pending.cancelled = true;
                self.cancelled_by_close.push(pending.call_id);
            }
        }
        // Take the stream out of the table; further read/write/close on this id
        // now see `InvalidResource`. The close op owns the `TlsIo` and flushes
        // `close_notify` through it.
        let entry = self.streams.remove(index);
        self.pending.push(TlsPending {
            call_id,
            lane: TlsLaneKey::Stream(stream),
            deadline: deadline_after(now, timeout),
            cancelled: false,
            state: TlsOpState::Close {
                io: entry.io,
                sent_close_notify: false,
            },
        });
        None
    }

    // --- advance / pump --------------------------------------------------

    pub(super) fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        // Step the shared Betelgeuse loop only while TLS has live work, so a
        // runtime that uses no TLS (and the deterministic simulated-IO TCP
        // tests) never sees an extra step from this lane.
        if self.has_live_socket_work() {
            let _ = self.io_loop.step();
        }

        // Close-wins cancellations become one synthetic `TargetClosed` each.
        while let Some(call_id) = self.cancelled_by_close.pop() {
            if self.pending.iter().any(|p| p.call_id == call_id) {
                completed.push(DriverCompletion {
                    call_id,
                    result: CallOutput::Failed(CallError::TargetClosed),
                });
            }
        }

        // Whole-op timeouts: one `Timeout` each, then tombstone.
        for pending in self.pending.iter_mut() {
            if !pending.cancelled && now >= pending.deadline {
                pending.cancelled = true;
                completed.push(DriverCompletion {
                    call_id: pending.call_id,
                    result: CallOutput::Failed(CallError::Timeout),
                });
            }
        }

        // Pump active ops. Index-based so completed ops can hand their `TlsIo`
        // back into `self.streams` and so cancelled tombstones can be reaped.
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].cancelled {
                // Tombstone: drop once the backend no longer holds the slot.
                if self.pending[index].holds_live_backend_ref() {
                    index += 1;
                } else {
                    self.pending.remove(index);
                }
                continue;
            }
            match self.pump(index) {
                Some(result) => {
                    let call_id = self.pending.remove(index).call_id;
                    completed.push(DriverCompletion { call_id, result });
                }
                None => index += 1,
            }
        }
    }

    /// Drives one active pending op. Returns `Some(output)` when it completes,
    /// `None` when it is still in flight.
    fn pump(&mut self, index: usize) -> Option<CallOutput> {
        // Read/Write borrow the `TlsIo` from `self.streams`; the others own it
        // in their pending state. Split the borrow accordingly.
        match &self.pending[index].state {
            TlsOpState::Read { .. }
            | TlsOpState::ReadBuf { .. }
            | TlsOpState::Write { .. }
            | TlsOpState::WriteOwned { .. } => self.pump_stream_op(index),
            TlsOpState::Connect { .. } => self.pump_connect(index),
            TlsOpState::Accept { .. } => self.pump_accept(index),
            TlsOpState::Close { .. } => self.pump_close(index),
        }
    }

    fn pump_connect(&mut self, index: usize) -> Option<CallOutput> {
        let step = {
            let TlsOpState::Connect { io, .. } = &mut self.pending[index].state else {
                unreachable!("pump_connect on non-connect op");
            };
            drive_handshake(io)
        };
        match step {
            PumpStep::Pending => None,
            PumpStep::Failed(error) => Some(CallOutput::Failed(error)),
            PumpStep::Done => {
                let TlsOpState::Connect { io, alpn_offered } =
                    std::mem::replace(&mut self.pending[index].state, TlsOpState::taken())
                else {
                    unreachable!();
                };
                if alpn_offered && io.conn.alpn_protocol().is_none() {
                    return Some(CallOutput::Failed(CallError::TlsAlpnMismatch));
                }
                let selected_alpn = io.conn.alpn_protocol().map(<[u8]>::to_vec);
                let id = self.register_stream(io);
                Some(CallOutput::TlsConnected {
                    stream: id,
                    selected_alpn,
                })
            }
        }
    }

    fn pump_accept(&mut self, index: usize) -> Option<CallOutput> {
        // Phase A: wait for the listener accept to land a socket, then wrap it
        // in a `ServerConnection`.
        {
            let TlsOpState::Accept {
                config,
                accept,
                io,
                peer_addr,
                ..
            } = &mut self.pending[index].state
            else {
                unreachable!("pump_accept on non-accept op");
            };
            if io.is_none() {
                if !accept.has_result() {
                    return None;
                }
                let socket = match accept.take_result().expect("accept advertised a result") {
                    Ok(socket) => socket,
                    Err(_) => return Some(CallOutput::Failed(CallError::Io)),
                };
                let addr = match socket.peer_addr() {
                    Ok(addr) => addr,
                    Err(_) => return Some(CallOutput::Failed(CallError::Io)),
                };
                let conn = match rustls::ServerConnection::new(Arc::clone(config)) {
                    Ok(conn) => conn,
                    Err(_) => return Some(CallOutput::Failed(CallError::TlsHandshake)),
                };
                *peer_addr = Some(addr);
                *io = Some(TlsIo::new(rustls::Connection::Server(conn), socket));
            }
        }

        // Phase B: drive the server handshake.
        let step = {
            let TlsOpState::Accept { io, .. } = &mut self.pending[index].state else {
                unreachable!();
            };
            drive_handshake(io.as_mut().expect("io set in phase A"))
        };
        match step {
            PumpStep::Pending => None,
            PumpStep::Failed(error) => Some(CallOutput::Failed(error)),
            PumpStep::Done => {
                let TlsOpState::Accept { io, peer_addr, .. } =
                    std::mem::replace(&mut self.pending[index].state, TlsOpState::taken())
                else {
                    unreachable!();
                };
                let io = io.expect("io set in phase A");
                let selected_alpn = io.conn.alpn_protocol().map(<[u8]>::to_vec);
                let id = self.register_stream(io);
                Some(CallOutput::TlsAccepted {
                    stream: id,
                    peer_addr: peer_addr.expect("peer addr set in phase A"),
                    selected_alpn,
                })
            }
        }
    }

    fn pump_close(&mut self, index: usize) -> Option<CallOutput> {
        let TlsOpState::Close {
            io,
            sent_close_notify,
        } = &mut self.pending[index].state
        else {
            unreachable!("pump_close on non-close op");
        };
        // Harvest any socket op the preempted read/write left in flight before
        // queueing close_notify, so we never run two socket ops at once. We
        // cannot abandon that op: its completion box lives in this `TlsIo`, and
        // the backend still points at it, so completing the close here would
        // drop a referenced box. We therefore wait for the inherited op to
        // release; the whole-op deadline is the backstop (an idle peer turns
        // this close into a `Timeout`). In practice only the isolate-facing
        // close of an *idle* stream reaches here (one pending op per stream),
        // so `socket_op` is normally already Idle.
        if let Err(error) = io.harvest() {
            return Some(CallOutput::Failed(error));
        }
        if io.socket_op.live() {
            return None;
        }
        if !*sent_close_notify {
            io.conn.send_close_notify();
            *sent_close_notify = true;
        }
        io.egress();
        match io.flush_or_fill(false) {
            Ok(true) => None,
            Ok(false) => Some(CallOutput::TlsClosed),
            Err(error) => Some(CallOutput::Failed(error)),
        }
    }

    fn pump_stream_op(&mut self, index: usize) -> Option<CallOutput> {
        let stream_id = match &self.pending[index].state {
            TlsOpState::Read { stream, .. }
            | TlsOpState::ReadBuf { stream, .. }
            | TlsOpState::Write { stream, .. }
            | TlsOpState::WriteOwned { stream, .. } => *stream,
            _ => unreachable!(),
        };
        let Some(stream_index) = self.streams.iter().position(|entry| entry.id == stream_id) else {
            // The stream vanished (closed underneath us). Surface it as a
            // closed target rather than a silent hang.
            return Some(CallOutput::Failed(CallError::TargetClosed));
        };

        // Pre-step: harvest the one in-flight socket op, feed/process inbound,
        // and pull outbound ciphertext. Scoped so the goal step can re-borrow.
        {
            let io = &mut self.streams[stream_index].io;
            if let Err(error) = io.harvest() {
                return Some(CallOutput::Failed(error));
            }
            if io.socket_op.live() {
                return None;
            }
            if let Err(error) = io.ingest() {
                return Some(CallOutput::Failed(error));
            }
            io.egress();
        }

        match &mut self.pending[index].state {
            TlsOpState::Read { max_len, .. } => {
                let max_len = *max_len;
                let io = &mut self.streams[stream_index].io;
                // Flush any wants_write (alerts, key updates) before reading —
                // the write-during-read path, still one socket op at a time.
                if !io.outbound.is_empty() {
                    return match io.flush_or_fill(false) {
                        Ok(_) => None,
                        Err(error) => Some(CallOutput::Failed(error)),
                    };
                }
                // `wants_read()` is true exactly when rustls has no buffered
                // plaintext and the peer has not cleanly closed. If so — and we
                // have not hit TCP EOF — there is nothing to read yet, so fetch
                // more ciphertext *before* allocating a `max_len` read buffer
                // for data that has not arrived. Only allocate once there is
                // plaintext to deliver or a close/EOF to surface.
                if io.conn.wants_read() && !io.read_eof {
                    return match io.flush_or_fill(true) {
                        Ok(true) => None,
                        // `!read_eof` guarantees `flush_or_fill` armed a recv.
                        Ok(false) => Some(CallOutput::Failed(CallError::Io)),
                        Err(error) => Some(CallOutput::Failed(error)),
                    };
                }
                if max_len == 0 {
                    return Some(CallOutput::TlsRead { bytes: Vec::new() });
                }
                let mut buffer = vec![0_u8; max_len];
                match std::io::Read::read(&mut io.conn.reader(), &mut buffer) {
                    Ok(count) => {
                        buffer.truncate(count);
                        Some(CallOutput::TlsRead { bytes: buffer })
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        // Rare: rustls reported no plaintext after all. Fetch
                        // more ciphertext rather than allocate again next turn.
                        match io.flush_or_fill(true) {
                            Ok(true) => None,
                            Ok(false) => Some(CallOutput::Failed(CallError::Io)),
                            Err(error) => Some(CallOutput::Failed(error)),
                        }
                    }
                    // Clean close_notify reads as `Ok(0)` above (empty
                    // `TlsRead`, matching today). Truncation surfaces here as
                    // `UnexpectedEof` -> `Io`, distinct from a clean close.
                    Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                        Some(CallOutput::Failed(CallError::Io))
                    }
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            TlsOpState::ReadBuf {
                buffer, max_len, ..
            } => {
                let max_len = *max_len;
                let io = &mut self.streams[stream_index].io;
                if !io.outbound.is_empty() {
                    return match io.flush_or_fill(false) {
                        Ok(_) => None,
                        Err(error) => Some(CallOutput::TlsReadBufFailed {
                            buffer: std::mem::take(buffer),
                            error,
                        }),
                    };
                }
                if io.conn.wants_read() && !io.read_eof {
                    return match io.flush_or_fill(true) {
                        Ok(true) => None,
                        Ok(false) => Some(CallOutput::TlsReadBufFailed {
                            buffer: std::mem::take(buffer),
                            error: CallError::Io,
                        }),
                        Err(error) => Some(CallOutput::TlsReadBufFailed {
                            buffer: std::mem::take(buffer),
                            error,
                        }),
                    };
                }
                buffer.resize(max_len, 0);
                match std::io::Read::read(&mut io.conn.reader(), buffer) {
                    Ok(count) => {
                        buffer.truncate(count);
                        let returned = std::mem::take(buffer);
                        Some(CallOutput::TlsReadBuf {
                            buffer: returned,
                            len: count,
                        })
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        match io.flush_or_fill(true) {
                            Ok(true) => None,
                            Ok(false) => Some(CallOutput::TlsReadBufFailed {
                                buffer: std::mem::take(buffer),
                                error: CallError::Io,
                            }),
                            Err(error) => Some(CallOutput::TlsReadBufFailed {
                                buffer: std::mem::take(buffer),
                                error,
                            }),
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                        Some(CallOutput::TlsReadBufFailed {
                            buffer: std::mem::take(buffer),
                            error: CallError::Io,
                        })
                    }
                    Err(_) => Some(CallOutput::TlsReadBufFailed {
                        buffer: std::mem::take(buffer),
                        error: CallError::Io,
                    }),
                }
            }
            TlsOpState::Write { plaintext, fed, .. } => {
                let io = &mut self.streams[stream_index].io;
                // Feed as much plaintext as rustls' bounded buffer accepts,
                // then flush its ciphertext before feeding more.
                while *fed < plaintext.len() {
                    let written = std::io::Write::write(&mut io.conn.writer(), &plaintext[*fed..])
                        .unwrap_or(0);
                    if written == 0 {
                        break;
                    }
                    *fed += written;
                }
                io.egress();
                let drained =
                    *fed >= plaintext.len() && io.outbound.is_empty() && !io.conn.wants_write();
                if drained && matches!(io.socket_op, SocketOp::Idle) {
                    return Some(CallOutput::TlsWrote {
                        count: plaintext.len(),
                    });
                }
                match io.flush_or_fill(false) {
                    Ok(_) => None,
                    Err(error) => Some(CallOutput::Failed(error)),
                }
            }
            TlsOpState::WriteOwned { plaintext, fed, .. } => {
                let io = &mut self.streams[stream_index].io;
                while *fed < plaintext.len() {
                    let written = std::io::Write::write(&mut io.conn.writer(), &plaintext[*fed..])
                        .unwrap_or(0);
                    if written == 0 {
                        break;
                    }
                    *fed += written;
                }
                io.egress();
                let drained =
                    *fed >= plaintext.len() && io.outbound.is_empty() && !io.conn.wants_write();
                if drained && matches!(io.socket_op, SocketOp::Idle) {
                    let count = plaintext.len();
                    return Some(CallOutput::TlsWroteOwned {
                        bytes: std::mem::take(plaintext),
                        count,
                    });
                }
                match io.flush_or_fill(false) {
                    Ok(_) => None,
                    Err(error) => Some(CallOutput::TlsWroteOwnedFailed {
                        bytes: std::mem::take(plaintext),
                        error,
                    }),
                }
            }
            _ => unreachable!(),
        }
    }

    /// Moves a fully-handshaked connection into the stream table and returns
    /// its freshly-allocated id.
    fn register_stream(&mut self, io: TlsIo) -> TlsStreamId {
        let id = TlsStreamId::new(self.next_stream_id);
        self.next_stream_id += 1;
        self.streams.push(TlsStreamEntry { id, io });
        id
    }

    // --- lifecycle -------------------------------------------------------

    pub(super) fn has_pending(&self) -> bool {
        self.pending.iter().any(|entry| !entry.cancelled)
    }

    /// Whether any backend completion slot is in flight — used to decide
    /// whether to step the shared loop this turn.
    fn has_live_socket_work(&self) -> bool {
        self.pending.iter().any(|p| p.holds_live_backend_ref())
            || self.streams.iter().any(|s| s.io.socket_op.live())
    }

    pub(super) fn cancel(&mut self, call_id: CallId) -> bool {
        let Some(pending) = self
            .pending
            .iter_mut()
            .find(|entry| entry.call_id == call_id && !entry.cancelled)
        else {
            return false;
        };
        pending.cancelled = true;
        true
    }

    pub(super) fn cancel_pending(&mut self, deadline: Instant) -> Result<(), DriverShutdownError> {
        // Orderly shutdown only. We share the io_loop with the TCP lane, so
        // `cancel_pending_completions` targets *every* backend-held slot. On
        // Linux, cancelling an in-flight operation is itself asynchronous:
        // the completion box is not safe to drop until the backend reports
        // the cancelled result. Drain TLS-owned boxes here before TCP performs
        // the final shared-loop shutdown check.
        //
        // This must NOT run from this lane's `Drop`: whole-loop destruction is
        // coordinated by `SharedIoLanes` while every sibling lane remains
        // alive. If release cannot be proven, that owner quarantines every lane
        // and loop handle together rather than freeing referenced boxes.
        for pending in &mut self.pending {
            pending.cancelled = true;
        }
        let cancel_result = self.io_loop.cancel_pending_completions();
        while Instant::now() < deadline && self.has_live_socket_work() {
            let _ = self.io_loop.step();
            self.reap_cancelled_tombstones();
            thread::sleep(Duration::from_millis(1));
        }
        self.reap_cancelled_tombstones();
        let tls_released = !self.has_live_socket_work();
        if tls_released {
            self.pending.clear();
            self.streams.clear();
        }
        for entry in self.listeners.drain(..) {
            entry.socket.close();
        }
        if cancel_result.is_err() || !tls_released {
            return Err(DriverShutdownError::BackendStillOwnsCompletions);
        }
        Ok(())
    }

    pub(super) fn resource_report(&self) -> DriverResourceReport {
        // worker_held: in-flight ops holding a socket not represented by a
        // table id (connect/accept/close). Read/write keep their socket in the
        // stream table, so they are counted under tls_streams instead.
        let worker_held = self
            .pending
            .iter()
            .filter(|p| p.holds_untabled_socket())
            .count();
        DriverResourceReport {
            listeners: self.listeners.len(),
            tls_streams: self.streams.len(),
            worker_held,
            // Physical pending entries the runtime still waits on. Cancelled
            // tombstones drop out; timeout/close-win entries already delivered
            // their terminal and are cancelled, so they too drop out here.
            pending_calls: self.pending.iter().filter(|p| !p.cancelled).count(),
            ..DriverResourceReport::default()
        }
    }

    // --- helpers ---------------------------------------------------------

    fn lane_has_active(&self, lane: TlsLaneKey) -> bool {
        self.pending
            .iter()
            .any(|entry| entry.lane == lane && !entry.cancelled)
    }

    fn reject_if_full_or_zero(
        &self,
        call_id: CallId,
        timeout: Duration,
    ) -> Option<DriverCompletion> {
        if timeout.is_zero() {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Timeout),
            });
        }
        if self.pending.iter().filter(|p| !p.cancelled).count() >= self.capacity {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::TlsFull),
            });
        }
        None
    }

    fn reap_cancelled_tombstones(&mut self) {
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].cancelled && !self.pending[index].holds_live_backend_ref() {
                self.pending.remove(index);
            } else {
                index += 1;
            }
        }
    }
}

// No `Drop` impl on purpose: the TLS lane shares its Betelgeuse loop with the
// TCP/storage/Unix lanes. `SharedIoLanes` owns whole-loop cancellation and the
// reclaim-or-quarantine decision before any completion storage can drop.

impl TlsPending {
    /// Whether the backend still holds a pointer to one of this op's
    /// completion slots — gates tombstone reaping.
    fn holds_live_backend_ref(&self) -> bool {
        match &self.state {
            TlsOpState::Connect { io, .. } | TlsOpState::Close { io, .. } => io.socket_op.live(),
            TlsOpState::Accept { io: Some(io), .. } => io.socket_op.live(),
            TlsOpState::Accept {
                io: None, accept, ..
            } => !accept.has_result(),
            // Read/Write drive a socket owned by the stream table, not by the
            // op, so the op holds no slot of its own.
            TlsOpState::Read { .. }
            | TlsOpState::ReadBuf { .. }
            | TlsOpState::Write { .. }
            | TlsOpState::WriteOwned { .. } => false,
        }
    }

    /// Whether this op holds a socket not represented by a stream-table id.
    fn holds_untabled_socket(&self) -> bool {
        matches!(
            self.state,
            TlsOpState::Connect { .. } | TlsOpState::Accept { .. } | TlsOpState::Close { .. }
        )
    }
}

impl TlsOpState {
    /// A spent placeholder used while moving a [`TlsIo`] out of a pending op.
    fn taken() -> Self {
        Self::Read {
            stream: TlsStreamId::new(0),
            max_len: 0,
        }
    }
}

/// Drives a [`TlsIo`] one step toward a completed handshake.
fn drive_handshake(io: &mut TlsIo) -> PumpStep {
    match io.harvest() {
        Ok(_) => {}
        Err(error) => return PumpStep::Failed(error),
    }
    if io.socket_op.live() {
        return PumpStep::Pending;
    }
    if let Err(error) = io.ingest() {
        return PumpStep::Failed(error);
    }
    io.egress();
    // Flush any handshake output (including the client's final `Finished`)
    // before declaring the handshake done — otherwise the peer never sees the
    // last flight and its own handshake hangs forever.
    if !io.outbound.is_empty() {
        return match io.arm_send() {
            Ok(()) => PumpStep::Pending,
            Err(error) => PumpStep::Failed(error),
        };
    }
    if !io.conn.is_handshaking() {
        return PumpStep::Done;
    }
    // Still handshaking and nothing to send: read more. A clean EOF
    // mid-handshake is a truncated handshake.
    if io.read_eof {
        return PumpStep::Failed(CallError::TlsHandshake);
    }
    match io.arm_recv() {
        Ok(()) => PumpStep::Pending,
        Err(error) => PumpStep::Failed(error),
    }
}

/// Builds a rustls client connection with explicit DER roots and optional ALPN.
/// No system root store, no client auth — the same security posture as before.
fn build_client_connection(
    server_name: &str,
    root_certificates: Vec<Vec<u8>>,
    alpn_protocols: Vec<Vec<u8>>,
) -> Result<rustls::ClientConnection, CallError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    for certificate in root_certificates {
        roots
            .add(rustls::pki_types::CertificateDer::from(certificate))
            .map_err(|_| CallError::TlsCertificate)?;
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn_protocols;
    let server_name = rustls::pki_types::ServerName::try_from(server_name.to_string())
        .map_err(|_| CallError::TlsName)?;
    rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|_| CallError::TlsName)
}

/// Builds a rustls server config from explicit DER cert chain + PKCS#8 key and
/// optional ALPN.
fn build_server_config(
    certificate_chain: Vec<Vec<u8>>,
    private_key: Vec<u8>,
    alpn_protocols: Vec<Vec<u8>>,
) -> Result<rustls::ServerConfig, CallError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
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
    Ok(config)
}

#[cfg(test)]
mod tests {
    //! Lane-level proofs. Each test runs a TLS client *and* server in a single
    //! lane on one Betelgeuse loop — the case FINDINGS #16 called impossible —
    //! using only the runtime's own socket rail and no spawned worker, so the
    //! substrate guard test that scans this file stays green.
    use super::*;

    fn localhost_identity() -> (Vec<u8>, Vec<u8>) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("generate local cert");
        (
            certified.cert.der().to_vec(),
            certified.key_pair.serialize_der(),
        )
    }

    /// A self-signed `localhost` cert whose validity window is entirely in the
    /// past, so a conformant client must reject it during the handshake.
    fn expired_localhost_identity() -> (Vec<u8>, Vec<u8>) {
        let mut params =
            rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("cert params");
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        let key = rcgen::KeyPair::generate().expect("key pair");
        let cert = params.self_signed(&key).expect("self-sign expired cert");
        (cert.der().to_vec(), key.serialize_der())
    }

    fn fresh_lane(capacity: usize) -> TlsLane {
        let io_loop = io_loop(Global).expect("init io loop for tls lane test");
        TlsLane::new(capacity, io_loop)
    }

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().expect("loopback addr")
    }

    /// Advance the lane until `call_id` completes, returning its output. Keeps
    /// a shared completion buffer so a peer op finishing first is not lost.
    fn drive(
        lane: &mut TlsLane,
        completed: &mut Vec<DriverCompletion>,
        call_id: CallId,
    ) -> CallOutput {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(index) = completed.iter().position(|c| c.call_id == call_id) {
                return completed.remove(index).result;
            }
            assert!(
                Instant::now() < deadline,
                "call {call_id:?} did not complete"
            );
            lane.advance(Instant::now(), completed);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn long() -> Duration {
        Duration::from_secs(5)
    }

    /// bind a server, accept + connect on the same lane, returning both stream
    /// ids and both handshake outputs.
    fn handshake_pair(
        lane: &mut TlsLane,
        completed: &mut Vec<DriverCompletion>,
        server_alpn: Vec<Vec<u8>>,
        client_alpn: Vec<Vec<u8>>,
    ) -> (TlsStreamId, CallOutput, TlsStreamId, CallOutput) {
        let (cert, key) = localhost_identity();
        let bound = lane
            .submit_bind(
                CallId::new(1),
                loopback(),
                vec![cert.clone()],
                key,
                server_alpn,
                Instant::now(),
            )
            .expect("bind completes inline");
        let (listener, addr) = match bound.result {
            CallOutput::TlsBound {
                listener,
                local_addr,
            } => (listener, local_addr),
            other => panic!("unexpected bind: {other:?}"),
        };
        assert!(
            lane.submit_accept(CallId::new(2), listener, long(), Instant::now())
                .is_none()
        );
        assert!(
            lane.submit_connect(
                CallId::new(3),
                addr,
                "localhost".to_string(),
                vec![cert],
                client_alpn,
                long(),
                Instant::now(),
            )
            .is_none()
        );
        let accepted = drive(lane, completed, CallId::new(2));
        let server_stream = match &accepted {
            CallOutput::TlsAccepted { stream, .. } => *stream,
            other => panic!("unexpected accept: {other:?}"),
        };
        let connected = drive(lane, completed, CallId::new(3));
        let client_stream = match &connected {
            CallOutput::TlsConnected { stream, .. } => *stream,
            other => panic!("unexpected connect: {other:?}"),
        };
        (server_stream, accepted, client_stream, connected)
    }

    #[test]
    fn lane_runs_client_and_server_echo_in_one_lane() {
        let mut lane = fresh_lane(64);
        let mut completed = Vec::new();
        let (server_stream, _accepted, client_stream, _connected) =
            handshake_pair(&mut lane, &mut completed, Vec::new(), Vec::new());

        // client -> server
        assert!(
            lane.submit_write(
                CallId::new(4),
                client_stream,
                b"ping".to_vec(),
                long(),
                Instant::now()
            )
            .is_none()
        );
        assert!(matches!(
            drive(&mut lane, &mut completed, CallId::new(4)),
            CallOutput::TlsWrote { count: 4 }
        ));
        assert!(
            lane.submit_read(CallId::new(5), server_stream, 16, long(), Instant::now())
                .is_none()
        );
        assert_eq!(
            match drive(&mut lane, &mut completed, CallId::new(5)) {
                CallOutput::TlsRead { bytes } => bytes,
                other => panic!("{other:?}"),
            },
            b"ping"
        );

        // server -> client
        assert!(
            lane.submit_write(
                CallId::new(6),
                server_stream,
                b"pong".to_vec(),
                long(),
                Instant::now()
            )
            .is_none()
        );
        assert!(matches!(
            drive(&mut lane, &mut completed, CallId::new(6)),
            CallOutput::TlsWrote { count: 4 }
        ));
        assert!(
            lane.submit_read(CallId::new(7), client_stream, 16, long(), Instant::now())
                .is_none()
        );
        assert_eq!(
            match drive(&mut lane, &mut completed, CallId::new(7)) {
                CallOutput::TlsRead { bytes } => bytes,
                other => panic!("{other:?}"),
            },
            b"pong"
        );

        // clean close_notify -> the peer's read sees a clean (empty) EOF.
        assert!(
            lane.submit_close(CallId::new(8), client_stream, long(), Instant::now())
                .is_none()
        );
        assert!(matches!(
            drive(&mut lane, &mut completed, CallId::new(8)),
            CallOutput::TlsClosed
        ));
        assert!(
            lane.submit_read(CallId::new(9), server_stream, 16, long(), Instant::now())
                .is_none()
        );
        assert_eq!(
            match drive(&mut lane, &mut completed, CallId::new(9)) {
                CallOutput::TlsRead { bytes } => bytes,
                other => panic!("clean close should read empty, got {other:?}"),
            },
            Vec::<u8>::new()
        );
    }

    #[test]
    fn lane_transfers_a_large_payload_in_bounded_chunks() {
        // A payload several TLS records / pump-turn caps long. One `tls_write`
        // drives encrypt+flush incrementally with a bounded plaintext buffer and
        // a per-turn ciphertext cap; the reader drains it across many reads. No
        // unbounded buffering, no `ResourceBusy`.
        let mut lane = fresh_lane(64);
        let mut completed = Vec::new();
        let (server_stream, _accepted, client_stream, _connected) =
            handshake_pair(&mut lane, &mut completed, Vec::new(), Vec::new());

        let payload: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
        assert!(
            lane.submit_write(
                CallId::new(100),
                client_stream,
                payload.clone(),
                long(),
                Instant::now()
            )
            .is_none()
        );
        assert!(matches!(
            drive(&mut lane, &mut completed, CallId::new(100)),
            CallOutput::TlsWrote { count } if count == payload.len()
        ));

        let mut received = Vec::new();
        let mut next = 101;
        while received.len() < payload.len() {
            assert!(
                lane.submit_read(
                    CallId::new(next),
                    server_stream,
                    64 * 1024,
                    long(),
                    Instant::now()
                )
                .is_none()
            );
            match drive(&mut lane, &mut completed, CallId::new(next)) {
                CallOutput::TlsRead { bytes } => {
                    assert!(!bytes.is_empty(), "unexpected clean EOF mid-transfer");
                    received.extend_from_slice(&bytes);
                }
                other => panic!("unexpected read output: {other:?}"),
            }
            next += 1;
        }
        assert_eq!(received, payload);
    }

    #[test]
    fn owned_tls_write_failures_return_plaintext_bytes() {
        let mut lane = fresh_lane(64);
        let invalid_bytes = b"invalid stream".to_vec();
        let invalid = lane
            .submit_write_owned(
                CallId::new(10),
                TlsStreamId::new(99),
                invalid_bytes.clone(),
                long(),
                Instant::now(),
            )
            .expect("invalid stream completes inline");
        assert!(matches!(
            invalid.result,
            CallOutput::TlsWroteOwnedFailed {
                bytes,
                error: CallError::InvalidResource,
            } if bytes == invalid_bytes
        ));

        let mut completed = Vec::new();
        let (_server_stream, _accepted, client_stream, _connected) =
            handshake_pair(&mut lane, &mut completed, Vec::new(), Vec::new());

        let timeout_bytes = b"zero timeout".to_vec();
        let timeout = lane
            .submit_write_owned(
                CallId::new(11),
                client_stream,
                timeout_bytes.clone(),
                Duration::ZERO,
                Instant::now(),
            )
            .expect("zero timeout completes inline");
        assert!(matches!(
            timeout.result,
            CallOutput::TlsWroteOwnedFailed {
                bytes,
                error: CallError::Timeout,
            } if bytes == timeout_bytes
        ));

        assert!(
            lane.submit_read_buf(
                CallId::new(12),
                client_stream,
                Vec::new(),
                16,
                long(),
                Instant::now()
            )
            .is_none()
        );
        let busy_bytes = b"busy lane".to_vec();
        let busy = lane
            .submit_write_owned(
                CallId::new(13),
                client_stream,
                busy_bytes.clone(),
                long(),
                Instant::now(),
            )
            .expect("busy stream completes inline");
        assert!(matches!(
            busy.result,
            CallOutput::TlsWroteOwnedFailed {
                bytes,
                error: CallError::ResourceBusy,
            } if bytes == busy_bytes
        ));
    }

    #[test]
    fn lane_negotiates_alpn_h2_on_both_sides() {
        let mut lane = fresh_lane(64);
        let mut completed = Vec::new();
        let (_server_stream, accepted, _client_stream, connected) = handshake_pair(
            &mut lane,
            &mut completed,
            vec![b"h2".to_vec()],
            vec![b"h2".to_vec()],
        );
        assert!(matches!(
            accepted,
            CallOutput::TlsAccepted { selected_alpn: Some(ref a), .. } if a == b"h2"
        ));
        assert!(matches!(
            connected,
            CallOutput::TlsConnected { selected_alpn: Some(ref a), .. } if a == b"h2"
        ));
    }

    #[test]
    fn lane_connect_alpn_offered_but_server_declines_is_mismatch() {
        let mut lane = fresh_lane(64);
        let mut completed = Vec::new();
        let (cert, key) = localhost_identity();
        let bound = lane
            .submit_bind(
                CallId::new(1),
                loopback(),
                vec![cert.clone()],
                key,
                Vec::new(),
                Instant::now(),
            )
            .expect("bind inline");
        let (listener, addr) = match bound.result {
            CallOutput::TlsBound {
                listener,
                local_addr,
            } => (listener, local_addr),
            other => panic!("{other:?}"),
        };
        assert!(
            lane.submit_accept(CallId::new(2), listener, long(), Instant::now())
                .is_none()
        );
        assert!(
            lane.submit_connect(
                CallId::new(3),
                addr,
                "localhost".to_string(),
                vec![cert],
                vec![b"h2".to_vec()],
                long(),
                Instant::now(),
            )
            .is_none()
        );
        assert!(matches!(
            drive(&mut lane, &mut completed, CallId::new(3)),
            CallOutput::Failed(CallError::TlsAlpnMismatch)
        ));
    }

    #[test]
    fn lane_connect_without_alpn_offer_negotiates_none() {
        let mut lane = fresh_lane(64);
        let mut completed = Vec::new();
        let (_server, _accepted, _client, connected) =
            handshake_pair(&mut lane, &mut completed, vec![b"h2".to_vec()], Vec::new());
        assert!(matches!(
            connected,
            CallOutput::TlsConnected {
                selected_alpn: None,
                ..
            }
        ));
    }

    #[test]
    fn lane_rejects_second_op_on_a_stream_with_resource_busy() {
        let mut lane = fresh_lane(64);
        let mut completed = Vec::new();
        let (_server, _accepted, client, _connected) =
            handshake_pair(&mut lane, &mut completed, Vec::new(), Vec::new());
        // First read parks (no data from the idle peer).
        assert!(
            lane.submit_read(CallId::new(10), client, 16, long(), Instant::now())
                .is_none()
        );
        // Second op on the same stream is rejected, not queued behind it.
        let busy = lane
            .submit_write(
                CallId::new(11),
                client,
                b"x".to_vec(),
                long(),
                Instant::now(),
            )
            .expect("second op rejected inline");
        assert!(matches!(
            busy.result,
            CallOutput::Failed(CallError::ResourceBusy)
        ));
    }

    #[test]
    fn lane_in_flight_connect_counts_worker_held_and_pending_only() {
        let mut lane = fresh_lane(64);
        let (cert, key) = localhost_identity();
        // Bind but never accept; the connect's handshake stays in flight.
        let bound = lane
            .submit_bind(
                CallId::new(1),
                loopback(),
                vec![cert.clone()],
                key,
                Vec::new(),
                Instant::now(),
            )
            .expect("bind inline");
        let addr = match bound.result {
            CallOutput::TlsBound { local_addr, .. } => local_addr,
            other => panic!("{other:?}"),
        };
        assert!(
            lane.submit_connect(
                CallId::new(2),
                addr,
                "localhost".to_string(),
                vec![cert],
                Vec::new(),
                long(),
                Instant::now()
            )
            .is_none()
        );
        let report = lane.resource_report();
        assert_eq!(
            report.owned_resource_count(),
            1,
            "the listener is table-owned"
        );
        assert_eq!(
            report.worker_held_resource_count(),
            1,
            "the connecting socket is un-tabled"
        );
        assert_eq!(
            report.pending_driver_call_count(),
            1,
            "the connect is the only pending op"
        );
    }

    #[test]
    fn lane_capacity_rejects_overflow_with_tls_full() {
        let mut lane = fresh_lane(1);
        let addr: SocketAddr = "127.0.0.1:9".parse().expect("discard port addr");
        assert!(
            lane.submit_connect(
                CallId::new(1),
                addr,
                "localhost".to_string(),
                Vec::new(),
                Vec::new(),
                long(),
                Instant::now()
            )
            .is_none()
        );
        let full = lane
            .submit_connect(
                CallId::new(2),
                addr,
                "localhost".to_string(),
                Vec::new(),
                Vec::new(),
                long(),
                Instant::now(),
            )
            .expect("second op past capacity rejected inline");
        assert!(matches!(
            full.result,
            CallOutput::Failed(CallError::TlsFull)
        ));
    }

    #[test]
    fn lane_connect_times_out_once_when_handshake_stalls() {
        let mut lane = fresh_lane(64);
        let mut completed = Vec::new();
        let (cert, key) = localhost_identity();
        // Bind but never accept: the TCP connect lands in the backlog, the
        // client sends its ClientHello, and then waits forever for a server
        // that never reads it. The whole-op deadline fires once.
        let bound = lane
            .submit_bind(
                CallId::new(1),
                loopback(),
                vec![cert.clone()],
                key,
                Vec::new(),
                Instant::now(),
            )
            .expect("bind inline");
        let addr = match bound.result {
            CallOutput::TlsBound { local_addr, .. } => local_addr,
            other => panic!("{other:?}"),
        };
        assert!(
            lane.submit_connect(
                CallId::new(2),
                addr,
                "localhost".to_string(),
                vec![cert],
                Vec::new(),
                Duration::from_millis(150),
                Instant::now(),
            )
            .is_none()
        );
        assert!(matches!(
            drive(&mut lane, &mut completed, CallId::new(2)),
            CallOutput::Failed(CallError::Timeout)
        ));
        // The timed-out op delivers exactly one terminal, then tombstones: no
        // second completion, and `has_pending` no longer counts it as active.
        assert!(!lane.has_pending());
        let mut more = Vec::new();
        lane.advance(Instant::now(), &mut more);
        assert!(more.is_empty(), "no second completion for a timed-out op");
    }

    #[test]
    fn lane_connect_rejects_expired_server_cert() {
        // The cert chain validates structurally but its validity window is in
        // the past; the client must reject it during the handshake rather than
        // proceed. Pins that cert validation (here, expiry) is not weakened.
        let mut lane = fresh_lane(64);
        let mut completed = Vec::new();
        let (cert, key) = expired_localhost_identity();
        let bound = lane
            .submit_bind(
                CallId::new(1),
                loopback(),
                vec![cert.clone()],
                key,
                Vec::new(),
                Instant::now(),
            )
            .expect("bind inline");
        let (listener, addr) = match bound.result {
            CallOutput::TlsBound {
                listener,
                local_addr,
            } => (listener, local_addr),
            other => panic!("{other:?}"),
        };
        assert!(
            lane.submit_accept(CallId::new(2), listener, long(), Instant::now())
                .is_none()
        );
        assert!(
            lane.submit_connect(
                CallId::new(3),
                addr,
                "localhost".to_string(),
                vec![cert],
                Vec::new(),
                long(),
                Instant::now(),
            )
            .is_none()
        );
        // The client rejects the expired cert; surfaced as a handshake failure.
        assert!(matches!(
            drive(&mut lane, &mut completed, CallId::new(3)),
            CallOutput::Failed(CallError::TlsHandshake)
        ));
    }

    #[test]
    fn lane_close_is_admitted_even_at_capacity() {
        // Close is teardown, exempt from the in-flight cap: a shard at capacity
        // must still be able to drain a stream. (A `TlsFull` on close would
        // deadlock cleanup.)
        let mut lane = fresh_lane(2);
        let mut completed = Vec::new();
        let (cert, key) = localhost_identity();
        let bound = lane
            .submit_bind(
                CallId::new(1),
                loopback(),
                vec![cert.clone()],
                key,
                Vec::new(),
                Instant::now(),
            )
            .expect("bind inline");
        let (listener, addr) = match bound.result {
            CallOutput::TlsBound {
                listener,
                local_addr,
            } => (listener, local_addr),
            other => panic!("{other:?}"),
        };
        // Establish one real client stream (accept + connect fit capacity 2).
        assert!(
            lane.submit_accept(CallId::new(2), listener, long(), Instant::now())
                .is_none()
        );
        assert!(
            lane.submit_connect(
                CallId::new(3),
                addr,
                "localhost".to_string(),
                vec![cert.clone()],
                Vec::new(),
                long(),
                Instant::now()
            )
            .is_none()
        );
        let _ = drive(&mut lane, &mut completed, CallId::new(2));
        let client_stream = match drive(&mut lane, &mut completed, CallId::new(3)) {
            CallOutput::TlsConnected { stream, .. } => stream,
            other => panic!("connect: {other:?}"),
        };

        // Fill the lane to capacity with two parked connects (the listener has
        // no pending accept, so their handshakes stall).
        assert!(
            lane.submit_connect(
                CallId::new(4),
                addr,
                "localhost".to_string(),
                vec![cert.clone()],
                Vec::new(),
                long(),
                Instant::now()
            )
            .is_none()
        );
        assert!(
            lane.submit_connect(
                CallId::new(5),
                addr,
                "localhost".to_string(),
                vec![cert],
                Vec::new(),
                long(),
                Instant::now()
            )
            .is_none()
        );
        // Sanity: a fresh connect now *is* refused — the cap is real.
        let refused = lane
            .submit_connect(
                CallId::new(6),
                addr,
                "localhost".to_string(),
                Vec::new(),
                Vec::new(),
                long(),
                Instant::now(),
            )
            .expect("connect past capacity refused");
        assert!(matches!(
            refused.result,
            CallOutput::Failed(CallError::TlsFull)
        ));

        // Close of the established stream is admitted despite the full lane,
        // and completes.
        assert!(
            lane.submit_close(CallId::new(7), client_stream, long(), Instant::now())
                .is_none(),
            "close must be admitted at capacity, not refused with TlsFull"
        );
        assert!(matches!(
            drive(&mut lane, &mut completed, CallId::new(7)),
            CallOutput::TlsClosed
        ));
    }

    #[test]
    fn maximum_timeout_survives_a_live_tls_handshake() {
        let mut lane = fresh_lane(2);
        let mut completed = Vec::new();
        let (cert, key) = localhost_identity();
        let bound = lane
            .submit_bind(
                CallId::new(1),
                loopback(),
                vec![cert.clone()],
                key,
                Vec::new(),
                Instant::now(),
            )
            .expect("bind completes inline");
        let (listener, addr) = match bound.result {
            CallOutput::TlsBound {
                listener,
                local_addr,
            } => (listener, local_addr),
            other => panic!("unexpected bind: {other:?}"),
        };

        assert!(
            lane.submit_accept(CallId::new(2), listener, Duration::MAX, Instant::now())
                .is_none()
        );
        assert!(
            lane.submit_connect(
                CallId::new(3),
                addr,
                "localhost".to_string(),
                vec![cert],
                Vec::new(),
                Duration::MAX,
                Instant::now(),
            )
            .is_none()
        );

        assert!(matches!(
            drive(&mut lane, &mut completed, CallId::new(2)),
            CallOutput::TlsAccepted { .. }
        ));
        assert!(matches!(
            drive(&mut lane, &mut completed, CallId::new(3)),
            CallOutput::TlsConnected { .. }
        ));
    }
}
