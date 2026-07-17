//! Native WebSocket first form for Tina-owned HTTP/1.1 connections.
//!
//! This module owns the server upgrade validator, the small frame codec,
//! typed session messages, and the bounded outbound queue used by the
//! connection isolate after a successful `101 Switching Protocols`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use http::header::{CONNECTION, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL, UPGRADE};
use http::{HeaderMap, Method};
use sha1::{Digest, Sha1};
use tina::{Address, Effect, Isolate};
use tina_runtime::{CallOutcome, RuntimeCall, call};

use crate::connection::HttpConnectionMsg;
use crate::streaming::RequestChunkReply;
use crate::types::HttpRequest;

const WEBSOCKET_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Capacity and timer knobs for one upgraded WebSocket session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketLimits {
    /// Largest payload accepted in one data frame.
    pub max_frame_bytes: usize,
    /// Largest complete message, including bounded fragmented reassembly.
    pub max_message_bytes: usize,
    /// Largest resident read buffer before the peer is closed.
    pub read_buffer_high_water: usize,
    /// Documented app-side budget. The actual mailbox cap is chosen
    /// when the app isolate is registered; overflow appears as
    /// `AppMailboxFull`.
    pub inbound_app_mailbox_capacity: usize,
    /// Max frames parked while a write is in flight.
    pub outbound_frame_queue_capacity: usize,
    /// Max queued outbound payload/framing bytes.
    pub max_queued_outbound_bytes: usize,
    /// Upper bound a room/broadcast specimen should fan out to.
    pub broadcast_fanout_max_targets: usize,
    /// How long a ping may remain unanswered.
    pub ping_pong_timeout: Duration,
    /// How long the peer gets to finish the close handshake.
    pub close_handshake_timeout: Duration,
}

impl Default for WebSocketLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 64 * 1024,
            max_message_bytes: 64 * 1024,
            read_buffer_high_water: 128 * 1024,
            inbound_app_mailbox_capacity: 16,
            outbound_frame_queue_capacity: 16,
            max_queued_outbound_bytes: 256 * 1024,
            broadcast_fanout_max_targets: 64,
            ping_pong_timeout: Duration::from_secs(30),
            close_handshake_timeout: Duration::from_secs(5),
        }
    }
}

/// Upgrade validation and session protocol errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketError {
    MethodNotGet,
    UpgradeMissing,
    ConnectionUpgradeMissing,
    MissingKey,
    InvalidKey,
    UnsupportedVersion,
    UnsupportedExtension,
    UnsupportedSubprotocol,
    InvalidSubprotocol,
    FrameTooLarge,
    MessageTooLarge,
    ReadBufferTooLarge,
    ClientFrameUnmasked,
    ServerFrameMasked,
    FragmentationUnsupported,
    ControlFrameFragmented,
    ControlFrameTooLarge,
    InvalidOpcode(u8),
    InvalidClosePayload,
    OutboundQueueFull,
    OutboundBytesFull,
    AppMailboxFull,
    PeerClosed,
    ProtocolError,
    Timeout,
    StaleSession,
    Closing,
}

/// Stable identity for one WebSocket session incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WebSocketSessionId {
    id: u64,
    generation: u64,
}

impl WebSocketSessionId {
    pub(crate) fn new(generation: u64) -> Self {
        Self {
            id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            generation,
        }
    }

    pub const fn raw(self) -> u64 {
        self.id
    }

    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Cloneable bounded send handle for one WebSocket session incarnation.
///
/// The handle targets the connection isolate that owns the upgraded stream.
/// Sends are ordinary Tina messages back to that owner; the handle never owns
/// a writer and never keeps a closed session alive.
#[derive(Debug, Clone, Copy)]
pub struct WebSocketSessionHandle {
    session_id: WebSocketSessionId,
    target: Address<HttpConnectionMsg, RequestChunkReply>,
}

impl WebSocketSessionHandle {
    pub(crate) fn new(
        session_id: WebSocketSessionId,
        target: Address<HttpConnectionMsg, RequestChunkReply>,
    ) -> Self {
        Self { session_id, target }
    }

    pub const fn session_id(self) -> WebSocketSessionId {
        self.session_id
    }

    pub const fn target(self) -> Address<HttpConnectionMsg, RequestChunkReply> {
        self.target
    }

    pub fn send(self, message: WebSocketMessage) -> HttpConnectionMsg {
        HttpConnectionMsg::WebSocketSend(WebSocketSend {
            session: self.session_id,
            message,
        })
    }

    pub fn text(self, text: impl Into<String>) -> HttpConnectionMsg {
        self.send(WebSocketMessage::Text(text.into()))
    }

    pub fn binary(self, bytes: impl Into<Vec<u8>>) -> HttpConnectionMsg {
        self.send(WebSocketMessage::Binary(bytes.into()))
    }

    pub fn close(
        self,
        code: Option<WebSocketCloseCode>,
        reason: impl Into<Vec<u8>>,
    ) -> HttpConnectionMsg {
        self.send(WebSocketMessage::Close(code, reason.into()))
    }

