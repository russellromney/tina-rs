//! Connection isolate.
//!
//! One connection isolate per accepted TCP stream. It:
//!
//! - reads bytes off the runtime-owned TCP stream;
//! - assembles whole frames using [`crate::parse_length_prefix`] and
//!   [`crate::decode_body`];
//! - forwards each request frame to a router address (the service
//!   registry) via an `IsolateCall`;
//! - encodes the reply or error frame and writes it back;
//! - bounds in-flight requests, write queue, and inbound buffer;
//! - closes the stream cleanly on shutdown, on bad-peer input, on idle
//!   timeout, and on peer disconnect, emitting a [`CloseReason`] to the
//!   optional watcher address so each path is observable.
//!
//! # Wire-error invariant
//!
//! Wire errors are server-reported only. The connection isolate maps router
//! outcomes to wire frames:
//!
//! | Router outcome                          | Wire frame                          |
//! |-----------------------------------------|-------------------------------------|
//! | `Replied(Ok(bytes))`                    | `Reply` with `bytes` payload        |
//! | `Replied(UnknownService)`               | `Error(UnknownService)`             |
//! | `Replied(UnknownMethod)`                | `Error(UnknownMethod)`              |
//! | `Replied(Decode)`                       | `Error(Decode)`                     |
//! | `Replied(Internal)`                     | `Error(Internal)`                   |
//! | `Full` (router or service mailbox full) | `Error(Full)`                       |
//! | `Closed` (router gone)                  | `Error(Internal)`                   |
//! | `Timeout` (router did not reply)        | *no frame; client times out locally*|
//!
//! `CallOutcome::Timeout` deliberately produces no wire frame. A `Timeout`
//! variant on [`crate::FrameError`] would violate the rule that
//! client-observed conditions never appear on the wire. Configure
//! `service_call_timeout` longer than the typical client deadline so this is
//! a tail case rather than a routine outcome.
//!
//! # Backpressure
//!
//! - One `tcp_read` is in flight at a time. The inbound buffer is naturally
//!   bounded by `max_frame_size + read_chunk` because the parse loop drains
//!   complete frames before each new read is issued.
//! - The write queue is bounded by `write_queue_cap`. Overflow closes the
//!   connection as a bad peer rather than allowing a runaway producer to
//!   pile pending replies in memory.
//! - When in-flight is at capacity, additional request frames are answered
//!   with `Error(Full)` immediately rather than queued.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, RuntimeCall, StreamId, call, sleep_then, tcp_close_stream, tcp_read,
    tcp_write,
};

use crate::frame::{
    DecodeError, Frame, FrameError, FrameKind, FrameLimits, LENGTH_PREFIX_SIZE, decode_body,
    encode, parse_length_prefix,
};

/// Configuration for [`Connection`].
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    /// Maximum body size for any frame on this connection. Mirrors
    /// [`FrameLimits::max_frame_size`].
    pub max_frame_size: usize,
    /// Maximum simultaneous in-flight requests on this connection.
    pub max_in_flight: usize,
    /// Idle timeout. If no read or write completes within this window, the
    /// connection closes with [`CloseReason::Idle`].
    ///
    /// The actual close window is `[idle_timeout, 2 × idle_timeout)` after
    /// the last activity, because the timer is re-armed on each tick using
    /// the current activity-sequence snapshot rather than tracked against
    /// wall-clock time.
    pub idle_timeout: Duration,
    /// Maximum bytes the connection asks the runtime to deliver per `TcpRead`.
    pub read_chunk: usize,
    /// Maximum number of encoded reply/error frames buffered awaiting write.
    /// Overflow closes the connection as a bad peer.
    pub write_queue_cap: usize,
    /// Timeout for the connection's `IsolateCall` to the router. Should be
    /// configured *longer* than the client-side deadline. See module docs for
    /// why.
    pub service_call_timeout: Duration,
}

impl Default for ConnectionConfig {
    /// Conservative defaults: 1 MiB frames, 64 in flight, 30 s idle,
    /// 8 KiB read chunks, 256-frame write queue, 5 s service-call
    /// timeout.
    fn default() -> Self {
        Self {
            max_frame_size: 1024 * 1024,
            max_in_flight: 64,
            idle_timeout: Duration::from_secs(30),
            read_chunk: 8 * 1024,
            write_queue_cap: 256,
            service_call_timeout: Duration::from_secs(5),
        }
    }
}

impl ConnectionConfig {
    /// Default configuration tuned for development and examples — same
    /// as [`ConnectionConfig::default`].
    pub fn dev() -> Self {
        Self::default()
    }

    /// Default configuration with `max_in_flight` clamped to `cap` and
    /// `write_queue_cap` set to `cap.max(16)` so the
    /// "queue holds at least one frame per in-flight slot" invariant
    /// stays satisfied. Useful for building deliberate-overload demos.
    pub fn bounded(cap: usize) -> Self {
        Self {
            max_in_flight: cap,
            write_queue_cap: cap.max(16),
            ..Self::default()
        }
    }

    /// Configuration designed to produce wire-visible overload at very
    /// small bursts: in-flight cap of 1, idle timeout of 5 s, service
    /// timeout of 8 s (longer than the registry default so registry
    /// timeouts dominate). Use for demos that want to show the `Full`
    /// path without needing a slow service.
    pub fn tiny_pressure() -> Self {
        Self {
            max_frame_size: 64 * 1024,
            max_in_flight: 1,
            idle_timeout: Duration::from_secs(5),
            read_chunk: 4096,
            write_queue_cap: 64,
            service_call_timeout: Duration::from_secs(8),
        }
    }
}

/// Request payload the connection forwards to the router.
///
/// The router isolate's mailbox accepts this type and replies with
/// [`RouterReply`]. The connection only knows the type, not the
/// registry implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterRequest {
    /// The wire `request_id` of the originating frame. The router does not
    /// need to interpret it; it is included for tracing/debugging.
    pub request_id: u64,
    /// Service name from the request frame.
    pub service: String,
    /// Method name from the request frame.
    pub method: String,
    /// Opaque request payload bytes.
    pub payload: Vec<u8>,
}

