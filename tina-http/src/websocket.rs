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

    /// Build the public bounded send call for this session.
    ///
    /// The call routes through the connection isolate that owns the upgraded
    /// stream and translates runtime `Full` / `Closed` / `Timeout` plus
    /// session-owner admission into [`WebSocketSessionMsg::SendOutcome`].
    pub fn send_effect<
        I: Isolate<Message = WebSocketSessionMsg, Call = RuntimeCall<WebSocketSessionMsg>>,
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

    pub fn text_effect<
        I: Isolate<Message = WebSocketSessionMsg, Call = RuntimeCall<WebSocketSessionMsg>>,
    >(
        self,
        text: impl Into<String>,
        timeout: Duration,
    ) -> Effect<I> {
        self.send_effect(WebSocketMessage::Text(text.into()), timeout)
    }

    pub fn binary_effect<
        I: Isolate<Message = WebSocketSessionMsg, Call = RuntimeCall<WebSocketSessionMsg>>,
    >(
        self,
        bytes: impl Into<Vec<u8>>,
        timeout: Duration,
    ) -> Effect<I> {
        self.send_effect(WebSocketMessage::Binary(bytes.into()), timeout)
    }

    pub fn close_effect<
        I: Isolate<Message = WebSocketSessionMsg, Call = RuntimeCall<WebSocketSessionMsg>>,
    >(
        self,
        code: Option<WebSocketCloseCode>,
        reason: impl Into<Vec<u8>>,
        timeout: Duration,
    ) -> Effect<I> {
        self.send_effect(WebSocketMessage::Close(code, reason.into()), timeout)
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
    accept_key: String,
    offered_subprotocols: Vec<String>,
    extension_offers: Vec<String>,
}

impl WebSocketUpgradeRequest {
    pub fn accept(
        self,
        app: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
        limits: WebSocketLimits,
    ) -> WebSocketAccept {
        WebSocketAccept {
            accept_key: self.accept_key,
            selected_subprotocol: None,
            app,
            limits,
        }
    }

    pub fn accept_subprotocol(
        &self,
        app: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
        limits: WebSocketLimits,
        subprotocol: impl Into<String>,
    ) -> Result<WebSocketAccept, WebSocketError> {
        let subprotocol = subprotocol.into();
        if !is_subprotocol_token(&subprotocol) {
            return Err(WebSocketError::InvalidSubprotocol);
        }
        if !self
            .offered_subprotocols
            .iter()
            .any(|offered| offered == &subprotocol)
        {
            return Err(WebSocketError::UnsupportedSubprotocol);
        }
        Ok(WebSocketAccept {
            accept_key: self.accept_key.clone(),
            selected_subprotocol: Some(subprotocol),
            app,
            limits,
        })
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
}

/// Accepted WebSocket handoff payload carried by
/// [`crate::HttpResponse::websocket`].
#[derive(Debug, Clone)]
pub struct WebSocketAccept {
    accept_key: String,
    selected_subprotocol: Option<String>,
    pub(crate) app: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
    pub(crate) limits: WebSocketLimits,
}

impl WebSocketAccept {
    pub fn accept_key(&self) -> &str {
        &self.accept_key
    }

    pub fn selected_subprotocol(&self) -> Option<&str> {
        self.selected_subprotocol.as_deref()
    }

    pub fn app(&self) -> Address<WebSocketSessionMsg, WebSocketSessionOutcome> {
        self.app
    }

    pub fn limits(&self) -> WebSocketLimits {
        self.limits
    }
}

/// Inbound message delivered from a WebSocket session to an app isolate.
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

/// Result of a public handle send after the connection owner evaluates
/// session identity, close state, frame count, and byte budgets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketSendOutcome {
    pub session: WebSocketSessionId,
    pub result: Result<(), WebSocketSendError>,
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
    if !masked {
        return FrameParse::Error(WebSocketError::ClientFrameUnmasked);
    }
    let mut offset = 2usize;
    let mut len = usize::from(b1 & 0x7f);
    if len == 126 {
        if buf.len() < offset + 2 {
            return FrameParse::NeedMore;
        }
        len = usize::from(u16::from_be_bytes([buf[offset], buf[offset + 1]]));
        offset += 2;
    } else if len == 127 {
        if buf.len() < offset + 8 {
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
        len = match usize::try_from(wide) {
            Ok(v) => v,
            Err(_) => return FrameParse::Error(WebSocketError::FrameTooLarge),
        };
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
    if buf.len() < offset + 4 + len {
        return FrameParse::NeedMore;
    }
    let mask = [
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ];
    offset += 4;
    let mut payload = buf[offset..offset + len].to_vec();
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    buf.drain(..offset + len);
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

pub(crate) fn encode_server_frame(opcode: u8, payload: Vec<u8>) -> Result<Vec<u8>, WebSocketError> {
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
    out.extend_from_slice(&payload);
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
}