    /// Build a bounded owner-routed diagnostic snapshot request.
    ///
    /// This does not subscribe to a stream of events and does not expose room
    /// state. It asks the connection isolate that owns this session for one
    /// point-in-time snapshot.
    pub fn report(self) -> HttpConnectionMsg {
        HttpConnectionMsg::WebSocketReport(WebSocketReportRequest {
            session: self.session_id,
        })
    }

    /// Build the public bounded send call for this session.
    ///
    /// The call routes through the connection isolate that owns the upgraded
    /// stream and translates runtime `Full` / `Closed` / `Timeout` plus
    /// session-owner admission into [`WebSocketSessionMsg::SendOutcome`].
    ///
    /// The continuation is an ordinary later message — not a request-lane
    /// reply — so write admission never re-enters the caller's request authority.
    pub fn send_effect<
        I: Isolate<Message = WebSocketSessionMsg, Io = RuntimeCall<WebSocketSessionMsg>>,
    >(
        self,
        message: WebSocketMessage,
        timeout: Duration,
    ) -> Effect<I> {
        let session = self.session_id;
        call(self.target, self.send(message), timeout).then(move |outcome| {
            WebSocketSessionMsg::SendOutcome(WebSocketSendOutcome::from_connection_call(
                session, outcome,
            ))
        })
    }

    /// Split-service form of [`Self::send_effect`].
    ///
    /// The connection call's admission outcome continues as a domain event so
    /// it never re-enters the caller's request lane.
    pub fn send_effect_service_event<
        I: Isolate<
            Message = tina::ServiceMessage<Event, Request>,
            Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
        >,
        Event,
        Request,
        F,
    >(
        self,
        message: WebSocketMessage,
        timeout: Duration,
        map_outcome: F,
    ) -> Effect<I>
    where
        Event: 'static,
        Request: 'static,
        F: FnOnce(WebSocketSendOutcome) -> Event + 'static,
    {
        let session = self.session_id;
        call(self.target, self.send(message), timeout).then_service_event(move |outcome| {
            map_outcome(WebSocketSendOutcome::from_connection_call(session, outcome))
        })
    }

    pub fn text_effect<
        I: Isolate<Message = WebSocketSessionMsg, Io = RuntimeCall<WebSocketSessionMsg>>,
    >(
        self,
        text: impl Into<String>,
        timeout: Duration,
    ) -> Effect<I> {
        self.send_effect(WebSocketMessage::Text(text.into()), timeout)
    }

    /// Split-service form of [`Self::text_effect`].
    pub fn text_effect_service_event<
        I: Isolate<
            Message = tina::ServiceMessage<Event, Request>,
            Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
        >,
        Event,
        Request,
        F,
    >(
        self,
        text: impl Into<String>,
        timeout: Duration,
        map_outcome: F,
    ) -> Effect<I>
    where
        Event: 'static,
        Request: 'static,
        F: FnOnce(WebSocketSendOutcome) -> Event + 'static,
    {
        self.send_effect_service_event(WebSocketMessage::Text(text.into()), timeout, map_outcome)
    }

    pub fn binary_effect<
        I: Isolate<Message = WebSocketSessionMsg, Io = RuntimeCall<WebSocketSessionMsg>>,
    >(
        self,
        bytes: impl Into<Vec<u8>>,
        timeout: Duration,
    ) -> Effect<I> {
        self.send_effect(WebSocketMessage::Binary(bytes.into()), timeout)
    }

    pub fn close_effect<
        I: Isolate<Message = WebSocketSessionMsg, Io = RuntimeCall<WebSocketSessionMsg>>,
    >(
        self,
        code: Option<WebSocketCloseCode>,
        reason: impl Into<Vec<u8>>,
        timeout: Duration,
    ) -> Effect<I> {
        self.send_effect(WebSocketMessage::Close(code, reason.into()), timeout)
    }

    /// Split-service form of [`Self::close_effect`].
    pub fn close_effect_service_event<
        I: Isolate<
            Message = tina::ServiceMessage<Event, Request>,
            Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
        >,
        Event,
        Request,
        F,
    >(
        self,
        code: Option<WebSocketCloseCode>,
        reason: impl Into<Vec<u8>>,
        timeout: Duration,
        map_outcome: F,
    ) -> Effect<I>
    where
        Event: 'static,
        Request: 'static,
        F: FnOnce(WebSocketSendOutcome) -> Event + 'static,
    {
        self.send_effect_service_event(
            WebSocketMessage::Close(code, reason.into()),
            timeout,
            map_outcome,
        )
    }

    /// Route a bounded session snapshot request through the connection owner.
    ///
    /// Use this for diagnostics and room policy decisions that need
    /// connection-owned state, such as selected subprotocol, close state, and
    /// queued outbound pressure. The returned snapshot is intentionally narrow:
    /// it is not a metrics feed, room report, or event log.
    pub fn report_effect<
        I: Isolate<Message = WebSocketSessionMsg, Io = RuntimeCall<WebSocketSessionMsg>>,
    >(
        self,
        timeout: Duration,
    ) -> Effect<I> {
        let session = self.session_id;
        call(self.target, self.report(), timeout).then(move |outcome| {
            WebSocketSessionMsg::SessionReport(WebSocketSessionReportOutcome::from_connection_call(
                session, outcome,
            ))
        })
    }