/// Reply payload returned by the router for one [`RouterRequest`].
///
/// The connection turns these into wire frames; see the module docs for the
/// mapping table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouterReply {
    /// The service produced reply bytes for this request.
    Ok(Vec<u8>),
    /// The router does not recognize the named service.
    UnknownService,
    /// The named service does not recognize the named method.
    UnknownMethod,
    /// The service rejected the request payload as undecodable.
    Decode,
    /// The service hit an internal error unrelated to the request payload.
    Internal,
    /// The named service is at capacity. The router emits this when its
    /// isolate-call to the service returned `CallOutcome::Full`.
    Full,
}

/// Why the connection treated a peer as malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadPeerReason {
    /// Length-prefix parsing or body decoding failed.
    ///
    /// If a single read chunk contained valid frames followed by a
    /// malformed one, the valid frames already drained from the inbound
    /// buffer are not routed: the connection closes on the first decode
    /// failure and abandons in-batch work. This is a deliberate "all bets
    /// off when the peer is bad" rule.
    Decode(DecodeError),
    /// Server saw a frame whose `kind` it cannot accept (Reply or Error).
    UnexpectedKind(FrameKind),
    /// The bounded write queue overflowed.
    WriteQueueOverflow,
}

/// Reason the connection closed. Emitted via the optional watcher address so
/// every close path is observable from outside the isolate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// Peer half-closed (read returned `Ok(empty)`).
    PeerClosed,
    /// Bad peer; connection is being closed defensively.
    BadPeer(BadPeerReason),
    /// Idle timeout elapsed with no read or write activity.
    Idle,
    /// External shutdown signal was delivered to this connection.
    Shutdown,
    /// Underlying TCP read/write/close returned an error.
    IoError(CallError),
    /// A locally-produced wire frame could not be encoded under the
    /// connection's own limits. This is a server-side misconfiguration —
    /// usually a reply payload exceeding `max_frame_size` — not peer
    /// misbehavior. The connection closes because no per-request error
    /// frame can be honestly produced.
    LocalEncodeFailed,
}

/// Inbound vocabulary for [`Connection`].
///
/// External callers only need [`ConnectionMsg::Begin`] and
/// [`ConnectionMsg::Shutdown`]. Other variants are runtime-driven and are
/// public so tests can simulate the runtime.
#[derive(Debug, Clone)]
pub enum ConnectionMsg {
    /// Initial kick-off: starts the read loop and arms the idle timer.
    Begin,
    /// Result of an outstanding `tcp_read`.
    Read(Result<Vec<u8>, CallError>),
    /// Result of an outstanding `tcp_write`.
    Wrote(Result<usize, CallError>),
    /// Result of `tcp_close_stream` after a close was issued.
    StreamClosed(Result<(), CallError>),
    /// Reply from the router for an in-flight request.
    Routed {
        /// Wire request id this outcome belongs to.
        request_id: u64,
        /// Service name echoed for error-frame construction.
        service: String,
        /// Method name echoed for error-frame construction.
        method: String,
        /// Router outcome.
        outcome: CallOutcome<RouterReply>,
    },
    /// Idle-timer tick. The `snapshot` is the activity sequence at the time
    /// the timer was armed; if the connection's current sequence has moved
    /// past it, the timer is re-armed; otherwise the connection closes idle.
    IdleTick {
        /// Activity sequence captured when the timer was armed.
        snapshot: u64,
    },
    /// External shutdown request (e.g. from a supervisor).
    Shutdown,
}

/// Initialization data for [`Connection::new`].
///
/// Use the [`ConnectionInit::new`] builder rather than constructing the
/// struct directly:
///
/// ```ignore
/// ConnectionInit::<MyShard>::new(stream, registry)
///     .with_config(ConnectionConfig::bounded(1))
///     .with_watcher(watcher_addr)
/// ```
///
/// Direct field access is retained for back-compat but the
/// `_shard: PhantomData` field is private — use the builder.
pub struct ConnectionInit<S>
where
    S: tina::Shard,
{
    /// Stream id assigned by the runtime when this connection was accepted.
    pub stream: StreamId,
    /// Address of the router (registry) isolate.
    pub router: Address<crate::registry::RegistryMsg, RouterReply>,
    /// Optional address that receives one [`CloseReason`] when this
    /// connection closes.
    pub watcher: Option<Address<CloseReason>>,
    /// Connection-local configuration.
    pub config: ConnectionConfig,
    /// Marker so the type carries the shard parameter.
    #[doc(hidden)]
    pub _shard: std::marker::PhantomData<S>,
}

impl<S> ConnectionInit<S>
where
    S: tina::Shard,
{
    /// Builds the minimum-viable initializer: an accepted stream id and
    /// the registry to forward requests to. Defaults `watcher` to
    /// `None` and `config` to [`ConnectionConfig::default`].
    pub fn new(
        stream: StreamId,
        router: Address<crate::registry::RegistryMsg, RouterReply>,
    ) -> Self {
        Self {
            stream,
            router,
            watcher: None,
            config: ConnectionConfig::default(),
            _shard: std::marker::PhantomData,
        }
    }

    /// Replaces the configuration.
    pub fn with_config(mut self, config: ConnectionConfig) -> Self {
        self.config = config;
        self
    }

    /// Attaches a watcher address that receives one [`CloseReason`]
    /// when the connection closes.
    pub fn with_watcher(mut self, watcher: Address<CloseReason>) -> Self {
        self.watcher = Some(watcher);
        self
    }
}

impl<S> std::fmt::Debug for ConnectionInit<S>
where
    S: tina::Shard,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionInit")
            .field("stream", &self.stream)
            .field("config", &self.config)
            .finish()
    }
}

/// Internal failure mode for [`Connection::drain_inbound`]. Either the peer
/// is malformed (closed as bad peer) or the connection cannot encode a
/// locally-produced frame (closed with [`CloseReason::LocalEncodeFailed`]).
#[derive(Debug)]
enum DrainError {
    BadPeer(BadPeerReason),
    LocalEncode,
}

