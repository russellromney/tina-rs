//! Client stub (Rock 4).
//!
//! One client isolate owns one outbound TCP stream to a server. The user
//! issues requests by sending [`ClientMsg::Request`]; the client encodes
//! frames, multiplexes a bounded in-flight map, matches replies by wire
//! request id, and delivers each [`ClientResultMsg`] to the user-supplied
//! `reply_to` address.
//!
//! # Why a per-request reply address (not `IsolateCall`)
//!
//! Tina's `IsolateCall` continuation chain pairs one `call_context` with
//! one synchronous return value, but the client is multiplexed: many user
//! requests sit in flight simultaneously, replies arrive out of order, and
//! a single `tcp_read` may serve any of the pending requests. Riding one
//! call_context through the read loop would couple every request's reply
//! to one request's pending slot. Instead, each [`ClientMsg::Request`]
//! carries a typed [`Address<ClientResultMsg>`] the client `tina::send`s
//! to when the matching reply (or local timeout, or close) is ready.
//!
//! # Wire-error invariant (client side)
//!
//! Client-observed conditions never appear as wire frames; they appear as
//! local [`ClientResult`] variants delivered to `reply_to`:
//!
//! | Outcome                     | `ClientResult`                |
//! |-----------------------------|-------------------------------|
//! | Server `Reply` frame        | `Ok(payload)`                 |
//! | Server `Error(code)` frame  | `ServerError(code)`           |
//! | Local deadline elapsed      | `Timeout`                     |
//! | Connection closed by peer   | `ConnectionClosed`            |
//! | Client idle timeout         | `Idle`                        |
//! | In-flight cap exceeded      | `Full`                        |
//! | Outgoing frame > max size   | `LocalEncodeFailed`           |
//! | Local IO error              | `IoError(CallError)`          |
//!
//! On any close path (peer disconnect, idle, IO error, shutdown) every
//! pending request gets a notification before the isolate stops.
//!
//! # Bounded everywhere
//!
//! - One `tcp_read` in flight at a time.
//! - In-flight map capped at `max_in_flight`. Overflow returns `Full`
//!   immediately, no queuing, no blocking.
//! - Write queue capped at `write_queue_cap`. Overflow closes the
//!   connection.
//! - Inbound buffer naturally bounded by `max_frame_size + read_chunk`.
//!
//! # No cancellation frame
//!
//! On local timeout the client drops the in-flight slot and notifies the
//! caller; it does not send a cancel frame to the server. The server
//! will complete the request on its own; the late reply, if any, is
//! discarded by the client (slot already gone).

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, RuntimeCall, StreamId, sleep_then, tcp_close_stream, tcp_connect, tcp_read,
    tcp_write,
};

use crate::frame::{
    DecodeError, Frame, FrameError, FrameKind, FrameLimits, LENGTH_PREFIX_SIZE, decode_body,
    encode, parse_length_prefix,
};

/// Configuration for [`Client`].
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Maximum body size for any frame. Mirrors
    /// [`FrameLimits::max_frame_size`].
    pub max_frame_size: usize,
    /// Maximum simultaneous in-flight requests. Over-cap requests are
    /// rejected with [`ClientResult::Full`] immediately.
    pub max_in_flight: usize,
    /// Idle timeout. If no read/write activity for this window, the
    /// connection closes; every pending request receives
    /// [`ClientResult::Idle`].
    ///
    /// The actual close window is `[idle_timeout, 2 × idle_timeout)` after
    /// the last activity, because the timer is re-armed against an
    /// activity-sequence snapshot rather than wall-clock time.
    pub idle_timeout: Duration,
    /// Maximum bytes per `TcpRead`.
    pub read_chunk: usize,
    /// Maximum number of encoded request frames buffered awaiting write.
    pub write_queue_cap: usize,
}

impl Default for ClientConfig {
    /// Conservative first-form defaults: 1 MiB frames, 64 in-flight, 30 s
    /// idle, 8 KiB read chunks, 256 frame write queue.
    fn default() -> Self {
        Self {
            max_frame_size: 1024 * 1024,
            max_in_flight: 64,
            idle_timeout: Duration::from_secs(30),
            read_chunk: 8 * 1024,
            write_queue_cap: 256,
        }
    }
}

/// User-side outcome for one RPC.
///
/// On any connection-wide close path (peer disconnect, server protocol
/// fault, idle timeout, IO error, shutdown) every pending request is
/// notified with the corresponding variant; this is fan-out, not
/// per-request isolation. A buggy server that emits undecodable bytes
/// closes the connection and surfaces as [`ClientResult::ConnectionClosed`]
/// for every pending request — first-form does not distinguish "clean
/// half-close" from "server emitted garbage."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientResult {
    /// Server returned a successful Reply frame.
    Ok(Vec<u8>),
    /// Server returned an Error frame with this code.
    ServerError(FrameError),
    /// Local deadline elapsed before any wire reply.
    Timeout,
    /// The connection closed before a reply arrived. Includes the
    /// "server sent undecodable bytes" case in first form; these are
    /// not separated.
    ConnectionClosed,
    /// Client idle timeout closed the connection.
    Idle,
    /// In-flight cap was at capacity when the request was issued.
    Full,
    /// The request frame could not be encoded under the configured
    /// `max_frame_size`. Server-side limits are not consulted; this is a
    /// purely local size-check failure.
    LocalEncodeFailed,
    /// A local IO error closed the connection. Every pending request is
    /// notified with this variant when the underlying TCP read or write
    /// fails; the failure is connection-wide, not per-request.
    IoError(CallError),
}

/// Result envelope delivered back to the caller's `reply_to` address.
#[derive(Debug, Clone)]
pub struct ClientResultMsg {
    /// The user-supplied tag from [`ClientRequest::correlator`].
    pub correlator: u64,
    /// The outcome of the call.
    pub result: ClientResult,
}