    /// Split-service form of [`Self::report_effect`].
    pub fn report_effect_service_event<
        I: Isolate<
            Message = tina::ServiceMessage<Event, Request>,
            Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
        >,
        Event,
        Request,
        F,
    >(
        self,
        timeout: Duration,
        map_outcome: F,
    ) -> Effect<I>
    where
        Event: 'static,
        Request: 'static,
        F: FnOnce(WebSocketSessionReportOutcome) -> Event + 'static,
    {
        let session = self.session_id;
        call(self.target, self.report(), timeout).then_service_event(move |outcome| {
            map_outcome(WebSocketSessionReportOutcome::from_connection_call(
                session, outcome,
            ))
        })
    }
}

impl PartialEq for WebSocketSessionHandle {
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.target.shard() == other.target.shard()
            && self.target.isolate() == other.target.isolate()
            && self.target.generation() == other.target.generation()
    }
}

impl Eq for WebSocketSessionHandle {}

/// Validated upgrade request. Convert it to [`WebSocketAccept`] once
/// the app session address and limits are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketUpgradeRequest {
    pub(crate) accept_key: String,
    offered_subprotocols: Vec<String>,
    extension_offers: Vec<String>,
}

impl WebSocketUpgradeRequest {
    /// Accept with a raw session app address.
    ///
    /// Prefer [`Self::accept_split_service`] or
    /// [`Self::accept_request_service`] for capability-typed install sites.
    /// Event-lane notifications still use observed send on this path so
    /// write completions never enter the application's request lane.
    pub fn accept(
        self,
        app: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
        limits: WebSocketLimits,
    ) -> WebSocketAccept {
        WebSocketAccept::from_parts(
            self.accept_key,
            None,
            crate::websocket_delivery::WebSocketAppDelivery::call(app),
            limits,
        )
    }

    pub fn accept_subprotocol(
        &self,
        app: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
        limits: WebSocketLimits,
        subprotocol: impl Into<String>,
    ) -> Result<WebSocketAccept, WebSocketError> {
        let subprotocol = subprotocol.into();
        self.ensure_subprotocol(&subprotocol)?;
        Ok(WebSocketAccept::from_parts(
            self.accept_key.clone(),
            Some(subprotocol),
            crate::websocket_delivery::WebSocketAppDelivery::call(app),
            limits,
        ))
    }

    pub fn accept_key(&self) -> &str {
        &self.accept_key
    }

    pub fn offered_subprotocols(&self) -> &[String] {
        &self.offered_subprotocols
    }

    pub fn extension_offers(&self) -> &[String] {
        &self.extension_offers
    }

    pub(crate) fn ensure_subprotocol(&self, subprotocol: &str) -> Result<(), WebSocketError> {
        if !is_subprotocol_token(subprotocol) {
            return Err(WebSocketError::InvalidSubprotocol);
        }
        if !self
            .offered_subprotocols
            .iter()
            .any(|offered| offered == subprotocol)
        {
            return Err(WebSocketError::UnsupportedSubprotocol);
        }
        Ok(())
    }
}

/// Accepted WebSocket handoff payload carried by
/// [`crate::HttpResponse::websocket`].
#[derive(Debug, Clone)]
pub struct WebSocketAccept {
    accept_key: String,
    selected_subprotocol: Option<String>,
    pub(crate) delivery: crate::websocket_delivery::WebSocketAppDelivery,
    pub(crate) limits: WebSocketLimits,
}

impl WebSocketAccept {
    pub(crate) fn from_parts(
        accept_key: String,
        selected_subprotocol: Option<String>,
        delivery: crate::websocket_delivery::WebSocketAppDelivery,
        limits: WebSocketLimits,
    ) -> Self {
        Self {
            accept_key,
            selected_subprotocol,
            delivery,
            limits,
        }
    }

    pub fn accept_key(&self) -> &str {
        &self.accept_key
    }

    pub fn selected_subprotocol(&self) -> Option<&str> {
        self.selected_subprotocol.as_deref()
    }

    /// Raw session app address when the accept used the legacy call form.
    ///
    /// Returns `None` when the accept was installed from a typed split or
    /// request-only service handle — hold that handle instead of asking for a
    /// raw address.
    pub fn app(&self) -> Option<Address<WebSocketSessionMsg, WebSocketSessionOutcome>> {
        self.delivery.legacy_app_address()
    }

    pub(crate) fn delivery(&self) -> crate::websocket_delivery::WebSocketAppDelivery {
        self.delivery
    }

    pub fn limits(&self) -> WebSocketLimits {
        self.limits
    }
}