/// Connection isolate. One instance per accepted TCP stream.
///
/// The isolate is generic over the shard type so applications can host the
/// connection on whatever shard they already use. The router and watcher
/// addresses must point at isolates living on the same runtime.
pub struct Connection<S>
where
    S: tina::Shard,
{
    stream: StreamId,
    router: Address<crate::registry::RegistryMsg, RouterReply>,
    watcher: Option<Address<CloseReason>>,
    config: ConnectionConfig,
    inbound: Vec<u8>,
    in_flight: usize,
    in_flight_ids: HashSet<u64>,
    write_queue: VecDeque<Vec<u8>>,
    /// Bytes currently inside an in-flight `tcp_write`. `None` means no write
    /// is pending; partial-write recovery prepends the unwritten tail back to
    /// the queue.
    write_in_flight: Option<Vec<u8>>,
    activity_seq: u64,
    closing: Option<CloseReason>,
    /// Once true, a `tcp_close_stream` has been dispatched; further messages
    /// are ignored until `StreamClosed` arrives.
    close_dispatched: bool,
    /// Once true, no further `tcp_read` should be issued (closing or peer
    /// half-closed).
    reads_done: bool,
    /// Once true, the read loop has been kicked off by `Begin`; a
    /// duplicate `Begin` is a no-op rather than issuing a second
    /// `tcp_read` (which the runtime rejects as `ResourceBusy`).
    began: bool,
    _shard: std::marker::PhantomData<S>,
}

impl<S> std::fmt::Debug for Connection<S>
where
    S: tina::Shard,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("stream", &self.stream)
            .field("in_flight", &self.in_flight)
            .field("write_queue", &self.write_queue.len())
            .field("write_in_flight", &self.write_in_flight.is_some())
            .field("activity_seq", &self.activity_seq)
            .field("closing", &self.closing)
            .field("close_dispatched", &self.close_dispatched)
            .field("reads_done", &self.reads_done)
            .field("began", &self.began)
            .finish()
    }
}

impl<S> Connection<S>
where
    S: tina::Shard,
{
    /// Builds a new connection isolate from initialization data.
    ///
    /// Panics in debug builds if `config.read_chunk == 0`; a zero
    /// `read_chunk` would cause every `tcp_read` to return `Ok(empty)`
    /// (the runtime's "no bytes requested" path), which the connection
    /// interprets as peer half-close — a healthy stream would tear
    /// itself down. In release builds the read path also clamps to
    /// `read_chunk.max(1)` to keep this from being a silent EOF.
    pub fn new(init: ConnectionInit<S>) -> Self {
        debug_assert!(
            init.config.read_chunk > 0,
            "ConnectionConfig::read_chunk must be > 0 (a zero read returns empty bytes \
             which the connection treats as peer EOF)",
        );
        Self {
            stream: init.stream,
            router: init.router,
            watcher: init.watcher,
            config: init.config,
            inbound: Vec::new(),
            in_flight: 0,
            in_flight_ids: HashSet::new(),
            write_queue: VecDeque::new(),
            write_in_flight: None,
            activity_seq: 0,
            closing: None,
            close_dispatched: false,
            reads_done: false,
            began: false,
            _shard: std::marker::PhantomData,
        }
    }
}