/// One outbound RPC.
#[derive(Debug, Clone)]
pub struct ClientRequest {
    /// Service name.
    pub service: String,
    /// Method name.
    pub method: String,
    /// Opaque request payload bytes.
    pub payload: Vec<u8>,
    /// Local deadline. The client emits a [`ClientResult::Timeout`]
    /// notification when this elapses with no matching reply.
    ///
    /// The deadline is armed when [`ClientMsg::Request`] is delivered to
    /// the client, **including** any time the request waits for a
    /// pending `tcp_connect` to complete. A request submitted before
    /// `Connected` therefore loses some of its deadline budget to the
    /// connect handshake. Wait for `Connected` (e.g. by sending `Begin`
    /// first and only then submitting requests) if you need the deadline
    /// to cover only the wire round-trip.
    pub deadline: Duration,
    /// User-supplied tag; echoed in the reply for the user's correlation.
    pub correlator: u64,
    /// Where the eventual [`ClientResultMsg`] should be delivered.
    pub reply_to: Address<ClientResultMsg>,
}

/// Inbound vocabulary for [`Client`].
///
/// External callers send [`ClientMsg::Begin`], [`ClientMsg::Request`], and
/// [`ClientMsg::Shutdown`]. The remaining variants are runtime-driven and
/// are public so tests can simulate the runtime.
#[derive(Debug, Clone)]
pub enum ClientMsg {
    /// Initial kick-off. If [`ClientStream`] was constructed with
    /// [`ClientStream::Established`], this arms the read loop and idle
    /// timer immediately. If [`ClientStream::Pending`], `Begin` issues
    /// the `tcp_connect` and the read/idle arm runs after `Connected`.
    Begin,
    /// Result of the `tcp_connect` issued in response to `Begin` for a
    /// [`ClientStream::Pending`] client.
    Connected(Result<StreamId, CallError>),
    /// User-issued RPC.
    Request(ClientRequest),
    /// Result of an outstanding `tcp_read`.
    Read(Result<Vec<u8>, CallError>),
    /// Result of an outstanding `tcp_write`.
    Wrote(Result<usize, CallError>),
    /// Result of `tcp_close_stream` after a close was issued.
    StreamClosed(Result<(), CallError>),
    /// Per-request deadline timer fired.
    DeadlineFired {
        /// Wire request id this deadline guards.
        request_id: u64,
    },
    /// Idle-timer tick. See `Connection`'s same-named variant.
    IdleTick {
        /// Activity sequence captured when the timer was armed.
        snapshot: u64,
    },
    /// External shutdown request. All pending requests are notified with
    /// `ConnectionClosed` and the connection closes cleanly.
    Shutdown,
}

/// How the client should obtain its TCP stream.
#[derive(Debug, Clone, Copy)]
pub enum ClientStream {
    /// The caller already performed `tcp_connect`; reuse this stream id.
    Established(StreamId),
    /// Issue `tcp_connect` to this address on `Begin`. The read loop
    /// arms after the connect completes.
    Pending(SocketAddr),
}

/// Initialization data for [`Client::new`].
///
/// Use the [`ClientInit::connect`] / [`ClientInit::with_stream`]
/// builders rather than constructing the struct directly:
///
/// ```ignore
/// // Dial on Begin:
/// ClientInit::<MyShard>::connect("127.0.0.1:9000".parse().unwrap())
///     .with_config(ClientConfig::default())
///
/// // Or reuse an already-established stream:
/// ClientInit::<MyShard>::with_stream(stream_id)
/// ```
pub struct ClientInit<S>
where
    S: tina::Shard,
{
    /// Source of the client's TCP stream.
    pub stream: ClientStream,
    /// Connection-local configuration.
    pub config: ClientConfig,
    /// Marker so the type carries the shard parameter.
    #[doc(hidden)]
    pub _shard: std::marker::PhantomData<S>,
}

impl<S> ClientInit<S>
where
    S: tina::Shard,
{
    /// Builds an initializer that dials `addr` on `Begin`.
    pub fn connect(addr: SocketAddr) -> Self {
        Self {
            stream: ClientStream::Pending(addr),
            config: ClientConfig::default(),
            _shard: std::marker::PhantomData,
        }
    }

    /// Builds an initializer reusing an already-established stream.
    pub fn with_stream(stream: StreamId) -> Self {
        Self {
            stream: ClientStream::Established(stream),
            config: ClientConfig::default(),
            _shard: std::marker::PhantomData,
        }
    }

    /// Replaces the configuration.
    pub fn with_config(mut self, config: ClientConfig) -> Self {
        self.config = config;
        self
    }
}

impl<S> std::fmt::Debug for ClientInit<S>
where
    S: tina::Shard,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientInit")
            .field("stream", &self.stream)
            .field("config", &self.config)
            .finish()
    }
}

#[derive(Debug)]
struct InFlight {
    correlator: u64,
    reply_to: Address<ClientResultMsg>,
}

/// Client isolate. One instance per outbound TCP connection.
pub struct Client<S>
where
    S: tina::Shard,
{
    /// Stream once established. `None` means a `tcp_connect` is pending
    /// (Pending init mode, before `Connected` arrives).
    stream: Option<StreamId>,
    /// Address to dial on `Begin`, recorded only when init mode was
    /// `Pending`. Stays populated until `Connected` so a duplicate
    /// `Begin` can no-op rather than tearing the connection down.
    pending_connect: Option<SocketAddr>,
    /// Set once `Begin` issued the `tcp_connect`; prevents a second
    /// `Begin` from re-dispatching or from tripping the
    /// no-stream-and-no-pending-connect close branch.
    connect_dispatched: bool,
    config: ClientConfig,
    inbound: Vec<u8>,
    in_flight: HashMap<u64, InFlight>,
    next_request_id: u64,
    /// Queued outbound frames. Each entry pairs the wire `request_id`
    /// the frame belongs to (if known) with the bytes. `None` means
    /// "this is the unwritten tail of a partial write" — those bytes
    /// have already been partially put on the wire and cannot be
    /// cancelled.
    write_queue: VecDeque<(Option<u64>, Vec<u8>)>,
    write_in_flight: Option<Vec<u8>>,
    activity_seq: u64,
    closing: Option<ClientResult>,
    close_dispatched: bool,
    reads_done: bool,
    read_started: bool,
    _shard: std::marker::PhantomData<S>,
}