/// App-injected control event for WebSocket session apps.
///
/// Tina's connection owner never emits this from wire input and never turns it
/// into a protocol fact. It is just an ordinary bounded app message with a
/// non-string spelling, so `Full` / `Closed` / `Timeout` remain visible at the
/// send/call site that injects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketSessionControl {
    Start,
    Tick(u64),
    Drain,
}

/// Inbound message delivered from a WebSocket session to an app isolate.
///
/// The connection owner delivers exactly **one session-rich event per wire
/// event**: `SessionText` / `SessionBinary` for messages, `SessionClose` /
/// `SessionClosed` for the close handshake or peer drop, `SessionPressure` for
/// typed backpressure, and `SessionOpen` + `SessionAccepted` on open. It does
/// not also emit the legacy `Text` / `Binary` / `Close` / `Open` / `Closed` /
/// `Pressure` variants for the same wire event — those remain only for
/// app-local messages an app sends to itself. A new app handles wire input with
/// the session-rich variants alone:
///
/// ```
/// use tina_http::{WebSocketSessionMsg, WebSocketSessionOutcome};
///
/// fn handle(msg: WebSocketSessionMsg) -> WebSocketSessionOutcome {
///     match msg {
///         WebSocketSessionMsg::SessionText { text, .. } => WebSocketSessionOutcome::Text(text),
///         WebSocketSessionMsg::SessionBinary { bytes, .. } => WebSocketSessionOutcome::Binary(bytes),
///         WebSocketSessionMsg::SessionClose { code, reason, .. } => {
///             WebSocketSessionOutcome::Close(code, reason)
///         }
///         // SessionOpen / SessionAccepted / SessionPressure / SessionClosed
///         // and everything else need no reply here.
///         _ => WebSocketSessionOutcome::None,
///     }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketSessionMsg {
    Open,
    SessionOpen {
        session: WebSocketSessionHandle,
    },
    SessionAccepted {
        session_id: WebSocketSessionId,
        selected_subprotocol: Option<String>,
    },
    SessionReport(WebSocketSessionReportOutcome),
    SessionText {
        session_id: WebSocketSessionId,
        text: String,
    },
    SessionBinary {
        session_id: WebSocketSessionId,
        bytes: Vec<u8>,
    },
    SessionClose {
        session_id: WebSocketSessionId,
        code: Option<WebSocketCloseCode>,
        reason: Vec<u8>,
    },
    SessionPressure {
        session_id: WebSocketSessionId,
        error: WebSocketError,
    },
    SessionClosed {
        session_id: WebSocketSessionId,
        error: WebSocketError,
    },
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<WebSocketCloseCode>, Vec<u8>),
    Pressure(WebSocketError),
    SendOutcome(WebSocketSendOutcome),
    AppControl(WebSocketSessionControl),
    /// App/admin message for production-shaped room shutdown. Tina's
    /// connection owner never emits this variant; room code may send it
    /// to its own WebSocket app isolate to close every stored session
    /// through the ordinary bounded handle path.
    Shutdown {
        code: Option<WebSocketCloseCode>,
        reason: Vec<u8>,
    },
    Closed(WebSocketError),
}

/// App reply to one WebSocket session turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketSessionOutcome {
    None,
    Text(String),
    Binary(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<WebSocketCloseCode>, Vec<u8>),
    Many(Vec<WebSocketMessage>),
}

/// Outbound server message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<WebSocketCloseCode>, Vec<u8>),
}

/// Admission request routed through the session owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketSend {
    pub session: WebSocketSessionId,
    pub message: WebSocketMessage,
}

/// Report request routed through the session owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketReportRequest {
    pub session: WebSocketSessionId,
}

/// Result of a public handle send after the connection owner evaluates
/// session identity, close state, frame count, and byte budgets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketSendOutcome {
    pub session: WebSocketSessionId,
    pub result: Result<(), WebSocketSendError>,
}

/// Bounded diagnostic snapshot of one connection-owned WebSocket session.
///
/// This is deliberately a point-in-time owner report, not a metrics stream or
/// a room/server report. It is `non_exhaustive` so Tina can add narrowly useful
/// fields without turning today's queue internals into a forever-complete
/// observability API.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketSessionReport {
    pub session: WebSocketSessionId,
    pub selected_subprotocol: Option<String>,
    pub close_sent: bool,
    pub close_received: bool,
    pub queued_outbound_frames: usize,
    pub queued_outbound_bytes: usize,
    pub pending_write_bytes: usize,
    pub last_pressure: Option<WebSocketError>,
    pub last_close_code: Option<WebSocketCloseCode>,
    pub last_close_reason_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketSessionReportOutcome {
    pub session: WebSocketSessionId,
    pub result: Result<WebSocketSessionReport, WebSocketSendError>,
}

/// Public send-admission failures. These are intentionally narrower than
/// protocol close reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSocketSendError {
    Stale,
    Closing,
    Closed,
    OutboundQueueFull,
    OutboundBytesFull,
    Protocol,
    Timeout,
}