impl<S> Connection<S>
where
    S: tina::Shard,
    Self: tina::Isolate<
            Message = ConnectionMsg,
            Send = Outbound<CloseReason>,
            Io = RuntimeCall<ConnectionMsg>,
        >,
{
    fn frame_limits(&self) -> FrameLimits {
        FrameLimits::new(self.config.max_frame_size)
    }

    fn read_effect(&self) -> Effect<Self> {
        // Clamp to >= 1: a zero-length read returns Ok(empty) which the
        // connection's EOF path interprets as peer half-close. The
        // debug_assert in `Client::new`/`Connection::new` catches this
        // in dev; the clamp is the release-build guard.
        let max_len = self.config.read_chunk.max(1);
        tcp_read(self.stream, max_len).then(ConnectionMsg::Read)
    }

    fn write_effect(bytes: Vec<u8>, stream: StreamId) -> Effect<Self> {
        tcp_write(stream, bytes).then(ConnectionMsg::Wrote)
    }

    fn idle_arm_effect(&self) -> Effect<Self> {
        sleep_then(
            self.config.idle_timeout,
            ConnectionMsg::IdleTick {
                snapshot: self.activity_seq,
            },
        )
    }

    /// Marks the connection as closing for `reason` and returns a batch that
    /// dispatches `tcp_close_stream`. If a close is already in progress this
    /// returns `noop()` so we never issue two close calls.
    fn begin_close(&mut self, reason: CloseReason) -> Effect<Self> {
        if self.closing.is_some() || self.close_dispatched {
            return noop();
        }
        self.closing = Some(reason);
        self.close_dispatched = true;
        self.reads_done = true;
        self.in_flight = 0;
        self.in_flight_ids.clear();
        self.write_queue.clear();
        self.write_in_flight = None;
        tcp_close_stream(self.stream).then(ConnectionMsg::StreamClosed)
    }

    fn finalize_close(&mut self) -> Effect<Self> {
        let reason = self.closing.take().unwrap_or(CloseReason::PeerClosed);
        match self.watcher {
            Some(watcher) => batch([send::<Self, _, _>(watcher, reason), stop()]),
            None => stop(),
        }
    }

    fn enqueue_write(&mut self, bytes: Vec<u8>) -> Result<(), BadPeerReason> {
        // Cap counts queued bytes only, not the bytes currently being written.
        // The actively-writing slot returns to the queue on partial-write
        // recovery, so the steady-state ceiling is `write_queue_cap` queued +
        // 1 in flight.
        if self.write_queue.len() >= self.config.write_queue_cap {
            return Err(BadPeerReason::WriteQueueOverflow);
        }
        self.write_queue.push_back(bytes);
        Ok(())
    }

    fn maybe_start_write(&mut self) -> Option<Effect<Self>> {
        if self.write_in_flight.is_some() {
            return None;
        }
        let bytes = self.write_queue.pop_front()?;
        self.write_in_flight = Some(bytes.clone());
        Some(Self::write_effect(bytes, self.stream))
    }

    fn encode_frame(&self, frame: &Frame) -> Option<Vec<u8>> {
        encode(frame, &self.frame_limits()).ok()
    }

    /// Encodes an `Error(Internal)` frame for `request_id`. Falls back to
    /// `None` only if even an empty-payload Internal frame cannot fit in the
    /// connection's `max_frame_size`, which would imply a misconfigured
    /// limit smaller than [`crate::frame::MAX_SERVICE_LEN`] +
    /// [`crate::frame::MAX_METHOD_LEN`] + minimum body.
    fn encode_internal_error(
        &self,
        request_id: u64,
        service: &str,
        method: &str,
    ) -> Option<Vec<u8>> {
        self.encode_frame(&Frame::error(
            request_id,
            service.to_string(),
            method.to_string(),
            FrameError::Internal,
            Vec::new(),
        ))
    }

    fn route_request(&mut self, frame: Frame) -> Effect<Self> {
        let request_id = frame.request_id;
        let service = frame.service.clone();
        let method = frame.method.clone();
        let request = RouterRequest {
            request_id,
            service: service.clone(),
            method: method.clone(),
            payload: frame.payload,
        };
        self.in_flight += 1;
        self.in_flight_ids.insert(request_id);
        call(
            self.router,
            crate::registry::RegistryMsg::Route(request),
            self.config.service_call_timeout,
        )
        .then(move |outcome| ConnectionMsg::Routed {
            request_id,
            service,
            method,
            outcome,
        })
    }

    /// Drains complete frames from the inbound buffer.
    ///
    /// Returns either the list of route effects to emit, or a [`DrainError`]
    /// the caller turns into a close. A bad-peer error mid-batch discards
    /// any frames already drained earlier in the same call: when the peer
    /// is malformed, all bets are off for the in-progress chunk.
    fn drain_inbound(&mut self) -> Result<Vec<Effect<Self>>, DrainError> {
        let mut emitted = Vec::new();
        loop {
            if self.inbound.len() < LENGTH_PREFIX_SIZE {
                return Ok(emitted);
            }
            let mut prefix = [0u8; LENGTH_PREFIX_SIZE];
            prefix.copy_from_slice(&self.inbound[..LENGTH_PREFIX_SIZE]);
            let body_len = parse_length_prefix(prefix, &self.frame_limits())
                .map_err(|e| DrainError::BadPeer(BadPeerReason::Decode(e)))?;
            // checked_add mirrors frame::decode. body_len is capped at
            // max_frame_size above, so this cannot overflow on 64-bit — keep the
            // math explicit for parity with the sibling decode path.
            let total = LENGTH_PREFIX_SIZE.checked_add(body_len).ok_or_else(|| {
                DrainError::BadPeer(BadPeerReason::Decode(DecodeError::BodyTooLarge {
                    len: body_len,
                    max: self.frame_limits().max_frame_size,
                }))
            })?;
            if self.inbound.len() < total {
                return Ok(emitted);
            }
            let body = self.inbound[LENGTH_PREFIX_SIZE..total].to_vec();
            // Slide the rest forward.
            self.inbound.drain(..total);
            let frame =
                decode_body(&body).map_err(|e| DrainError::BadPeer(BadPeerReason::Decode(e)))?;
            match frame.kind {
                FrameKind::Request => {
                    if self.in_flight_ids.contains(&frame.request_id) {
                        let duplicate = Frame::error(
                            frame.request_id,
                            frame.service,
                            frame.method,
                            FrameError::Protocol,
                            b"duplicate request_id".to_vec(),
                        );
                        let bytes = self
                            .encode_frame(&duplicate)
                            .ok_or(DrainError::LocalEncode)?;
                        self.enqueue_write(bytes).map_err(DrainError::BadPeer)?;
                    } else if self.in_flight >= self.config.max_in_flight {
                        let full = Frame::error(
                            frame.request_id,
                            frame.service,
                            frame.method,
                            FrameError::Full,
                            Vec::new(),
                        );
                        let bytes = self.encode_frame(&full).ok_or(DrainError::LocalEncode)?;
                        self.enqueue_write(bytes).map_err(DrainError::BadPeer)?;
                    } else {
                        emitted.push(self.route_request(frame));
                    }
                }
                FrameKind::Reply | FrameKind::Error => {
                    return Err(DrainError::BadPeer(BadPeerReason::UnexpectedKind(
                        frame.kind,
                    )));
                }
            }
        }
    }

    fn on_read(&mut self, result: Result<Vec<u8>, CallError>) -> Effect<Self> {
        if self.closing.is_some() || self.close_dispatched {
            return noop();
        }
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(err) => return self.begin_close(CloseReason::IoError(err)),
        };
        if bytes.is_empty() {
            // Peer half-closed.
            return self.begin_close(CloseReason::PeerClosed);
        }
        self.activity_seq = self.activity_seq.wrapping_add(1);
        self.inbound.extend_from_slice(&bytes);
        let mut effects = match self.drain_inbound() {
            Ok(effects) => effects,
            Err(DrainError::BadPeer(reason)) => {
                return self.begin_close(CloseReason::BadPeer(reason));
            }
            Err(DrainError::LocalEncode) => {
                return self.begin_close(CloseReason::LocalEncodeFailed);
            }
        };
        if let Some(write) = self.maybe_start_write() {
            effects.push(write);
        }
        if !self.reads_done {
            effects.push(self.read_effect());
        }
        Effect::Batch(effects)
    }

    fn on_wrote(&mut self, result: Result<usize, CallError>) -> Effect<Self> {
        if self.close_dispatched {
            // The runtime delivered the completion of a write that was
            // already in flight when we issued tcp_close_stream. Drop it;
            // begin_close has already cleared write_in_flight.
            return noop();
        }
        let count = match result {
            Ok(c) => c,
            Err(err) => return self.begin_close(CloseReason::IoError(err)),
        };
        let attempted = self.write_in_flight.take().unwrap_or_default();
        if count > attempted.len() {
            // Runtime bug, but defend ourselves.
            return self.begin_close(CloseReason::IoError(CallError::Io));
        }
        if !attempted.is_empty() && count == 0 {
            // Zero-progress write on a non-empty buffer would loop forever
            // if we re-queued and re-issued. Treat as a hard IO error.
            return self.begin_close(CloseReason::IoError(CallError::Io));
        }
        if count > 0 {
            self.activity_seq = self.activity_seq.wrapping_add(1);
        }
        if count < attempted.len() {
            // Partial write; re-queue the unwritten tail at the front.
            let tail = attempted[count..].to_vec();
            self.write_queue.push_front(tail);
        }
        match self.maybe_start_write() {
            Some(eff) => eff,
            None => noop(),
        }
    }

    fn on_routed(
        &mut self,
        request_id: u64,
        service: String,
        method: String,
        outcome: CallOutcome<RouterReply>,
    ) -> Effect<Self> {
        if self.closing.is_some() || self.close_dispatched {
            // Late reply after we've decided to close: drop it.
            return noop();
        }
        // The slot is freed regardless of outcome. We cap saturating_sub even
        // though in_flight should always be >= 1 here.
        self.in_flight = self.in_flight.saturating_sub(1);
        self.in_flight_ids.remove(&request_id);
        let frame_to_send: Option<Frame> = match outcome {
            CallOutcome::Replied(RouterReply::Ok(payload)) => {
                Some(Frame::reply(request_id, service, method, payload))
            }
            CallOutcome::Replied(RouterReply::UnknownService) => Some(Frame::error(
                request_id,
                service,
                method,
                FrameError::UnknownService,
                Vec::new(),
            )),
            CallOutcome::Replied(RouterReply::UnknownMethod) => Some(Frame::error(
                request_id,
                service,
                method,
                FrameError::UnknownMethod,
                Vec::new(),
            )),
            CallOutcome::Replied(RouterReply::Decode) => Some(Frame::error(
                request_id,
                service,
                method,
                FrameError::Decode,
                Vec::new(),
            )),
            CallOutcome::Replied(RouterReply::Internal) => Some(Frame::error(
                request_id,
                service,
                method,
                FrameError::Internal,
                Vec::new(),
            )),
            CallOutcome::Replied(RouterReply::Full) => Some(Frame::error(
                request_id,
                service,
                method,
                FrameError::Full,
                Vec::new(),
            )),
            CallOutcome::Full => Some(Frame::error(
                request_id,
                service,
                method,
                FrameError::Full,
                Vec::new(),
            )),
            CallOutcome::Closed => Some(Frame::error(
                request_id,
                service,
                method,
                FrameError::Internal,
                Vec::new(),
            )),
            CallOutcome::Rejected(_) => Some(Frame::error(
                request_id,
                service,
                method,
                FrameError::Internal,
                Vec::new(),
            )),
            // Wire-error invariant: no Timeout frame. Client times out
            // locally. The slot is freed; the late router reply, if any,
            // is dropped by the runtime since the IsolateCall already
            // resolved as Timeout.
            CallOutcome::Timeout => None,
        };
        let Some(frame) = frame_to_send else {
            return noop();
        };
        // If the chosen frame doesn't fit (e.g. an Ok reply payload exceeds
        // max_frame_size), substitute a small Internal error frame so the
        // *one* request that failed gets a wire response instead of taking
        // down every concurrent request on this connection.
        let bytes = match self.encode_frame(&frame) {
            Some(bytes) => bytes,
            None => match self.encode_internal_error(request_id, &frame.service, &frame.method) {
                Some(bytes) => bytes,
                None => return self.begin_close(CloseReason::LocalEncodeFailed),
            },
        };
        if let Err(reason) = self.enqueue_write(bytes) {
            return self.begin_close(CloseReason::BadPeer(reason));
        }
        match self.maybe_start_write() {
            Some(eff) => eff,
            None => noop(),
        }
    }

    fn on_idle_tick(&mut self, snapshot: u64) -> Effect<Self> {
        if self.closing.is_some() || self.close_dispatched {
            return noop();
        }
        if snapshot == self.activity_seq {
            return self.begin_close(CloseReason::Idle);
        }
        self.idle_arm_effect()
    }

    fn on_stream_closed(&mut self, _result: Result<(), CallError>) -> Effect<Self> {
        // Either the close we issued completed, or it failed; either way the
        // stream is no longer usable. Emit watcher and stop.
        self.finalize_close()
    }

    fn on_shutdown(&mut self) -> Effect<Self> {
        self.begin_close(CloseReason::Shutdown)
    }

    fn on_begin(&mut self) -> Effect<Self> {
        if self.closing.is_some() || self.close_dispatched || self.began {
            return noop();
        }
        self.began = true;
        Effect::Batch(vec![self.read_effect(), self.idle_arm_effect()])
    }
}