impl<S> std::fmt::Debug for Client<S>
where
    S: tina::Shard,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("stream", &self.stream)
            .field("pending_connect", &self.pending_connect)
            .field("connect_dispatched", &self.connect_dispatched)
            .field("in_flight", &self.in_flight.len())
            .field("next_request_id", &self.next_request_id)
            .field("write_queue", &self.write_queue.len())
            .field("write_in_flight", &self.write_in_flight.is_some())
            .field("activity_seq", &self.activity_seq)
            .field("closing", &self.closing)
            .field("close_dispatched", &self.close_dispatched)
            .field("reads_done", &self.reads_done)
            .field("read_started", &self.read_started)
            .finish()
    }
}

impl<S> Client<S>
where
    S: tina::Shard,
{
    /// Builds a new client isolate from initialization data. The TCP
    /// stream may either be already established (caller did the
    /// `tcp_connect` themselves) or [`ClientStream::Pending`], in which
    /// case `Begin` will dispatch the connect.
    ///
    /// # Required configuration invariants (debug-asserted)
    ///
    /// - `max_in_flight > 0`
    /// - `write_queue_cap >= max_in_flight`. With the inverse, a normal
    ///   request rate could fill the write queue while the in-flight map
    ///   has slack, forcing the connection into the
    ///   `IoError(CallError::Io)` close branch under non-overload
    ///   conditions.
    /// - `read_chunk > 0`. A zero-length `tcp_read` returns
    ///   `Ok(empty)`, which the client interprets as peer half-close
    ///   and tears the connection down. Release builds also clamp the
    ///   read path to `read_chunk.max(1)` to keep this from being a
    ///   silent EOF if the assert is bypassed.
    /// - `max_frame_size >= MIN_BODY_SIZE` (enforced separately by the
    ///   frame codec).
    pub fn new(init: ClientInit<S>) -> Self {
        debug_assert!(
            init.config.max_in_flight > 0,
            "max_in_flight must be positive"
        );
        debug_assert!(
            init.config.write_queue_cap >= init.config.max_in_flight,
            "write_queue_cap ({}) must be >= max_in_flight ({})",
            init.config.write_queue_cap,
            init.config.max_in_flight,
        );
        debug_assert!(
            init.config.read_chunk > 0,
            "ClientConfig::read_chunk must be > 0 (a zero read returns empty bytes \
             which the client treats as peer EOF)",
        );
        let (stream, pending_connect) = match init.stream {
            ClientStream::Established(id) => (Some(id), None),
            ClientStream::Pending(addr) => (None, Some(addr)),
        };
        Self {
            stream,
            pending_connect,
            connect_dispatched: false,
            config: init.config,
            inbound: Vec::new(),
            in_flight: HashMap::new(),
            next_request_id: 1,
            write_queue: VecDeque::new(),
            write_in_flight: None,
            activity_seq: 0,
            closing: None,
            close_dispatched: false,
            reads_done: false,
            read_started: false,
            _shard: std::marker::PhantomData,
        }
    }
}