impl From<WebSocketError> for WebSocketSendError {
    fn from(value: WebSocketError) -> Self {
        match value {
            WebSocketError::StaleSession => Self::Stale,
            WebSocketError::Closing => Self::Closing,
            WebSocketError::PeerClosed => Self::Closed,
            WebSocketError::OutboundQueueFull => Self::OutboundQueueFull,
            WebSocketError::OutboundBytesFull => Self::OutboundBytesFull,
            _ => Self::Protocol,
        }
    }
}

impl WebSocketSendOutcome {
    pub fn from_connection_call(
        session: WebSocketSessionId,
        outcome: CallOutcome<RequestChunkReply>,
    ) -> Self {
        let result = match outcome {
            CallOutcome::Replied(RequestChunkReply::WebSocketSend(outcome)) => outcome.result,
            CallOutcome::Replied(_) => Err(WebSocketSendError::Protocol),
            CallOutcome::Full => Err(WebSocketSendError::OutboundQueueFull),
            CallOutcome::Closed | CallOutcome::Rejected(_) => Err(WebSocketSendError::Closed),
            CallOutcome::Timeout => Err(WebSocketSendError::Timeout),
        };
        Self { session, result }
    }
}

impl WebSocketSessionReportOutcome {
    pub fn from_connection_call(
        session: WebSocketSessionId,
        outcome: CallOutcome<RequestChunkReply>,
    ) -> Self {
        let result = match outcome {
            CallOutcome::Replied(RequestChunkReply::WebSocketReport(outcome)) => outcome.result,
            CallOutcome::Replied(_) => Err(WebSocketSendError::Protocol),
            CallOutcome::Full => Err(WebSocketSendError::OutboundQueueFull),
            CallOutcome::Closed | CallOutcome::Rejected(_) => Err(WebSocketSendError::Closed),
            CallOutcome::Timeout => Err(WebSocketSendError::Timeout),
        };
        Self { session, result }
    }
}

/// Close code wrapper. Keeps raw code visible while avoiding a large
/// enum in the first form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketCloseCode(pub u16);

/// Bounded queue used by the session when app output arrives while the
/// write lane is busy.
#[derive(Debug, Clone)]
pub struct WebSocketOutboundQueue {
    queue: VecDeque<Vec<u8>>,
    max_frames: usize,
    max_bytes: usize,
    queued_bytes: usize,
}

impl WebSocketOutboundQueue {
    pub fn new(max_frames: usize, max_bytes: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            max_frames,
            max_bytes,
            queued_bytes: 0,
        }
    }

    pub fn push(&mut self, bytes: Vec<u8>) -> Result<(), WebSocketError> {
        if self.queue.len() >= self.max_frames {
            return Err(WebSocketError::OutboundQueueFull);
        }
        let next = self
            .queued_bytes
            .checked_add(bytes.len())
            .ok_or(WebSocketError::OutboundBytesFull)?;
        if next > self.max_bytes {
            return Err(WebSocketError::OutboundBytesFull);
        }
        self.queued_bytes = next;
        self.queue.push_back(bytes);
        Ok(())
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let bytes = self.queue.pop_front()?;
        self.queued_bytes = self.queued_bytes.saturating_sub(bytes.len());
        Some(bytes)
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn max_frames(&self) -> usize {
        self.max_frames
    }

    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }
}

pub(crate) enum FrameParse {
    NeedMore,
    Frame(WebSocketFrame),
    Error(WebSocketError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WebSocketFrame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// Validate an HTTP request as a server-side WebSocket upgrade.
pub fn websocket_upgrade(
    request: &HttpRequest,
    _limits: WebSocketLimits,
) -> Result<WebSocketUpgradeRequest, WebSocketError> {
    if request.method != Method::GET {
        return Err(WebSocketError::MethodNotGet);
    }
    if !header_has_token(&request.headers, UPGRADE, "websocket") {
        return Err(WebSocketError::UpgradeMissing);
    }
    if !header_has_token(&request.headers, CONNECTION, "upgrade") {
        return Err(WebSocketError::ConnectionUpgradeMissing);
    }
    match request.headers.get("sec-websocket-version") {
        Some(version) if version.as_bytes() == b"13" => {}
        _ => return Err(WebSocketError::UnsupportedVersion),
    }
    let offered_subprotocols = parse_subprotocol_headers(&request.headers)?;
    let extension_offers = parse_extension_offers(&request.headers);
    let key = request
        .headers
        .get(SEC_WEBSOCKET_KEY)
        .ok_or(WebSocketError::MissingKey)?;
    let key = key.to_str().map_err(|_| WebSocketError::InvalidKey)?.trim();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(key.as_bytes())
        .map_err(|_| WebSocketError::InvalidKey)?;
    if decoded.len() != 16 {
        return Err(WebSocketError::InvalidKey);
    }
    Ok(WebSocketUpgradeRequest {
        accept_key: websocket_accept_key(key),
        offered_subprotocols,
        extension_offers,
    })
}

fn header_has_token(headers: &HeaderMap, name: http::HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().ok().is_some_and(|s| {
            s.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
    })
}

fn websocket_accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WEBSOCKET_GUID);
    let digest = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(digest)
}