// The `tina_runtime::isolate` proc macro mishandles generic self types
// (`Connection<S>`) by emitting duplicate generic-argument lists, so we
// implement [`tina::Isolate`] manually for the generic case.
impl<S> tina::Isolate for Connection<S>
where
    S: tina::Shard,
{
    type Message = ConnectionMsg;
    type Reply = ();
    type Send = Outbound<CloseReason>;
    type Spawn = std::convert::Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Io = RuntimeCall<ConnectionMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = S;

    fn handle(
        &mut self,
        msg: ConnectionMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ConnectionMsg::Begin => self.on_begin(),
            ConnectionMsg::Read(result) => self.on_read(result),
            ConnectionMsg::Wrote(result) => self.on_wrote(result),
            ConnectionMsg::StreamClosed(result) => self.on_stream_closed(result),
            ConnectionMsg::Routed {
                request_id,
                service,
                method,
                outcome,
            } => self.on_routed(request_id, service, method, outcome),
            ConnectionMsg::IdleTick { snapshot } => self.on_idle_tick(snapshot),
            ConnectionMsg::Shutdown => self.on_shutdown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tina::{IsolateId, ShardId};

    #[derive(Debug)]
    struct TestShard;

    impl tina::Shard for TestShard {
        fn id(&self) -> ShardId {
            ShardId::new(0)
        }
    }

    fn router_addr() -> Address<crate::registry::RegistryMsg, RouterReply> {
        Address::<crate::registry::RegistryMsg>::new(ShardId::new(0), IsolateId::new(1))
            .with_reply::<RouterReply>()
    }

    fn watcher_addr() -> Address<CloseReason> {
        Address::new(ShardId::new(0), IsolateId::new(2))
    }

    fn make_connection(config: ConnectionConfig) -> Connection<TestShard> {
        Connection::new(ConnectionInit {
            stream: StreamId::new(0),
            router: router_addr(),
            watcher: Some(watcher_addr()),
            config,
            _shard: std::marker::PhantomData,
        })
    }

    fn small_config() -> ConnectionConfig {
        ConnectionConfig {
            max_frame_size: 1024,
            max_in_flight: 2,
            idle_timeout: Duration::from_millis(50),
            read_chunk: 256,
            write_queue_cap: 8,
            service_call_timeout: Duration::from_millis(100),
        }
    }

    fn dispatch(
        conn: &mut Connection<TestShard>,
        msg: ConnectionMsg,
    ) -> Effect<Connection<TestShard>> {
        let mut shard = TestShard;
        let mut ctx = Context::new(&mut shard, IsolateId::new(99));
        conn.handle(msg, &mut ctx)
    }

    fn count_effect_leaves<I: tina::Isolate>(effect: &Effect<I>) -> EffectShape {
        let mut shape = EffectShape::default();
        walk(effect, &mut shape);
        shape
    }

    fn walk<I: tina::Isolate>(effect: &Effect<I>, shape: &mut EffectShape) {
        match effect {
            Effect::Noop => shape.noop += 1,
            Effect::Reply(_) => shape.reply += 1,
            Effect::Reject(_) => shape.reply += 1,
            Effect::Send(_) => shape.send += 1,
            Effect::Spawn(_) => shape.spawn += 1,
            Effect::SpawnObserved(_) => shape.spawn_observed += 1,
            Effect::SpawnObservedOn(_) => shape.spawn_observed += 1,
            Effect::Stop => shape.stop += 1,
            Effect::Fail => shape.stop += 1,
            Effect::StopWith(_) => shape.stop_with += 1,
            Effect::RestartChildren => shape.restart += 1,
            Effect::StopChildren => shape.restart += 1,
            Effect::Io(_) => shape.call += 1,
            Effect::Batch(items) => {
                for item in items {
                    walk(item, shape);
                }
            }
            Effect::ReplyTo(_, _) => shape.reply_to += 1,
            Effect::Fact(_) => shape.fact += 1,
        }
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct EffectShape {
        noop: usize,
        reply: usize,
        send: usize,
        spawn: usize,
        spawn_observed: usize,
        stop: usize,
        stop_with: usize,
        restart: usize,
        call: usize,
        reply_to: usize,
        fact: usize,
    }

    fn build_request(request_id: u64, service: &str, method: &str, payload: &[u8]) -> Vec<u8> {
        let frame = Frame::request(request_id, service, method, payload.to_vec());
        encode(&frame, &FrameLimits::new(1024)).expect("encode test request")
    }

    #[test]
    fn begin_emits_read_and_idle_arm() {
        let mut conn = make_connection(small_config());
        let effect = dispatch(&mut conn, ConnectionMsg::Begin);
        let shape = count_effect_leaves(&effect);
        // tcp_read + sleep_then both come back as Effect::Io.
        assert_eq!(shape.call, 2, "begin should emit two runtime calls");
        assert_eq!(conn.in_flight, 0);
        assert!(conn.closing.is_none());
    }

    #[test]
    fn begin_is_idempotent() {
        // A duplicate Begin must not issue a second tcp_read on the
        // same stream; the runtime would reject it as ResourceBusy and
        // the connection would close as IoError. Bootstrap/retry
        // wiring would otherwise hit this path.
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        assert!(conn.began);
        let second = dispatch(&mut conn, ConnectionMsg::Begin);
        assert!(
            matches!(second, Effect::Noop),
            "duplicate Begin must be a noop"
        );
        assert!(conn.closing.is_none(), "duplicate Begin must not close");
    }

    #[test]
    fn read_request_routes_to_router() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let bytes = build_request(7, "billing", "charge", b"{}");
        let effect = dispatch(&mut conn, ConnectionMsg::Read(Ok(bytes)));
        let shape = count_effect_leaves(&effect);
        // One isolate-call (route) + one tcp_read (re-arm). No write yet (waiting for routed).
        assert_eq!(shape.call, 2, "should route + reissue read");
        assert_eq!(conn.in_flight, 1);
        assert_eq!(conn.activity_seq, 1);
    }

    #[test]
    fn empty_read_closes_with_peer_closed() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let effect = dispatch(&mut conn, ConnectionMsg::Read(Ok(Vec::new())));
        // Effect should be tcp_close_stream (one Call).
        let shape = count_effect_leaves(&effect);
        assert_eq!(shape.call, 1);
        assert_eq!(conn.closing.as_ref(), Some(&CloseReason::PeerClosed));
        assert!(conn.close_dispatched);
    }

    #[test]
    fn read_io_error_closes_with_io_error() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let effect = dispatch(&mut conn, ConnectionMsg::Read(Err(CallError::Io)));
        let shape = count_effect_leaves(&effect);
        assert_eq!(shape.call, 1);
        assert_eq!(
            conn.closing.as_ref(),
            Some(&CloseReason::IoError(CallError::Io))
        );
    }

    #[test]
    fn unsupported_version_closes_bad_peer() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        // Hand-craft a frame body with version 99 instead of 1.
        let mut body = vec![99u8, 0u8, 0u8]; // version=99, kind=Request, error_code=0
        body.extend_from_slice(&0u64.to_be_bytes());
        body.push(0); // service_len
        body.push(0); // method_len
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend(body);
        let _effect = dispatch(&mut conn, ConnectionMsg::Read(Ok(bytes)));
        match conn.closing.as_ref() {
            Some(CloseReason::BadPeer(BadPeerReason::Decode(DecodeError::UnsupportedVersion(
                99,
            )))) => {}
            other => panic!("expected BadPeer(Decode(UnsupportedVersion)), got {other:?}"),
        }
    }

    #[test]
    fn body_too_large_closes_bad_peer() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        // Length prefix declares body > max_frame_size (1024); we use 5000.
        let prefix = (5000u32).to_be_bytes().to_vec();
        let _effect = dispatch(&mut conn, ConnectionMsg::Read(Ok(prefix)));
        match conn.closing.as_ref() {
            Some(CloseReason::BadPeer(BadPeerReason::Decode(DecodeError::BodyTooLarge {
                ..
            }))) => {}
            other => panic!("expected BadPeer(Decode(BodyTooLarge)), got {other:?}"),
        }
    }

    #[test]
    fn reply_kind_from_peer_closes_bad_peer() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        // Server should not receive Reply frames; this is a peer doing the wrong thing.
        let frame = Frame::reply(1, "svc", "m", Vec::new());
        let bytes = encode(&frame, &FrameLimits::new(1024)).unwrap();
        let _effect = dispatch(&mut conn, ConnectionMsg::Read(Ok(bytes)));
        assert!(matches!(
            conn.closing.as_ref(),
            Some(CloseReason::BadPeer(BadPeerReason::UnexpectedKind(
                FrameKind::Reply
            )))
        ));
    }

    #[test]
    fn in_flight_cap_replies_full() {
        let mut config = small_config();
        config.max_in_flight = 1;
        let mut conn = make_connection(config);
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        // First request reserves the only slot.
        let r1 = build_request(1, "s", "m", &[]);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r1)));
        assert_eq!(conn.in_flight, 1);
        // Second request: cap is full → should enqueue a Full error frame.
        let r2 = build_request(2, "s", "m", &[]);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r2)));
        assert_eq!(conn.in_flight, 1);
        // Either the write started (write_in_flight set, queue empty) or it's queued.
        assert!(conn.write_in_flight.is_some() || !conn.write_queue.is_empty());
    }

    #[test]
    fn duplicate_request_id_while_in_flight_returns_protocol_error_without_dispatch() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let first = build_request(11, "svc", "m", b"first");
        let effect = dispatch(&mut conn, ConnectionMsg::Read(Ok(first)));
        assert_eq!(count_effect_leaves(&effect).call, 2, "route + next read");
        assert_eq!(conn.in_flight, 1);
        assert!(conn.in_flight_ids.contains(&11));

        let duplicate = build_request(11, "svc", "m", b"second");
        let effect = dispatch(&mut conn, ConnectionMsg::Read(Ok(duplicate)));
        assert_eq!(
            count_effect_leaves(&effect).call,
            2,
            "duplicate should write protocol error + continue read, not route service again"
        );
        assert_eq!(conn.in_flight, 1);
        let pending = conn
            .write_in_flight
            .as_ref()
            .or_else(|| conn.write_queue.front())
            .expect("protocol error frame should be queued or writing");
        let frame = crate::frame::decode(pending, &FrameLimits::new(1024)).unwrap();
        assert_eq!(frame.kind, FrameKind::Error);
        assert_eq!(frame.error, Some(FrameError::Protocol));
        assert_eq!(frame.request_id, 11);
    }

    #[test]
    fn routed_ok_enqueues_reply_frame() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let r = build_request(42, "svc", "m", b"in");
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r)));
        assert_eq!(conn.in_flight, 1);
        let _ = dispatch(
            &mut conn,
            ConnectionMsg::Routed {
                request_id: 42,
                service: "svc".into(),
                method: "m".into(),
                outcome: CallOutcome::Replied(RouterReply::Ok(b"out".to_vec())),
            },
        );
        assert_eq!(conn.in_flight, 0);
        assert!(conn.write_in_flight.is_some(), "write should have started");
    }

    #[test]
    fn routed_timeout_emits_no_wire_frame() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let r = build_request(7, "svc", "m", &[]);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r)));
        assert_eq!(conn.in_flight, 1);
        let _ = dispatch(
            &mut conn,
            ConnectionMsg::Routed {
                request_id: 7,
                service: "svc".into(),
                method: "m".into(),
                outcome: CallOutcome::Timeout,
            },
        );
        // Slot freed, but no write should have been generated.
        assert_eq!(conn.in_flight, 0);
        assert!(conn.write_in_flight.is_none());
        assert!(conn.write_queue.is_empty());
    }

    #[test]
    fn routed_full_writes_full_frame() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let r = build_request(5, "svc", "m", &[]);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r)));
        let _ = dispatch(
            &mut conn,
            ConnectionMsg::Routed {
                request_id: 5,
                service: "svc".into(),
                method: "m".into(),
                outcome: CallOutcome::Full,
            },
        );
        let pending = conn.write_in_flight.as_ref().expect("write started");
        let frame = crate::frame::decode(pending, &FrameLimits::new(1024)).unwrap();
        assert_eq!(frame.kind, FrameKind::Error);
        assert_eq!(frame.error, Some(FrameError::Full));
        assert_eq!(frame.request_id, 5);
    }

    #[test]
    fn routed_router_reply_full_writes_full_frame() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let r = build_request(8, "svc", "m", &[]);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r)));
        let _ = dispatch(
            &mut conn,
            ConnectionMsg::Routed {
                request_id: 8,
                service: "svc".into(),
                method: "m".into(),
                outcome: CallOutcome::Replied(RouterReply::Full),
            },
        );
        let pending = conn.write_in_flight.as_ref().expect("write started");
        let frame = crate::frame::decode(pending, &FrameLimits::new(1024)).unwrap();
        assert_eq!(frame.kind, FrameKind::Error);
        assert_eq!(frame.error, Some(FrameError::Full));
    }

    #[test]
    fn routed_closed_writes_internal_frame() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let r = build_request(6, "svc", "m", &[]);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r)));
        let _ = dispatch(
            &mut conn,
            ConnectionMsg::Routed {
                request_id: 6,
                service: "svc".into(),
                method: "m".into(),
                outcome: CallOutcome::Closed,
            },
        );
        let pending = conn.write_in_flight.as_ref().expect("write started");
        let frame = crate::frame::decode(pending, &FrameLimits::new(1024)).unwrap();
        assert_eq!(frame.error, Some(FrameError::Internal));
    }

    #[test]
    fn partial_write_requeues_tail() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let r = build_request(1, "s", "m", b"hi");
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r)));
        let _ = dispatch(
            &mut conn,
            ConnectionMsg::Routed {
                request_id: 1,
                service: "s".into(),
                method: "m".into(),
                outcome: CallOutcome::Replied(RouterReply::Ok(b"long-reply-body".to_vec())),
            },
        );
        let attempted_len = conn.write_in_flight.as_ref().unwrap().len();
        // Pretend only 4 bytes were accepted by the runtime.
        let _ = dispatch(&mut conn, ConnectionMsg::Wrote(Ok(4)));
        // A new write should have started for the tail.
        assert!(conn.write_in_flight.is_some());
        assert_eq!(
            conn.write_in_flight.as_ref().unwrap().len(),
            attempted_len - 4
        );
    }

    #[test]
    fn idle_tick_with_no_activity_closes_idle() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        // No reads/writes happened; activity_seq is still 0.
        let _ = dispatch(&mut conn, ConnectionMsg::IdleTick { snapshot: 0 });
        assert_eq!(conn.closing.as_ref(), Some(&CloseReason::Idle));
    }

    #[test]
    fn idle_tick_with_activity_rearms() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        // Bump activity by reading something.
        let r = build_request(1, "s", "m", &[]);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r)));
        assert!(conn.activity_seq > 0);
        let _ = dispatch(&mut conn, ConnectionMsg::IdleTick { snapshot: 0 });
        // Should not have closed.
        assert!(conn.closing.is_none());
    }

    #[test]
    fn shutdown_closes_visibly() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let _ = dispatch(&mut conn, ConnectionMsg::Shutdown);
        assert_eq!(conn.closing.as_ref(), Some(&CloseReason::Shutdown));
        assert!(conn.close_dispatched);
    }

    #[test]
    fn stream_closed_emits_watcher_send_and_stop() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let _ = dispatch(&mut conn, ConnectionMsg::Shutdown);
        let effect = dispatch(&mut conn, ConnectionMsg::StreamClosed(Ok(())));
        let shape = count_effect_leaves(&effect);
        assert_eq!(shape.send, 1, "watcher receives one CloseReason");
        assert_eq!(shape.stop, 1, "isolate stops");
    }

    #[test]
    fn stream_closed_without_watcher_just_stops() {
        let mut conn = Connection::<TestShard>::new(ConnectionInit {
            stream: StreamId::new(0),
            router: router_addr(),
            watcher: None,
            config: small_config(),
            _shard: std::marker::PhantomData,
        });
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let _ = dispatch(&mut conn, ConnectionMsg::Shutdown);
        let effect = dispatch(&mut conn, ConnectionMsg::StreamClosed(Ok(())));
        let shape = count_effect_leaves(&effect);
        assert_eq!(shape.send, 0);
        assert_eq!(shape.stop, 1);
    }

    #[test]
    fn write_queue_overflow_closes_bad_peer() {
        let mut config = small_config();
        // Lower the cap so we can fill it within the in-flight limit.
        config.max_in_flight = 2;
        config.write_queue_cap = 2;
        let mut conn = make_connection(config);
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        // Saturate in-flight first.
        let _ = dispatch(
            &mut conn,
            ConnectionMsg::Read(Ok(build_request(1, "s", "m", &[]))),
        );
        let _ = dispatch(
            &mut conn,
            ConnectionMsg::Read(Ok(build_request(2, "s", "m", &[]))),
        );
        assert_eq!(conn.in_flight, 2);
        // Now keep sending requests beyond the cap; each one queues a Full
        // error frame. After 2 such errors the write queue is full
        // (assuming the first one was started immediately and the second
        // queued).
        for rid in 3..=10u64 {
            let _ = dispatch(
                &mut conn,
                ConnectionMsg::Read(Ok(build_request(rid, "s", "m", &[]))),
            );
            if conn.closing.is_some() {
                break;
            }
        }
        match conn.closing.as_ref() {
            Some(CloseReason::BadPeer(BadPeerReason::WriteQueueOverflow)) => {}
            other => panic!("expected WriteQueueOverflow close, got {other:?}"),
        }
    }

    #[test]
    fn fragmented_frames_assemble_across_reads() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let bytes = build_request(11, "svc", "method", b"payload");
        let (head, tail) = bytes.split_at(5);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(head.to_vec())));
        // First chunk is shorter than the frame; nothing should have routed yet.
        assert_eq!(conn.in_flight, 0);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(tail.to_vec())));
        assert_eq!(conn.in_flight, 1, "frame should route after second chunk");
    }

    #[test]
    fn zero_progress_write_closes_io_error() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let r = build_request(1, "s", "m", &[]);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r)));
        let _ = dispatch(
            &mut conn,
            ConnectionMsg::Routed {
                request_id: 1,
                service: "s".into(),
                method: "m".into(),
                outcome: CallOutcome::Replied(RouterReply::Ok(b"reply".to_vec())),
            },
        );
        assert!(conn.write_in_flight.is_some());
        let _ = dispatch(&mut conn, ConnectionMsg::Wrote(Ok(0)));
        assert_eq!(
            conn.closing.as_ref(),
            Some(&CloseReason::IoError(CallError::Io)),
            "zero-progress write must not loop"
        );
    }

    #[test]
    fn oversize_reply_falls_back_to_internal_error() {
        let mut config = small_config();
        // Tight limit: 13 (min body) + 3 (service) + 1 (method) + N (payload).
        // Set max_frame_size = 32 so a 30-byte Ok payload doesn't fit but a
        // small Internal error frame does.
        config.max_frame_size = 32;
        let mut conn = make_connection(config);
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let r = build_request(1, "svc", "m", &[]);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r)));
        let big = vec![b'x'; 100];
        let _ = dispatch(
            &mut conn,
            ConnectionMsg::Routed {
                request_id: 1,
                service: "svc".into(),
                method: "m".into(),
                outcome: CallOutcome::Replied(RouterReply::Ok(big)),
            },
        );
        // Should not have closed; should have substituted Internal-error frame.
        assert!(
            conn.closing.is_none(),
            "oversize reply must not tear down connection"
        );
        let pending = conn.write_in_flight.as_ref().expect("write started");
        let frame = crate::frame::decode(pending, &FrameLimits::new(32)).unwrap();
        assert_eq!(frame.kind, FrameKind::Error);
        assert_eq!(frame.error, Some(FrameError::Internal));
        assert_eq!(frame.request_id, 1);
    }

    #[test]
    fn late_reply_after_close_is_dropped() {
        let mut conn = make_connection(small_config());
        let _ = dispatch(&mut conn, ConnectionMsg::Begin);
        let r = build_request(99, "s", "m", &[]);
        let _ = dispatch(&mut conn, ConnectionMsg::Read(Ok(r)));
        let _ = dispatch(&mut conn, ConnectionMsg::Shutdown);
        // After shutdown, a late routed reply must not write a frame or panic.
        let _ = dispatch(
            &mut conn,
            ConnectionMsg::Routed {
                request_id: 99,
                service: "s".into(),
                method: "m".into(),
                outcome: CallOutcome::Replied(RouterReply::Ok(b"late".to_vec())),
            },
        );
        // No new write should have started after shutdown.
        assert!(conn.write_in_flight.is_none());
        assert!(conn.write_queue.is_empty());
    }
}