impl<S> Client<S>
where
    S: tina::Shard,
    Self: tina::Isolate<
            Message = ClientMsg,
            Send = Outbound<ClientResultMsg>,
            Call = RuntimeCall<ClientMsg>,
        >,
{
    fn frame_limits(&self) -> FrameLimits {
        FrameLimits::new(self.config.max_frame_size)
    }

    /// Returns the established stream id, or `None` if `tcp_connect` has
    /// not completed yet.
    fn established_stream(&self) -> Option<StreamId> {
        self.stream
    }

    fn read_effect(&self) -> Effect<Self> {
        let stream = self
            .established_stream()
            .expect("read_effect called before stream established");
        // Clamp to >= 1; see Connection::read_effect for rationale (a
        // zero-length read is interpreted as peer EOF).
        let max_len = self.config.read_chunk.max(1);
        tcp_read(stream, max_len).reply(ClientMsg::Read)
    }

    fn write_effect(bytes: Vec<u8>, stream: StreamId) -> Effect<Self> {
        tcp_write(stream, bytes).reply(ClientMsg::Wrote)
    }

    fn idle_arm_effect(&self) -> Effect<Self> {
        sleep_then(
            self.config.idle_timeout,
            ClientMsg::IdleTick {
                snapshot: self.activity_seq,
            },
        )
    }

    fn deadline_arm_effect(&self, request_id: u64, deadline: Duration) -> Effect<Self> {
        sleep_then(deadline, ClientMsg::DeadlineFired { request_id })
    }

    fn is_closing(&self) -> bool {
        self.closing.is_some() || self.close_dispatched
    }

    fn maybe_start_write(&mut self) -> Option<Effect<Self>> {
        if self.write_in_flight.is_some() {
            return None;
        }
        // Stream not yet established (Pending init mode, before Connected).
        // Leave the bytes queued; on_connected starts the write.
        let stream = self.established_stream()?;
        let (_request_id, bytes) = self.write_queue.pop_front()?;
        self.write_in_flight = Some(bytes.clone());
        Some(Self::write_effect(bytes, stream))
    }

    /// Notifies an in-flight entry with `result`.
    fn notify(entry: InFlight, result: ClientResult) -> Effect<Self> {
        Self::notify_caller(entry.reply_to, entry.correlator, result)
    }

    /// Notifies an arbitrary caller with `result`. Used both for in-flight
    /// completion and for synchronous rejections (Full / LocalEncodeFailed
    /// / ConnectionClosed-after-close) where no `InFlight` slot exists.
    fn notify_caller(
        reply_to: Address<ClientResultMsg>,
        correlator: u64,
        result: ClientResult,
    ) -> Effect<Self> {
        send::<Self, _, _>(reply_to, ClientResultMsg { correlator, result })
    }

    /// Picks the next free `request_id`, probing forward on collision.
    /// Returns `None` if every slot in the in-flight map is currently
    /// occupied (which by construction means the in-flight cap has been
    /// reached, since the map cannot grow past that bound).
    fn allocate_request_id(&mut self) -> Option<u64> {
        // The cap check at the call site rules out a saturated map, but
        // leave defensive bookkeeping in case capacity invariants ever
        // shift. Probe up to `max_in_flight + 1` ids so we cannot loop
        // forever on a corrupted state.
        let limit = self.config.max_in_flight.saturating_add(1);
        for _ in 0..=limit {
            let candidate = self.next_request_id;
            // Wrapping arithmetic; skip 0 because tests and humans both
            // expect the first id to be 1.
            self.next_request_id = self.next_request_id.wrapping_add(1);
            if self.next_request_id == 0 {
                self.next_request_id = 1;
            }
            if !self.in_flight.contains_key(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Begins the close path with `reason`. Notifies every pending
    /// in-flight request with `reason` first so each user gets one
    /// final notification, then dispatches `tcp_close_stream`.
    fn begin_close(&mut self, reason: ClientResult) -> Effect<Self> {
        if self.close_dispatched {
            return noop();
        }
        let mut effects: Vec<Effect<Self>> = Vec::new();
        for (_, entry) in self.in_flight.drain() {
            effects.push(Self::notify(entry, reason.clone()));
        }
        self.closing = Some(reason);
        self.close_dispatched = true;
        self.reads_done = true;
        self.write_queue.clear();
        self.write_in_flight = None;
        // If the stream was never established (connect failed), there is
        // no resource to close at the runtime; finalize synchronously.
        match self.established_stream() {
            Some(stream) => {
                effects.push(tcp_close_stream(stream).reply(ClientMsg::StreamClosed));
                Effect::Batch(effects)
            }
            None => {
                effects.push(stop());
                Effect::Batch(effects)
            }
        }
    }

    fn finalize_close(&mut self) -> Effect<Self> {
        // The close completion arrives after begin_close has already
        // notified every pending caller and cleared local state.
        self.closing.take();
        stop()
    }

    fn on_begin(&mut self) -> Effect<Self> {
        if self.is_closing() || self.read_started || self.connect_dispatched {
            return noop();
        }
        if self.established_stream().is_some() {
            self.read_started = true;
            return Effect::Batch(vec![self.read_effect(), self.idle_arm_effect()]);
        }
        let Some(addr) = self.pending_connect else {
            // No stream and no pending connect — misconfiguration.
            return self.begin_close(ClientResult::IoError(CallError::InvalidResource));
        };
        self.connect_dispatched = true;
        tcp_connect(addr).reply(|res| match res {
            Ok((stream, _local, _peer)) => ClientMsg::Connected(Ok(stream)),
            Err(err) => ClientMsg::Connected(Err(err)),
        })
    }

    fn on_connected(&mut self, result: Result<StreamId, CallError>) -> Effect<Self> {
        if self.is_closing() {
            return noop();
        }
        match result {
            Ok(stream) => {
                self.stream = Some(stream);
                self.read_started = true;
                let mut effects = vec![self.read_effect(), self.idle_arm_effect()];
                // If requests came in before Connected, their bytes are
                // already queued; kick the write loop now that the stream
                // exists.
                if let Some(eff) = self.maybe_start_write() {
                    effects.push(eff);
                }
                Effect::Batch(effects)
            }
            Err(err) => self.begin_close(ClientResult::IoError(err)),
        }
    }

    fn on_request(&mut self, req: ClientRequest) -> Effect<Self> {
        debug_assert!(
            !req.deadline.is_zero(),
            "ClientRequest::deadline must be non-zero; a zero deadline races with the wire write",
        );
        if self.is_closing() {
            // Connection going down: report ConnectionClosed immediately.
            return Self::notify_caller(
                req.reply_to,
                req.correlator,
                ClientResult::ConnectionClosed,
            );
        }
        if self.in_flight.len() >= self.config.max_in_flight {
            return Self::notify_caller(req.reply_to, req.correlator, ClientResult::Full);
        }
        let request_id = match self.allocate_request_id() {
            Some(id) => id,
            // Unreachable while max_in_flight matches the map cap, but
            // surface as Full rather than panic on a corrupted state.
            None => {
                return Self::notify_caller(req.reply_to, req.correlator, ClientResult::Full);
            }
        };
        let frame = Frame::request(request_id, req.service, req.method, req.payload);
        let bytes = match encode(&frame, &self.frame_limits()) {
            Ok(bytes) => bytes,
            Err(_) => {
                return Self::notify_caller(
                    req.reply_to,
                    req.correlator,
                    ClientResult::LocalEncodeFailed,
                );
            }
        };
        if self.write_queue.len() >= self.config.write_queue_cap {
            // Local backpressure beyond write_queue_cap is a hard
            // configuration fault: with `write_queue_cap >= max_in_flight`
            // (a sensible-defaults invariant), the queue cannot exceed cap
            // unless the runtime is wedged. Mirror the connection isolate
            // and tear the connection down so the user sees IoError on
            // every pending request rather than a per-request `Full` that
            // they would naturally retry into the same congestion.
            //
            // The triggering request itself was never inserted into
            // `in_flight`, so `begin_close` would not notify it. Notify
            // it explicitly first; `begin_close` then fans `IoError` out
            // to every other pending request.
            let notify = Self::notify_caller(
                req.reply_to,
                req.correlator,
                ClientResult::IoError(CallError::Io),
            );
            return Effect::Batch(vec![
                notify,
                self.begin_close(ClientResult::IoError(CallError::Io)),
            ]);
        }
        self.in_flight.insert(
            request_id,
            InFlight {
                correlator: req.correlator,
                reply_to: req.reply_to,
            },
        );
        self.write_queue.push_back((Some(request_id), bytes));
        let mut effects = Vec::new();
        if let Some(eff) = self.maybe_start_write() {
            effects.push(eff);
        }
        effects.push(self.deadline_arm_effect(request_id, req.deadline));
        Effect::Batch(effects)
    }

    fn drain_inbound(&mut self) -> Result<Vec<Effect<Self>>, DecodeError> {
        let mut effects = Vec::new();
        loop {
            if self.inbound.len() < LENGTH_PREFIX_SIZE {
                return Ok(effects);
            }
            let mut prefix = [0u8; LENGTH_PREFIX_SIZE];
            prefix.copy_from_slice(&self.inbound[..LENGTH_PREFIX_SIZE]);
            let body_len = parse_length_prefix(prefix, &self.frame_limits())?;
            let total = LENGTH_PREFIX_SIZE + body_len;
            if self.inbound.len() < total {
                return Ok(effects);
            }
            let body = self.inbound[LENGTH_PREFIX_SIZE..total].to_vec();
            self.inbound.drain(..total);
            let frame = decode_body(&body)?;
            // Server frames are always Reply or Error; a Request from the
            // server would itself be a peer fault, but we treat it as a
            // protocol error and close.
            match frame.kind {
                FrameKind::Reply => {
                    if let Some(entry) = self.in_flight.remove(&frame.request_id) {
                        effects.push(Self::notify(entry, ClientResult::Ok(frame.payload)));
                    }
                    // Stray reply (no in-flight slot) is dropped silently.
                }
                FrameKind::Error => {
                    if let Some(entry) = self.in_flight.remove(&frame.request_id) {
                        let code = frame.error.unwrap_or(FrameError::Internal);
                        effects.push(Self::notify(entry, ClientResult::ServerError(code)));
                    }
                }
                FrameKind::Request => {
                    // Server should not send Request frames to a client.
                    return Err(DecodeError::UnknownKind(0));
                }
            }
        }
    }

    fn on_read(&mut self, result: Result<Vec<u8>, CallError>) -> Effect<Self> {
        if self.is_closing() {
            return noop();
        }
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(err) => return self.begin_close(ClientResult::IoError(err)),
        };
        if bytes.is_empty() {
            return self.begin_close(ClientResult::ConnectionClosed);
        }
        self.activity_seq = self.activity_seq.wrapping_add(1);
        self.inbound.extend_from_slice(&bytes);
        let mut effects = match self.drain_inbound() {
            Ok(effects) => effects,
            // Server sent us garbage; fail every in-flight as
            // ConnectionClosed and tear down. We do not invent a more
            // specific user-visible variant — the wire-error invariant
            // is one-directional (server → client).
            Err(_err) => return self.begin_close(ClientResult::ConnectionClosed),
        };
        if !self.reads_done {
            effects.push(self.read_effect());
        }
        Effect::Batch(effects)
    }

    fn on_wrote(&mut self, result: Result<usize, CallError>) -> Effect<Self> {
        if self.close_dispatched {
            return noop();
        }
        let count = match result {
            Ok(c) => c,
            Err(err) => return self.begin_close(ClientResult::IoError(err)),
        };
        let attempted = self.write_in_flight.take().unwrap_or_default();
        if count > attempted.len() {
            return self.begin_close(ClientResult::IoError(CallError::Io));
        }
        if !attempted.is_empty() && count == 0 {
            // Zero-progress: avoid spinning forever.
            return self.begin_close(ClientResult::IoError(CallError::Io));
        }
        if count > 0 {
            self.activity_seq = self.activity_seq.wrapping_add(1);
        }
        if count < attempted.len() {
            let tail = attempted[count..].to_vec();
            // Tail tags as `None`: those bytes have already been
            // partially written to the wire and cannot be cancelled by
            // a subsequent deadline-fire on the same request.
            self.write_queue.push_front((None, tail));
        }
        match self.maybe_start_write() {
            Some(eff) => eff,
            None => noop(),
        }
    }

    fn on_deadline(&mut self, request_id: u64) -> Effect<Self> {
        if self.is_closing() {
            return noop();
        }
        match self.in_flight.remove(&request_id) {
            Some(entry) => {
                // Drop any not-yet-written queued frame for this
                // request. Bytes in `write_in_flight` and any
                // partial-write tail (tagged `None`) are already on
                // their way to the wire and cannot be recalled — the
                // server completes the request and the late reply is
                // dropped on arrival, per the no-cancellation-frame
                // rule. But a request whose bytes never made it past
                // the write queue can be silently suppressed: no
                // wasted server work, no wasted bytes on the wire.
                self.write_queue.retain(|(rid, _)| *rid != Some(request_id));
                Self::notify(entry, ClientResult::Timeout)
            }
            // Already replied to (or already closed); nothing to do.
            None => noop(),
        }
    }

    fn on_idle(&mut self, snapshot: u64) -> Effect<Self> {
        if self.is_closing() {
            return noop();
        }
        if snapshot == self.activity_seq {
            return self.begin_close(ClientResult::Idle);
        }
        self.idle_arm_effect()
    }

    fn on_stream_closed(&mut self, _result: Result<(), CallError>) -> Effect<Self> {
        self.finalize_close()
    }

    fn on_shutdown(&mut self) -> Effect<Self> {
        self.begin_close(ClientResult::ConnectionClosed)
    }
}

impl<S> tina::Isolate for Client<S>
where
    S: tina::Shard,
{
    type Message = ClientMsg;
    type Reply = ();
    type Send = Outbound<ClientResultMsg>;
    type Spawn = std::convert::Infallible;
    type Call = RuntimeCall<ClientMsg>;
    type Shard = S;

    fn handle(&mut self, msg: ClientMsg, _ctx: &mut Context<'_, S>) -> Effect<Self> {
        match msg {
            ClientMsg::Begin => self.on_begin(),
            ClientMsg::Connected(result) => self.on_connected(result),
            ClientMsg::Request(req) => self.on_request(req),
            ClientMsg::Read(result) => self.on_read(result),
            ClientMsg::Wrote(result) => self.on_wrote(result),
            ClientMsg::StreamClosed(result) => self.on_stream_closed(result),
            ClientMsg::DeadlineFired { request_id } => self.on_deadline(request_id),
            ClientMsg::IdleTick { snapshot } => self.on_idle(snapshot),
            ClientMsg::Shutdown => self.on_shutdown(),
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

    fn reply_addr(id: u64) -> Address<ClientResultMsg> {
        Address::new(ShardId::new(0), IsolateId::new(id))
    }

    fn small_config() -> ClientConfig {
        ClientConfig {
            max_frame_size: 1024,
            max_in_flight: 2,
            idle_timeout: Duration::from_millis(50),
            read_chunk: 256,
            write_queue_cap: 8,
        }
    }

    fn make_client(config: ClientConfig) -> Client<TestShard> {
        Client::new(ClientInit {
            stream: ClientStream::Established(StreamId::new(0)),
            config,
            _shard: std::marker::PhantomData,
        })
    }

    fn dispatch(client: &mut Client<TestShard>, msg: ClientMsg) -> Effect<Client<TestShard>> {
        let mut shard = TestShard;
        let mut ctx = Context::new(&mut shard, IsolateId::new(99));
        client.handle(msg, &mut ctx)
    }

    fn make_request(correlator: u64, deadline_ms: u64) -> ClientRequest {
        ClientRequest {
            service: "svc".into(),
            method: "m".into(),
            payload: b"req".to_vec(),
            deadline: Duration::from_millis(deadline_ms),
            correlator,
            reply_to: reply_addr(7),
        }
    }

    fn collect_sends(effect: &Effect<Client<TestShard>>) -> Vec<ClientResultMsg> {
        let mut out = Vec::new();
        walk(effect, &mut out);
        out
    }

    fn walk(effect: &Effect<Client<TestShard>>, out: &mut Vec<ClientResultMsg>) {
        match effect {
            Effect::Send(send) => {
                out.push(send.message().clone());
            }
            Effect::Batch(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            _ => {}
        }
    }

    fn build_reply_frame(request_id: u64, payload: &[u8]) -> Vec<u8> {
        let f = Frame::reply(request_id, "svc", "m", payload.to_vec());
        encode(&f, &FrameLimits::new(1024)).unwrap()
    }

    fn build_error_frame(request_id: u64, code: FrameError) -> Vec<u8> {
        let f = Frame::error(request_id, "svc", "m", code, Vec::new());
        encode(&f, &FrameLimits::new(1024)).unwrap()
    }

    #[test]
    fn begin_kicks_read_loop_and_arms_idle() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Begin);
        assert!(client.read_started);
        // Idle should be armed; no in-flight, no writes.
        assert!(client.in_flight.is_empty());
        assert!(client.write_in_flight.is_none());
    }

    #[test]
    fn begin_is_idempotent() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Begin);
        let effect = dispatch(&mut client, ClientMsg::Begin);
        assert!(matches!(effect, Effect::Noop));
    }

    #[test]
    fn begin_in_pending_mode_is_idempotent_before_connected() {
        // Regression: a second `Begin` while `tcp_connect` is in flight
        // must NOT trip the "no stream and no pending connect" branch
        // and tear the connection down.
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let mut client = Client::<TestShard>::new(ClientInit {
            stream: ClientStream::Pending(addr),
            config: small_config(),
            _shard: std::marker::PhantomData,
        });
        // First Begin: dispatches tcp_connect.
        let first = dispatch(&mut client, ClientMsg::Begin);
        assert!(
            matches!(first, Effect::Call(_)),
            "first Begin must dispatch the connect"
        );
        assert!(client.connect_dispatched);
        assert!(client.closing.is_none());
        // Second Begin while still pending: noop, MUST NOT close.
        let second = dispatch(&mut client, ClientMsg::Begin);
        assert!(
            matches!(second, Effect::Noop),
            "second Begin must be a noop"
        );
        assert!(
            client.closing.is_none(),
            "second Begin must not close the connection"
        );
    }

    #[test]
    fn deadline_fire_emits_only_send_no_wire_write() {
        // Wire-error invariant: a local timeout produces a user
        // notification (Effect::Send) but never a wire frame
        // (Effect::Call(tcp_write...)).
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Begin);
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(99, 50)));
        let effect = dispatch(&mut client, ClientMsg::DeadlineFired { request_id: 1 });
        // Walk the effect tree; expect a single Send and zero Call leaves.
        let mut sends = 0usize;
        let mut calls = 0usize;
        fn count<I: tina::Isolate>(eff: &Effect<I>, sends: &mut usize, calls: &mut usize) {
            match eff {
                Effect::Send(_) => *sends += 1,
                Effect::Call(_) => *calls += 1,
                Effect::Batch(items) => items.iter().for_each(|e| count(e, sends, calls)),
                _ => {}
            }
        }
        count(&effect, &mut sends, &mut calls);
        assert_eq!(sends, 1, "deadline must produce one Send notification");
        assert_eq!(calls, 0, "deadline must NOT produce any wire-write Call");
    }

    #[test]
    fn request_registers_in_flight_and_starts_write() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Begin);
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 100)));
        assert_eq!(client.in_flight.len(), 1);
        assert!(client.write_in_flight.is_some());
        assert!(client.read_started);
    }

    #[test]
    fn over_cap_request_returns_full_immediately_no_in_flight_slot() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 100)));
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(2, 100)));
        assert_eq!(client.in_flight.len(), 2);
        let effect = dispatch(&mut client, ClientMsg::Request(make_request(3, 100)));
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].correlator, 3);
        assert_eq!(sends[0].result, ClientResult::Full);
        assert_eq!(client.in_flight.len(), 2, "Full must not consume a slot");
    }

    #[test]
    fn matching_reply_notifies_with_ok() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(42, 100)));
        // Wire request_id should be 1 (next_request_id starts at 1).
        let frame = build_reply_frame(1, b"out");
        let effect = dispatch(&mut client, ClientMsg::Read(Ok(frame)));
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].correlator, 42);
        assert_eq!(sends[0].result, ClientResult::Ok(b"out".to_vec()));
        assert_eq!(client.in_flight.len(), 0);
    }

    #[test]
    fn out_of_order_replies_match_correctly() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(100, 1000)));
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(200, 1000)));
        // Wire ids 1 and 2; reply 2 first.
        let frame_2 = build_reply_frame(2, b"second");
        let effect = dispatch(&mut client, ClientMsg::Read(Ok(frame_2)));
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].correlator, 200);
        assert_eq!(sends[0].result, ClientResult::Ok(b"second".to_vec()));
        assert_eq!(client.in_flight.len(), 1);
        let frame_1 = build_reply_frame(1, b"first");
        let effect = dispatch(&mut client, ClientMsg::Read(Ok(frame_1)));
        let sends = collect_sends(&effect);
        assert_eq!(sends[0].correlator, 100);
        assert_eq!(sends[0].result, ClientResult::Ok(b"first".to_vec()));
        assert_eq!(client.in_flight.len(), 0);
    }

    #[test]
    fn server_error_frame_propagates_as_server_error() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(7, 100)));
        let frame = build_error_frame(1, FrameError::UnknownService);
        let effect = dispatch(&mut client, ClientMsg::Read(Ok(frame)));
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].correlator, 7);
        assert_eq!(
            sends[0].result,
            ClientResult::ServerError(FrameError::UnknownService)
        );
    }

    #[test]
    fn deadline_fires_timeout_when_still_pending() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(99, 50)));
        let effect = dispatch(&mut client, ClientMsg::DeadlineFired { request_id: 1 });
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].result, ClientResult::Timeout);
        assert_eq!(client.in_flight.len(), 0, "slot freed on timeout");
    }

    #[test]
    fn write_queue_overflow_notifies_triggering_request_before_closing() {
        // The request that trips write_queue_cap must itself receive a
        // ClientResultMsg, not just the in-flight requests that
        // begin_close fans out. Caller would otherwise hang.
        //
        // The default invariant (`write_queue_cap >= max_in_flight`,
        // debug-asserted in `Client::new`) makes natural overflow
        // unreachable, but the fields are public on `ClientConfig` so a
        // release-build misconfiguration (or a bug in the runtime that
        // wedges writes) could still hit this branch. Force it by
        // mutating `config` directly after construction.
        let mut client = make_client(small_config());
        client.config.write_queue_cap = 1;
        client.config.max_in_flight = 8;
        let _ = dispatch(&mut client, ClientMsg::Begin);
        // First request: takes write_in_flight, queue stays at 0.
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(0, 1000)));
        // Second request: enqueues, queue length = 1 (== cap).
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 1000)));
        assert_eq!(client.write_queue.len(), 1, "queue at cap before trigger");
        // Third request: queue is at cap → overflow branch.
        let effect = dispatch(&mut client, ClientMsg::Request(make_request(99, 1000)));
        let sends = collect_sends(&effect);
        let triggering: Vec<_> = sends.iter().filter(|m| m.correlator == 99).collect();
        assert_eq!(
            triggering.len(),
            1,
            "triggering request must receive exactly one notification, got {sends:?}",
        );
        assert!(matches!(
            triggering[0].result,
            ClientResult::IoError(CallError::Io)
        ));
        // The two prior in-flight requests also get fan-out IoError.
        let total_io_errors = sends
            .iter()
            .filter(|m| matches!(m.result, ClientResult::IoError(_)))
            .count();
        assert_eq!(
            total_io_errors, 3,
            "2 in-flight + 1 triggering must be notified, got {sends:?}",
        );
    }

    #[test]
    fn deadline_drops_unwritten_queued_request() {
        // A request whose bytes are still queued (not yet on the wire)
        // gets silently suppressed when its deadline fires. The server
        // never sees it; the user still receives Timeout exactly once.
        let mut config = small_config();
        config.max_in_flight = 4;
        config.write_queue_cap = 8;
        let mut client = make_client(config);
        let _ = dispatch(&mut client, ClientMsg::Begin);
        // First request: takes the in-flight write slot (write_in_flight set).
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 1000)));
        assert!(client.write_in_flight.is_some());
        let in_flight_bytes_len = client.write_in_flight.as_ref().unwrap().len();
        // Second request: queued behind the first.
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(2, 1000)));
        assert_eq!(
            client.write_queue.len(),
            1,
            "second request should be queued"
        );
        // Now fire the deadline for request id 2 (the one still queued).
        let effect = dispatch(&mut client, ClientMsg::DeadlineFired { request_id: 2 });
        // User got Timeout for correlator 2.
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].correlator, 2);
        assert_eq!(sends[0].result, ClientResult::Timeout);
        // The queued frame for request 2 is gone.
        assert!(
            client.write_queue.is_empty(),
            "queued frame for timed-out request must be dropped",
        );
        // The first request's in-flight bytes are untouched.
        assert_eq!(
            client.write_in_flight.as_ref().unwrap().len(),
            in_flight_bytes_len,
            "in-flight bytes for unrelated request must not be affected",
        );
    }

    #[test]
    fn deadline_after_reply_is_dropped() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(99, 50)));
        let frame = build_reply_frame(1, b"ok");
        let _ = dispatch(&mut client, ClientMsg::Read(Ok(frame)));
        // Deadline fires AFTER the reply already closed the slot.
        let effect = dispatch(&mut client, ClientMsg::DeadlineFired { request_id: 1 });
        // Must not produce another notification (would be double-reply).
        let sends = collect_sends(&effect);
        assert!(sends.is_empty());
    }

    #[test]
    fn empty_read_closes_pending_with_connection_closed() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 1000)));
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(2, 1000)));
        let effect = dispatch(&mut client, ClientMsg::Read(Ok(Vec::new())));
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 2, "every pending request gets notified");
        for s in &sends {
            assert_eq!(s.result, ClientResult::ConnectionClosed);
        }
        assert!(client.close_dispatched);
    }

    #[test]
    fn idle_no_activity_closes_with_idle() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 1000)));
        // Activity_seq is 0 (request itself doesn't bump it). Idle tick
        // with snapshot 0 matches → close as idle.
        let effect = dispatch(&mut client, ClientMsg::IdleTick { snapshot: 0 });
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].result, ClientResult::Idle);
        assert!(client.close_dispatched);
    }

    #[test]
    fn idle_with_activity_rearms() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 1000)));
        // Force activity by simulating a write completion.
        let attempted_len = client.write_in_flight.as_ref().unwrap().len();
        let _ = dispatch(&mut client, ClientMsg::Wrote(Ok(attempted_len)));
        assert!(client.activity_seq > 0);
        let effect = dispatch(&mut client, ClientMsg::IdleTick { snapshot: 0 });
        // No notifications, no close.
        let sends = collect_sends(&effect);
        assert!(sends.is_empty());
        assert!(!client.close_dispatched);
    }

    #[test]
    fn shutdown_notifies_pending_with_connection_closed_and_stops() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 1000)));
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(2, 1000)));
        let effect = dispatch(&mut client, ClientMsg::Shutdown);
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 2);
        for s in &sends {
            assert_eq!(s.result, ClientResult::ConnectionClosed);
        }
        let close_eff = dispatch(&mut client, ClientMsg::StreamClosed(Ok(())));
        assert!(matches!(close_eff, Effect::Stop));
    }

    #[test]
    fn stray_reply_for_unknown_request_id_is_silently_dropped() {
        let mut client = make_client(small_config());
        // No requests registered.
        let frame = build_reply_frame(999, b"stray");
        let effect = dispatch(&mut client, ClientMsg::Read(Ok(frame)));
        let sends = collect_sends(&effect);
        assert!(sends.is_empty());
        assert!(!client.close_dispatched, "stray reply must not close");
    }

    #[test]
    fn server_request_frame_closes_connection() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 1000)));
        // Server sending a Request kind is a peer fault on the client side.
        let bad = encode(
            &Frame::request(99, "x", "y", Vec::new()),
            &FrameLimits::new(1024),
        )
        .unwrap();
        let effect = dispatch(&mut client, ClientMsg::Read(Ok(bad)));
        // Pending is notified with ConnectionClosed.
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].result, ClientResult::ConnectionClosed);
    }

    #[test]
    fn local_encode_failed_when_payload_exceeds_max_frame_size() {
        let mut config = small_config();
        config.max_frame_size = 32;
        let mut client = make_client(config);
        let big = vec![b'x'; 100];
        let req = ClientRequest {
            service: "svc".into(),
            method: "m".into(),
            payload: big,
            deadline: Duration::from_millis(100),
            correlator: 5,
            reply_to: reply_addr(7),
        };
        let effect = dispatch(&mut client, ClientMsg::Request(req));
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].correlator, 5);
        assert_eq!(sends[0].result, ClientResult::LocalEncodeFailed);
        assert_eq!(
            client.in_flight.len(),
            0,
            "encode failure must not consume slot"
        );
    }

    #[test]
    fn fragmented_reply_assembles_across_reads() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(50, 1000)));
        let frame = build_reply_frame(1, b"fragmented-payload");
        let (head, tail) = frame.split_at(7);
        let _ = dispatch(&mut client, ClientMsg::Read(Ok(head.to_vec())));
        assert_eq!(
            client.in_flight.len(),
            1,
            "still pending after partial read"
        );
        let effect = dispatch(&mut client, ClientMsg::Read(Ok(tail.to_vec())));
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(
            sends[0].result,
            ClientResult::Ok(b"fragmented-payload".to_vec())
        );
    }

    #[test]
    fn zero_progress_write_closes_io_error_and_notifies_pending() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 1000)));
        let effect = dispatch(&mut client, ClientMsg::Wrote(Ok(0)));
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert!(matches!(sends[0].result, ClientResult::IoError(_)));
    }

    #[test]
    fn read_io_error_closes_and_notifies() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 1000)));
        let effect = dispatch(&mut client, ClientMsg::Read(Err(CallError::Io)));
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].result, ClientResult::IoError(CallError::Io));
    }

    #[test]
    fn request_after_close_returns_connection_closed() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 1000)));
        let _ = dispatch(&mut client, ClientMsg::Shutdown);
        let effect = dispatch(&mut client, ClientMsg::Request(make_request(2, 1000)));
        let sends = collect_sends(&effect);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].correlator, 2);
        assert_eq!(sends[0].result, ClientResult::ConnectionClosed);
    }

    #[test]
    fn request_id_allocator_skips_in_use_slots_on_wraparound() {
        let mut config = small_config();
        config.max_in_flight = 4;
        config.write_queue_cap = 8;
        let mut client = make_client(config);
        // Pre-stuff the in-flight map with id 1.
        client.in_flight.insert(
            1,
            InFlight {
                correlator: 99,
                reply_to: reply_addr(7),
            },
        );
        // First allocation should skip 1 and return 2.
        let id = client.allocate_request_id().expect("free slot exists");
        assert_eq!(id, 2);
        // Force wraparound: next_request_id near u64::MAX, only 0 / 1 are
        // problematic — wrap to 1, find it occupied, advance to 2 (also
        // occupied via the just-allocated id... but allocate_request_id
        // just returned 2 without inserting, so 2 is free).
        client.next_request_id = u64::MAX;
        let id = client.allocate_request_id().expect("wraparound finds free");
        assert_eq!(id, u64::MAX);
        // Now next_request_id wrapped to 1; 1 is in_flight so probe to 2.
        let id = client.allocate_request_id().expect("probes past 1");
        assert_eq!(id, 2);
    }

    #[test]
    fn partial_write_requeues_tail() {
        let mut client = make_client(small_config());
        let _ = dispatch(&mut client, ClientMsg::Request(make_request(1, 1000)));
        let attempted_len = client.write_in_flight.as_ref().unwrap().len();
        let _ = dispatch(&mut client, ClientMsg::Wrote(Ok(4)));
        // New write started for the tail.
        assert!(client.write_in_flight.is_some());
        assert_eq!(
            client.write_in_flight.as_ref().unwrap().len(),
            attempted_len - 4
        );
    }
}