pub(crate) fn websocket_accept_key_for_client(key: &str) -> String {
    websocket_accept_key(key)
}

fn parse_subprotocol_headers(headers: &HeaderMap) -> Result<Vec<String>, WebSocketError> {
    let mut out = Vec::new();
    for value in headers.get_all(SEC_WEBSOCKET_PROTOCOL) {
        let value = value
            .to_str()
            .map_err(|_| WebSocketError::InvalidSubprotocol)?;
        for part in value.split(',') {
            let token = part.trim();
            if token.is_empty() || !is_subprotocol_token(token) {
                return Err(WebSocketError::InvalidSubprotocol);
            }
            if out.iter().any(|existing| existing == token) {
                return Err(WebSocketError::InvalidSubprotocol);
            }
            out.push(token.to_owned());
        }
    }
    Ok(out)
}

fn parse_extension_offers(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("sec-websocket-extensions")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(str::trim))
        .filter(|offer| !offer.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn is_subprotocol_token(token: &str) -> bool {
    !token.is_empty()
        && token.bytes().all(|b| {
            matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
                    | b'0'..=b'9'
                    | b'A'..=b'Z'
                    | b'a'..=b'z'
            )
        })
}

pub(crate) fn parse_client_frame(buf: &mut Vec<u8>, limits: WebSocketLimits) -> FrameParse {
    parse_frame(buf, limits, true)
}

pub(crate) fn parse_server_frame(buf: &mut Vec<u8>, limits: WebSocketLimits) -> FrameParse {
    parse_frame(buf, limits, false)
}

fn parse_frame(buf: &mut Vec<u8>, limits: WebSocketLimits, expect_masked: bool) -> FrameParse {
    if buf.len() < 2 {
        return FrameParse::NeedMore;
    }
    let b0 = buf[0];
    let b1 = buf[1];
    let fin = b0 & 0x80 != 0;
    let rsv = b0 & 0x70;
    let opcode = b0 & 0x0f;
    let masked = b1 & 0x80 != 0;
    if rsv != 0 {
        return FrameParse::Error(WebSocketError::UnsupportedExtension);
    }
    if expect_masked && !masked {
        return FrameParse::Error(WebSocketError::ClientFrameUnmasked);
    }
    if !expect_masked && masked {
        return FrameParse::Error(WebSocketError::ServerFrameMasked);
    }
    let mut offset = 2usize;
    let mut len = usize::from(b1 & 0x7f);
    if len == 126 {
        let Some(need) = offset.checked_add(2) else {
            return FrameParse::Error(WebSocketError::FrameTooLarge);
        };
        if buf.len() < need {
            return FrameParse::NeedMore;
        }
        len = usize::from(u16::from_be_bytes([buf[offset], buf[offset + 1]]));
        if len < 126 {
            return FrameParse::Error(WebSocketError::ProtocolError);
        }
        offset += 2;
    } else if len == 127 {
        let Some(need) = offset.checked_add(8) else {
            return FrameParse::Error(WebSocketError::FrameTooLarge);
        };
        if buf.len() < need {
            return FrameParse::NeedMore;
        }
        let wide = u64::from_be_bytes([
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
            buf[offset + 4],
            buf[offset + 5],
            buf[offset + 6],
            buf[offset + 7],
        ]);
        if wide & (1u64 << 63) != 0 {
            return FrameParse::Error(WebSocketError::ProtocolError);
        }
        len = match usize::try_from(wide) {
            Ok(v) => v,
            Err(_) => return FrameParse::Error(WebSocketError::FrameTooLarge),
        };
        if len <= usize::from(u16::MAX) {
            return FrameParse::Error(WebSocketError::ProtocolError);
        }
        offset += 8;
    }
    let is_control = opcode >= 0x8;
    if is_control && len > 125 {
        return FrameParse::Error(WebSocketError::ControlFrameTooLarge);
    }
    if is_control && !fin {
        return FrameParse::Error(WebSocketError::ControlFrameFragmented);
    }
    if len > limits.max_frame_bytes {
        return FrameParse::Error(WebSocketError::FrameTooLarge);
    }
    if len > limits.max_message_bytes {
        return FrameParse::Error(WebSocketError::MessageTooLarge);
    }
    let mask_end = if masked {
        let Some(mask_end) = offset.checked_add(4) else {
            return FrameParse::Error(WebSocketError::FrameTooLarge);
        };
        mask_end
    } else {
        offset
    };
    let Some(frame_end) = mask_end.checked_add(len) else {
        return FrameParse::Error(WebSocketError::FrameTooLarge);
    };
    if buf.len() < frame_end {
        return FrameParse::NeedMore;
    }
    let mask = if masked {
        [
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
        ]
    } else {
        [0, 0, 0, 0]
    };
    offset = mask_end;
    let mut payload = buf[offset..frame_end].to_vec();
    if masked {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }
    buf.drain(..frame_end);
    match opcode {
        0x0 => FrameParse::Frame(WebSocketFrame {
            fin,
            opcode,
            payload,
        }),
        0x1 | 0x2 | 0x8 | 0x9 | 0xA => FrameParse::Frame(WebSocketFrame {
            fin,
            opcode,
            payload,
        }),
        other => FrameParse::Error(WebSocketError::InvalidOpcode(other)),
    }
}

pub(crate) fn encode_server_message(message: WebSocketMessage) -> Result<Vec<u8>, WebSocketError> {
    match message {
        WebSocketMessage::Text(text) => encode_server_frame(0x1, text.into_bytes()),
        WebSocketMessage::Binary(bytes) => encode_server_frame(0x2, bytes),
        WebSocketMessage::Ping(bytes) => encode_server_frame(0x9, bytes),
        WebSocketMessage::Pong(bytes) => encode_server_frame(0xA, bytes),
        WebSocketMessage::Close(code, reason) => {
            let mut payload = Vec::with_capacity(2 + reason.len());
            if let Some(code) = code {
                payload.extend_from_slice(&code.0.to_be_bytes());
            }
            payload.extend_from_slice(&reason);
            encode_server_frame(0x8, payload)
        }
    }
}

pub(crate) fn encode_client_message(
    message: WebSocketMessage,
    mask: [u8; 4],
) -> Result<Vec<u8>, WebSocketError> {
    match message {
        WebSocketMessage::Text(text) => encode_client_frame(0x1, text.into_bytes(), mask),
        WebSocketMessage::Binary(bytes) => encode_client_frame(0x2, bytes, mask),
        WebSocketMessage::Ping(bytes) => encode_client_frame(0x9, bytes, mask),
        WebSocketMessage::Pong(bytes) => encode_client_frame(0xA, bytes, mask),
        WebSocketMessage::Close(code, reason) => {
            let mut payload = Vec::with_capacity(2 + reason.len());
            if let Some(code) = code {
                payload.extend_from_slice(&code.0.to_be_bytes());
            }
            payload.extend_from_slice(&reason);
            encode_client_frame(0x8, payload, mask)
        }
    }
}

pub(crate) fn encode_client_frame(
    opcode: u8,
    payload: Vec<u8>,
    mask: [u8; 4],
) -> Result<Vec<u8>, WebSocketError> {
    if opcode >= 0x8 && payload.len() > 125 {
        return Err(WebSocketError::ControlFrameTooLarge);
    }
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode);
    if payload.len() < 126 {
        out.push(0x80 | payload.len() as u8);
    } else if u16::try_from(payload.len()).is_ok() {
        out.push(0x80 | 126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (i, byte) in payload.into_iter().enumerate() {
        out.push(byte ^ mask[i % 4]);
    }
    Ok(out)
}

pub(crate) fn encode_server_frame(opcode: u8, payload: Vec<u8>) -> Result<Vec<u8>, WebSocketError> {
    encode_server_frame_from(opcode, &payload)
}

/// Encode a server (unmasked) frame from a borrowed payload slice. Lets a
/// caller that still needs the owned payload afterwards (e.g. echoing a ping
/// payload into a pong *and* notifying the app) build the wire frame without
/// cloning the payload.
pub(crate) fn encode_server_frame_from(
    opcode: u8,
    payload: &[u8],
) -> Result<Vec<u8>, WebSocketError> {
    if opcode >= 0x8 && payload.len() > 125 {
        return Err(WebSocketError::ControlFrameTooLarge);
    }
    let mut out = Vec::with_capacity(2 + payload.len() + 8);
    out.push(0x80 | opcode);
    if payload.len() < 126 {
        out.push(payload.len() as u8);
    } else if u16::try_from(payload.len()).is_ok() {
        out.push(126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    Ok(out)
}

pub(crate) fn outcome_messages(outcome: WebSocketSessionOutcome) -> Vec<WebSocketMessage> {
    match outcome {
        WebSocketSessionOutcome::None => Vec::new(),
        WebSocketSessionOutcome::Text(text) => vec![WebSocketMessage::Text(text)],
        WebSocketSessionOutcome::Binary(bytes) => vec![WebSocketMessage::Binary(bytes)],
        WebSocketSessionOutcome::Pong(bytes) => vec![WebSocketMessage::Pong(bytes)],
        WebSocketSessionOutcome::Close(code, reason) => vec![WebSocketMessage::Close(code, reason)],
        WebSocketSessionOutcome::Many(messages) => messages,
    }
}

pub(crate) fn decode_close_payload(
    payload: &[u8],
) -> Result<(Option<WebSocketCloseCode>, Vec<u8>), WebSocketError> {
    match payload.len() {
        0 => Ok((None, Vec::new())),
        1 => Err(WebSocketError::InvalidClosePayload),
        _ => {
            let code = u16::from_be_bytes([payload[0], payload[1]]);
            if !valid_close_code(code) || std::str::from_utf8(&payload[2..]).is_err() {
                return Err(WebSocketError::InvalidClosePayload);
            }
            Ok((Some(WebSocketCloseCode(code)), payload[2..].to_vec()))
        }
    }
}

fn valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masked_text_frame_with_len_marker(marker: u8, extended: &[u8], payload: &[u8]) -> Vec<u8> {
        let mask = [0x11, 0x22, 0x33, 0x44];
        let mut frame = vec![0x81, 0x80 | marker];
        frame.extend_from_slice(extended);
        frame.extend_from_slice(&mask);
        for (i, byte) in payload.iter().enumerate() {
            frame.push(*byte ^ mask[i % 4]);
        }
        frame
    }

    #[test]
    fn accept_key_matches_rfc_example() {
        assert_eq!(
            websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn outbound_queue_caps_frames_and_bytes() {
        let mut queue = WebSocketOutboundQueue::new(1, 4);
        queue.push(vec![1, 2, 3, 4]).unwrap();
        assert_eq!(queue.push(vec![5]), Err(WebSocketError::OutboundQueueFull));
        assert_eq!(queue.pop(), Some(vec![1, 2, 3, 4]));
        assert_eq!(
            queue.push(vec![1, 2, 3, 4, 5]),
            Err(WebSocketError::OutboundBytesFull)
        );
    }

    #[test]
    fn session_handle_carries_session_and_address_generation() {
        let session = WebSocketSessionId {
            id: 7,
            generation: 3,
        };
        let target = Address::<HttpConnectionMsg, RequestChunkReply>::new_with_generation(
            tina::ShardId::new(1),
            tina::IsolateId::new(2),
            tina::AddressGeneration::new(9),
        );
        let handle = WebSocketSessionHandle::new(session, target);
        assert_eq!(handle.session_id(), session);
        assert_eq!(handle.target().generation().get(), 9);
        match handle.text("hello") {
            HttpConnectionMsg::WebSocketSend(send) => {
                assert_eq!(send.session, session);
                assert_eq!(send.message, WebSocketMessage::Text("hello".to_owned()));
            }
            other => panic!("expected WebSocketSend, got {other:?}"),
        }
    }

    #[test]
    fn stale_session_ids_are_distinct_from_refill_sessions() {
        let stale = WebSocketSessionId {
            id: 1,
            generation: 0,
        };
        let refill = WebSocketSessionId {
            id: 2,
            generation: 0,
        };
        assert_ne!(stale, refill);
    }

    #[test]
    fn app_control_lane_is_not_peer_text() {
        let control = WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(42));
        assert_ne!(control, WebSocketSessionMsg::Text("__tick:42".to_string()));
        assert!(matches!(
            control,
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(42))
        ));
    }

    #[test]
    fn client_frame_rejects_non_minimal_126_length() {
        let mut buf = masked_text_frame_with_len_marker(126, &1u16.to_be_bytes(), b"x");
        let parsed = parse_client_frame(&mut buf, WebSocketLimits::default());
        assert!(matches!(
            parsed,
            FrameParse::Error(WebSocketError::ProtocolError)
        ));
    }

    #[test]
    fn client_frame_rejects_non_minimal_127_length() {
        let mut buf = masked_text_frame_with_len_marker(127, &125u64.to_be_bytes(), b"x");
        let parsed = parse_client_frame(&mut buf, WebSocketLimits::default());
        assert!(matches!(
            parsed,
            FrameParse::Error(WebSocketError::ProtocolError)
        ));
    }

    #[test]
    fn client_frame_rejects_127_length_high_bit() {
        let mut buf = masked_text_frame_with_len_marker(127, &(1u64 << 63).to_be_bytes(), b"");
        let parsed = parse_client_frame(&mut buf, WebSocketLimits::default());
        assert!(matches!(
            parsed,
            FrameParse::Error(WebSocketError::ProtocolError)
        ));
    }

    #[test]
    fn client_frame_rejects_huge_frame_before_end_offset_overflow() {
        let mut buf = masked_text_frame_with_len_marker(127, &(u64::MAX >> 1).to_be_bytes(), b"");
        let parsed = parse_client_frame(&mut buf, WebSocketLimits::default());
        assert!(
            matches!(
                parsed,
                FrameParse::Error(WebSocketError::FrameTooLarge | WebSocketError::ProtocolError)
            ),
            "huge frame must reject before computing a wrapped end offset"
        );
    }
}

/// Pure parse entry points for the out-of-workspace fuzz harness. The frame
/// parser is `pub(crate)`; this is the only sanctioned way past that boundary
/// and exists only under the `fuzzing` feature.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing {
    /// Feed bytes to the client- and server-side frame parsers under the
    /// default limits. Returns nothing: the harness asserts only that neither
    /// parser panics on arbitrary input.
    pub fn fuzz_parse_frames(bytes: &[u8]) {
        let limits = super::WebSocketLimits::default();
        let mut client_buf = bytes.to_vec();
        let _ = super::parse_client_frame(&mut client_buf, limits);
        let mut server_buf = bytes.to_vec();
        let _ = super::parse_server_frame(&mut server_buf, limits);
    }
}
