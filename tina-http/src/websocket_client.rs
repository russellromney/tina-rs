//! Native WebSocket client connection isolate.
//!
//! One isolate owns one TCP/TLS rail, performs the HTTP/1.1 client
//! upgrade, masks outbound client frames, parses unmasked server frames,
//! and exposes bounded send/receive/report calls. It is intentionally a
//! session-shaped client, not a reconnecting pool and not a Tokio bridge.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use http::StatusCode;
use http::header::{SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_PROTOCOL};
use tina::prelude::*;
use tina::{CallContext, RequestContext, reply_to};
use tina_runtime::{
    CallError, ProtocolFact, StreamId, TcpReadBufReply, TcpWriteOwnedReply, TlsReadBufReply,
    TlsStreamId, TlsWriteOwnedReply, WebSocketCloseReason, tcp_close_stream, tcp_connect,
    tcp_read_buf, tcp_write_owned, tls_close, tls_connect, tls_read_buf, tls_write_owned,
};

use crate::parse::{ResponseParseProgress, parse_response_head};
use crate::target::TlsTrustRoots;
use crate::types::HttpLimits;
use crate::websocket::{
    FrameParse, WebSocketCloseCode, WebSocketError, WebSocketLimits, WebSocketMessage,
    decode_close_payload, encode_client_message, parse_server_frame,
};

const READ_CHUNK: usize = 4096;
static NEXT_MASK_SEED: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

fn tls_read_reply_to_tcp(reply: TlsReadBufReply) -> TcpReadBufReply {
    TcpReadBufReply {
        buffer: reply.buffer,
        len: reply.len,
    }
}

fn tls_write_reply_to_tcp(reply: TlsWriteOwnedReply) -> TcpWriteOwnedReply {
    TcpWriteOwnedReply {
        bytes: reply.bytes,
        written: reply.written,
    }
}

#[derive(Debug, Clone)]
pub enum WebSocketTarget {
    Ws {
        addr: SocketAddr,
        host: String,
        path: String,
    },
    Wss {
        addr: SocketAddr,
        host: String,
        path: String,
        server_name: String,
        trust_roots: TlsTrustRoots,
    },
}

impl WebSocketTarget {
    pub fn ws(addr: SocketAddr, host: impl Into<String>, path: impl Into<String>) -> Self {
        Self::Ws {
            addr,
            host: host.into(),
            path: path.into(),
        }
    }

    pub fn wss(
        addr: SocketAddr,
        host: impl Into<String>,
        path: impl Into<String>,
        server_name: impl Into<String>,
        trust_roots: TlsTrustRoots,
    ) -> Self {
        Self::Wss {
            addr,
            host: host.into(),
            path: path.into(),
            server_name: server_name.into(),
            trust_roots,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketClientError {
    Busy,
    NotConnected,
    Closed,
    InvalidTarget,
    Connect,
    Tls(CallError),
    Write,
    Read,
    BadHandshake,
    AcceptMismatch,
    UnsupportedSubprotocol,
    Protocol(WebSocketError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketClientConnected {
    pub selected_subprotocol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketClientEvent {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close {
        code: Option<WebSocketCloseCode>,
        reason: Vec<u8>,
    },
}

#[non_exhaustive]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WebSocketClientReport {
    pub state: WebSocketClientState,
    pub sent_messages: u64,
    pub received_messages: u64,
    pub pings_received: u64,
    pub pongs_received: u64,
    pub protocol_errors: u64,
    /// Call-only messages received through fire-and-forget `try_send`.
    /// This is caller misuse, but it must be visible in release builds.
    pub wrong_lane_messages: u64,
    pub outbound_queue_full: u64,
    pub outbound_bytes_full: u64,
    pub queued_events: usize,
    pub queued_outbound_frames: usize,
    pub queued_outbound_bytes: usize,
    pub high_water_queued_events: usize,
    pub high_water_outbound_bytes: usize,
    pub close_sent: bool,
    pub close_received: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WebSocketClientState {
    #[default]
    Idle,
    Connecting,
    Open,
    Closing,
    Closed,
}

#[derive(Debug, Clone)]
pub enum WebSocketClientMsg {
    Connect {
        target: WebSocketTarget,
        subprotocols: Vec<String>,
    },
    Send(WebSocketMessage),
    Receive,
    Report,
    Stop,
    Connected(Result<(StreamId, SocketAddr, SocketAddr), CallError>),
    TlsConnected(Result<TlsStreamId, CallError>),
    Wrote(Result<TcpWriteOwnedReply, CallError>),
    Read(Result<TcpReadBufReply, CallError>),
    Closed(Result<(), CallError>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketClientReply {
    Connected(Result<WebSocketClientConnected, WebSocketClientError>),
    Sent(Result<(), WebSocketClientError>),
    Event(Result<WebSocketClientEvent, WebSocketClientError>),
    Report(WebSocketClientReport),
}

#[derive(Debug, Clone, Copy)]
enum ClientTransport {
    Tcp(StreamId),
    Tls(TlsStreamId),
}

struct PendingConnect {
    key: String,
    subprotocols: Vec<String>,
    reply_to: Option<RequestContext<WebSocketClientReply>>,
}

struct ClientFragment {
    opcode: u8,
    payload: Vec<u8>,
}

pub struct WebSocketClientConnection<S: Shard + 'static> {
    limits: WebSocketLimits,
    http_limits: HttpLimits,
    state: WebSocketClientState,
    transport: Option<ClientTransport>,
    pending_connect: Option<PendingConnect>,
    pending_receive: Option<RequestContext<WebSocketClientReply>>,
    read_buf: Vec<u8>,
    fragmented_message: Option<ClientFragment>,
    read_scratch: Vec<u8>,
    pending_write: Vec<u8>,
    write_queue: VecDeque<Vec<u8>>,
    queued_write_bytes: usize,
    write_in_flight: bool,
    read_in_flight: bool,
    events: VecDeque<WebSocketClientEvent>,
    selected_subprotocol: Option<String>,
    close_sent: bool,
    close_received: bool,
    report: WebSocketClientReport,
    mask_seed: u64,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> WebSocketClientConnection<S> {
    pub fn new(limits: WebSocketLimits) -> Self {
        Self {
            limits,
            http_limits: HttpLimits::default(),
            state: WebSocketClientState::Idle,
            transport: None,
            pending_connect: None,
            pending_receive: None,
            read_buf: Vec::new(),
            fragmented_message: None,
            read_scratch: Vec::new(),
            pending_write: Vec::new(),
            write_queue: VecDeque::new(),
            queued_write_bytes: 0,
            write_in_flight: false,
            read_in_flight: false,
            events: VecDeque::new(),
            selected_subprotocol: None,
            close_sent: false,
            close_received: false,
            report: WebSocketClientReport::default(),
            mask_seed: NEXT_MASK_SEED.fetch_add(0x517c_c1b7_2722_0a95, Ordering::Relaxed),
            _shard: PhantomData,
        }
    }
}

impl<S: Shard + 'static> Isolate for WebSocketClientConnection<S> {
    tina::isolate_types! {
        message: WebSocketClientMsg,
        reply: WebSocketClientReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: tina_runtime::RuntimeCall<WebSocketClientMsg>,
        fact: ProtocolFact,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: WebSocketClientMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WebSocketClientMsg::Connected(Ok((stream, _, _))) => {
                self.handle_connected(ClientTransport::Tcp(stream))
            }
            WebSocketClientMsg::Connected(Err(_)) => {
                self.fail_connect(WebSocketClientError::Connect)
            }
            WebSocketClientMsg::TlsConnected(Ok(stream)) => {
                self.handle_connected(ClientTransport::Tls(stream))
            }
            WebSocketClientMsg::TlsConnected(Err(error)) => {
                self.fail_connect(WebSocketClientError::Tls(error))
            }
            WebSocketClientMsg::Wrote(Ok(reply)) => self.handle_wrote(reply),
            WebSocketClientMsg::Wrote(Err(_)) => self.close_with(WebSocketCloseReason::Abnormal),
            WebSocketClientMsg::Read(Ok(reply)) => self.handle_read(reply),
            WebSocketClientMsg::Read(Err(_)) => self.close_with(WebSocketCloseReason::Abnormal),
            WebSocketClientMsg::Closed(_) => noop(),
            WebSocketClientMsg::Stop => self.close_with(WebSocketCloseReason::LocalInitiated),
            WebSocketClientMsg::Connect { .. }
            | WebSocketClientMsg::Send(_)
            | WebSocketClientMsg::Receive
            | WebSocketClientMsg::Report => self.wrong_lane_message(),
        }
    }

    fn handle_call(
        &mut self,
        msg: WebSocketClientMsg,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            WebSocketClientMsg::Connect {
                target,
                subprotocols,
            } => self.handle_connect(target, subprotocols, Some(call.into_request_context())),
            WebSocketClientMsg::Send(message) => self.handle_send(message, call),
            WebSocketClientMsg::Receive => self.handle_receive(call),
            WebSocketClientMsg::Report => call.reply(WebSocketClientReply::Report(self.snapshot())),
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl<S: Shard + 'static> WebSocketClientConnection<S> {
    fn handle_connect(
        &mut self,
        target: WebSocketTarget,
        subprotocols: Vec<String>,
        reply_to: Option<RequestContext<WebSocketClientReply>>,
    ) -> Effect<Self> {
        if self.state != WebSocketClientState::Idle && self.state != WebSocketClientState::Closed {
            return reply_to_ws(
                reply_to,
                WebSocketClientReply::Connected(Err(WebSocketClientError::Busy)),
            );
        }
        if !valid_target(&target) || !subprotocols.iter().all(|p| is_subprotocol_token(p)) {
            return reply_to_ws(
                reply_to,
                WebSocketClientReply::Connected(Err(WebSocketClientError::InvalidTarget)),
            );
        }
        let key = client_key(self.mask_seed);
        let request = encode_upgrade_request(&target, &key, &subprotocols);
        self.state = WebSocketClientState::Connecting;
        self.close_sent = false;
        self.close_received = false;
        self.selected_subprotocol = None;
        self.events.clear();
        self.pending_write.clear();
        self.write_queue.clear();
        self.queued_write_bytes = 0;
        self.read_buf.clear();
        self.report.state = self.state;
        self.pending_connect = Some(PendingConnect {
            key,
            subprotocols,
            reply_to,
        });
        self.pending_write = request;
        match target {
            WebSocketTarget::Ws { addr, .. } => {
                tcp_connect(addr).then(WebSocketClientMsg::Connected)
            }
            WebSocketTarget::Wss {
                addr,
                server_name,
                trust_roots,
                ..
            } => tls_connect(
                addr,
                server_name,
                trust_roots.root_certificates_der,
                self.limits.ping_pong_timeout,
            )
            .then(WebSocketClientMsg::TlsConnected),
        }
    }

    fn handle_connected(&mut self, transport: ClientTransport) -> Effect<Self> {
        self.transport = Some(transport);
        self.write_more()
    }

    fn handle_wrote(&mut self, reply: TcpWriteOwnedReply) -> Effect<Self> {
        self.write_in_flight = false;
        let TcpWriteOwnedReply { mut bytes, written } = reply;
        let drain = written.min(bytes.len());
        bytes.drain(..drain);
        self.pending_write = bytes;
        if !self.pending_write.is_empty() {
            return self.write_more();
        }
        if let Some(next) = self.write_queue.pop_front() {
            self.queued_write_bytes = self.queued_write_bytes.saturating_sub(next.len());
            self.pending_write = next;
            return self.write_more();
        }
        if self.state == WebSocketClientState::Connecting || self.should_read_more() {
            self.read_more()
        } else if self.close_sent {
            self.state = WebSocketClientState::Closed;
            self.report.state = self.state;
            self.close_transport()
        } else {
            noop()
        }
    }

    fn handle_read(&mut self, reply: TcpReadBufReply) -> Effect<Self> {
        self.read_in_flight = false;
        let TcpReadBufReply { buffer, len } = reply;
        if len == 0 {
            self.read_scratch = buffer;
            return self.close_with(WebSocketCloseReason::GoingAway);
        }
        self.read_buf.extend_from_slice(&buffer[..len]);
        self.read_scratch = buffer;
        if self.state == WebSocketClientState::Connecting {
            return self.try_finish_handshake();
        }
        self.drain_frames()
    }

    fn try_finish_handshake(&mut self) -> Effect<Self> {
        match parse_response_head(&self.read_buf, &self.http_limits) {
            ResponseParseProgress::NeedMore => self.read_more(),
            ResponseParseProgress::Failed(_) => {
                self.fail_connect(WebSocketClientError::BadHandshake)
            }
            ResponseParseProgress::Complete { head, head_len } => {
                let Some(pending) = self.pending_connect.take() else {
                    return self.close_with(WebSocketCloseReason::ProtocolError);
                };
                let expected = crate::websocket::websocket_accept_key_for_client(&pending.key);
                if head.status != StatusCode::SWITCHING_PROTOCOLS {
                    return self
                        .fail_connect_reply(WebSocketClientError::BadHandshake, pending.reply_to);
                }
                if !header_has_token(&head.headers, http::header::UPGRADE, "websocket")
                    || !header_has_token(&head.headers, http::header::CONNECTION, "upgrade")
                {
                    return self
                        .fail_connect_reply(WebSocketClientError::BadHandshake, pending.reply_to);
                }
                if head
                    .headers
                    .get(SEC_WEBSOCKET_ACCEPT)
                    .and_then(|v| v.to_str().ok())
                    != Some(expected.as_str())
                {
                    return self.fail_connect_reply(
                        WebSocketClientError::AcceptMismatch,
                        pending.reply_to,
                    );
                }
                let selected = head
                    .headers
                    .get(SEC_WEBSOCKET_PROTOCOL)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                if let Some(selected) = &selected {
                    if !pending.subprotocols.iter().any(|p| p == selected) {
                        return self.fail_connect_reply(
                            WebSocketClientError::UnsupportedSubprotocol,
                            pending.reply_to,
                        );
                    }
                }
                self.read_buf.drain(..head_len);
                self.state = WebSocketClientState::Open;
                self.report.state = self.state;
                self.selected_subprotocol = selected.clone();
                let connected = WebSocketClientConnected {
                    selected_subprotocol: selected,
                };
                batch(vec![
                    reply_to_ws(
                        pending.reply_to,
                        WebSocketClientReply::Connected(Ok(connected)),
                    ),
                    self.drain_frames(),
                ])
            }
        }
    }

    fn handle_send(
        &mut self,
        message: WebSocketMessage,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        if self.state == WebSocketClientState::Closed {
            return call.reply(WebSocketClientReply::Sent(Err(
                WebSocketClientError::Closed,
            )));
        }
        if self.state != WebSocketClientState::Open {
            return call.reply(WebSocketClientReply::Sent(Err(
                WebSocketClientError::NotConnected,
            )));
        }
        if let WebSocketMessage::Close(code, reason) = message {
            return batch(vec![
                call.reply(WebSocketClientReply::Sent(Ok(()))),
                self.enqueue_close(code, reason),
            ]);
        }
        match self.enqueue_message(message) {
            Ok(()) => batch(vec![
                call.reply(WebSocketClientReply::Sent(Ok(()))),
                self.write_more(),
            ]),
            Err(error) => call.reply(WebSocketClientReply::Sent(Err(
                WebSocketClientError::Protocol(error),
            ))),
        }
    }

    fn handle_receive(&mut self, call: CallContext<'_, Self>) -> Effect<Self> {
        if let Some(event) = self.events.pop_front() {
            return call.reply(WebSocketClientReply::Event(Ok(event)));
        }
        if self.state == WebSocketClientState::Closed {
            return call.reply(WebSocketClientReply::Event(Err(
                WebSocketClientError::Closed,
            )));
        }
        if matches!(
            self.state,
            WebSocketClientState::Idle | WebSocketClientState::Connecting
        ) {
            return call.reply(WebSocketClientReply::Event(Err(
                WebSocketClientError::NotConnected,
            )));
        }
        if self.pending_receive.is_some() {
            return call.reply(WebSocketClientReply::Event(Err(WebSocketClientError::Busy)));
        }
        self.pending_receive = Some(call.into_request_context());
        self.read_more()
    }

    fn drain_frames(&mut self) -> Effect<Self> {
        let mut effects = Vec::new();
        loop {
            match parse_server_frame(&mut self.read_buf, self.limits) {
                FrameParse::NeedMore => break,
                FrameParse::Error(error) => return self.fail_protocol(error),
                FrameParse::Frame(frame) => match frame.opcode {
                    0x1 | 0x2 if frame.fin => {
                        if self.fragmented_message.is_some() {
                            return self.fail_protocol(WebSocketError::ProtocolError);
                        }
                        if let Err(error) = self.push_data_message(frame.opcode, frame.payload) {
                            return self.fail_protocol(error);
                        }
                    }
                    0x1 | 0x2 => {
                        if self.fragmented_message.is_some() {
                            return self.fail_protocol(WebSocketError::ProtocolError);
                        }
                        self.fragmented_message = Some(ClientFragment {
                            opcode: frame.opcode,
                            payload: frame.payload,
                        });
                    }
                    0x0 => {
                        let complete = {
                            let Some(fragment) = self.fragmented_message.as_mut() else {
                                return self.fail_protocol(WebSocketError::ProtocolError);
                            };
                            let next_len =
                                match fragment.payload.len().checked_add(frame.payload.len()) {
                                    Some(len) => len,
                                    None => {
                                        return self.fail_protocol(WebSocketError::MessageTooLarge);
                                    }
                                };
                            if next_len > self.limits.max_message_bytes {
                                return self.fail_protocol(WebSocketError::MessageTooLarge);
                            }
                            fragment.payload.extend_from_slice(&frame.payload);
                            if frame.fin {
                                self.fragmented_message
                                    .take()
                                    .map(|fragment| (fragment.opcode, fragment.payload))
                            } else {
                                None
                            }
                        };
                        if let Some((opcode, payload)) = complete {
                            if let Err(error) = self.push_data_message(opcode, payload) {
                                return self.fail_protocol(error);
                            }
                        }
                    }
                    0x8 => {
                        self.close_received = true;
                        self.report.close_received = true;
                        let (code, reason) = match decode_close_payload(&frame.payload) {
                            Ok(decoded) => decoded,
                            Err(error) => return self.fail_protocol(error),
                        };
                        self.push_event(WebSocketClientEvent::Close { code, reason });
                        effects.push(self.enqueue_close(code, Vec::new()));
                        effects
                            .push(self.close_fact(WebSocketCloseReason::Normal, code.map(|c| c.0)));
                        self.state = WebSocketClientState::Closing;
                        self.report.state = self.state;
                        break;
                    }
                    0x9 => {
                        self.report.pings_received += 1;
                        let payload = frame.payload.clone();
                        self.push_event(WebSocketClientEvent::Ping(frame.payload.clone()));
                        effects.push(
                            match self.enqueue_message(WebSocketMessage::Pong(payload)) {
                                Ok(()) => self.write_more(),
                                Err(error) => self.fail_protocol(error),
                            },
                        );
                    }
                    0xA => {
                        self.report.pongs_received += 1;
                        self.push_event(WebSocketClientEvent::Pong(frame.payload));
                    }
                    _ => return self.close_with(WebSocketCloseReason::ProtocolError),
                },
            }
        }
        effects.push(self.deliver_or_read_more());
        batch(effects)
    }

    fn push_data_message(&mut self, opcode: u8, payload: Vec<u8>) -> Result<(), WebSocketError> {
        match opcode {
            0x1 => match String::from_utf8(payload) {
                Ok(text) => {
                    self.push_event(WebSocketClientEvent::Text(text));
                    Ok(())
                }
                Err(_) => Err(WebSocketError::ProtocolError),
            },
            0x2 => {
                self.push_event(WebSocketClientEvent::Binary(payload));
                Ok(())
            }
            _ => Err(WebSocketError::ProtocolError),
        }
    }

    fn push_event(&mut self, event: WebSocketClientEvent) {
        self.report.received_messages += 1;
        self.events.push_back(event);
        self.report.high_water_queued_events =
            self.report.high_water_queued_events.max(self.events.len());
    }

    fn deliver_or_read_more(&mut self) -> Effect<Self> {
        if let Some(pending) = self.pending_receive.take() {
            if let Some(event) = self.events.pop_front() {
                return reply_to(pending, WebSocketClientReply::Event(Ok(event)));
            }
            self.pending_receive = Some(pending);
        }
        if self.should_read_more() {
            self.read_more()
        } else {
            self.write_more()
        }
    }

    fn enqueue_message(&mut self, message: WebSocketMessage) -> Result<(), WebSocketError> {
        let bytes = encode_client_message(message, self.next_mask())?;
        if self.pending_write.is_empty() && !self.write_in_flight {
            self.report.high_water_outbound_bytes =
                self.report.high_water_outbound_bytes.max(bytes.len());
            self.pending_write = bytes;
        } else {
            if self.write_queue.len() >= self.limits.outbound_frame_queue_capacity {
                self.report.outbound_queue_full += 1;
                return Err(WebSocketError::OutboundQueueFull);
            }
            let next = match self.queued_write_bytes.checked_add(bytes.len()) {
                Some(next) => next,
                None => {
                    self.report.outbound_bytes_full += 1;
                    return Err(WebSocketError::OutboundBytesFull);
                }
            };
            if next > self.limits.max_queued_outbound_bytes {
                self.report.outbound_bytes_full += 1;
                return Err(WebSocketError::OutboundBytesFull);
            }
            self.queued_write_bytes = next;
            self.report.high_water_outbound_bytes = self.report.high_water_outbound_bytes.max(next);
            self.write_queue.push_back(bytes);
        }
        self.report.sent_messages += 1;
        Ok(())
    }

    fn enqueue_close(&mut self, code: Option<WebSocketCloseCode>, reason: Vec<u8>) -> Effect<Self> {
        self.close_sent = true;
        self.report.close_sent = true;
        self.state = WebSocketClientState::Closing;
        self.report.state = self.state;
        match self.enqueue_message(WebSocketMessage::Close(code, reason)) {
            Ok(()) => self.write_more(),
            Err(error) => self.fail_protocol(error),
        }
    }

    fn fail_protocol(&mut self, error: WebSocketError) -> Effect<Self> {
        self.report.protocol_errors += 1;
        self.push_event(WebSocketClientEvent::Close {
            code: Some(WebSocketCloseCode(1002)),
            reason: format!("{error:?}").into_bytes(),
        });
        self.state = WebSocketClientState::Closed;
        self.report.state = self.state;
        batch(vec![
            self.deliver_or_read_more(),
            self.close_fact(WebSocketCloseReason::ProtocolError, Some(1002)),
            self.close_transport(),
        ])
    }

    fn write_more(&mut self) -> Effect<Self> {
        if self.write_in_flight || self.pending_write.is_empty() {
            return noop();
        }
        let Some(transport) = self.transport else {
            return noop();
        };
        self.write_in_flight = true;
        let bytes = std::mem::take(&mut self.pending_write);
        match transport {
            ClientTransport::Tcp(stream) => tcp_write_owned(stream, bytes)
                .then(|result| WebSocketClientMsg::Wrote(result.map_err(|error| error.error))),
            ClientTransport::Tls(stream) => {
                tls_write_owned(stream, bytes, self.limits.ping_pong_timeout).then(|result| {
                    WebSocketClientMsg::Wrote(
                        result
                            .map(tls_write_reply_to_tcp)
                            .map_err(|error| error.error),
                    )
                })
            }
        }
    }

    fn read_more(&mut self) -> Effect<Self> {
        if self.read_in_flight || self.write_in_flight {
            return noop();
        }
        let Some(transport) = self.transport else {
            return noop();
        };
        self.read_in_flight = true;
        let buffer = std::mem::take(&mut self.read_scratch);
        match transport {
            ClientTransport::Tcp(stream) => tcp_read_buf(stream, buffer, READ_CHUNK)
                .then(|result| WebSocketClientMsg::Read(result.map_err(|error| error.error))),
            ClientTransport::Tls(stream) => {
                tls_read_buf(stream, buffer, READ_CHUNK, self.limits.ping_pong_timeout).then(
                    |result| {
                        WebSocketClientMsg::Read(
                            result
                                .map(tls_read_reply_to_tcp)
                                .map_err(|error| error.error),
                        )
                    },
                )
            }
        }
    }

    fn should_read_more(&self) -> bool {
        if self.state != WebSocketClientState::Open {
            return false;
        }
        self.transport.is_some() && self.pending_receive.is_some()
    }

    fn fail_connect(&mut self, error: WebSocketClientError) -> Effect<Self> {
        let reply_to = self.pending_connect.take().and_then(|p| p.reply_to);
        self.fail_connect_reply(error, reply_to)
    }

    fn fail_connect_reply(
        &mut self,
        error: WebSocketClientError,
        reply_to: Option<RequestContext<WebSocketClientReply>>,
    ) -> Effect<Self> {
        self.state = WebSocketClientState::Closed;
        self.report.state = self.state;
        batch(vec![
            reply_to_ws(reply_to, WebSocketClientReply::Connected(Err(error))),
            self.close_transport(),
        ])
    }

    fn close_with(&mut self, reason: WebSocketCloseReason) -> Effect<Self> {
        self.state = WebSocketClientState::Closed;
        self.report.state = self.state;
        let event_reply = self.pending_receive.take().map(|pending| {
            reply_to(
                pending,
                WebSocketClientReply::Event(Err(WebSocketClientError::Closed)),
            )
        });
        let mut effects = vec![self.close_fact(reason, None), self.close_transport()];
        if let Some(reply) = event_reply {
            effects.push(reply);
        }
        batch(effects)
    }

    fn close_transport(&mut self) -> Effect<Self> {
        let Some(transport) = self.transport.take() else {
            return noop();
        };
        match transport {
            ClientTransport::Tcp(stream) => {
                tcp_close_stream(stream).then(WebSocketClientMsg::Closed)
            }
            ClientTransport::Tls(stream) => {
                tls_close(stream, self.limits.ping_pong_timeout).then(WebSocketClientMsg::Closed)
            }
        }
    }

    fn close_fact(&self, reason: WebSocketCloseReason, code: Option<u16>) -> Effect<Self> {
        tina::fact::<Self>(ProtocolFact::WebSocketSessionClosed {
            session: tina_runtime::WebSocketSessionId::new(self.mask_seed),
            reason,
            code,
        })
    }

    fn snapshot(&self) -> WebSocketClientReport {
        let mut report = self.report.clone();
        report.state = self.state;
        report.queued_events = self.events.len();
        report.queued_outbound_frames =
            self.write_queue.len() + usize::from(!self.pending_write.is_empty());
        report.queued_outbound_bytes = self.queued_write_bytes + self.pending_write.len();
        report.close_sent = self.close_sent;
        report.close_received = self.close_received;
        report
    }

    fn wrong_lane_message(&mut self) -> Effect<Self> {
        self.report.wrong_lane_messages = self.report.wrong_lane_messages.saturating_add(1);
        noop()
    }

    fn next_mask(&mut self) -> [u8; 4] {
        self.mask_seed = self
            .mask_seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let bytes = self.mask_seed.to_be_bytes();
        [bytes[0], bytes[2], bytes[5], bytes[7]]
    }
}

fn reply_to_ws<S: Shard + 'static>(
    request: Option<RequestContext<WebSocketClientReply>>,
    reply_msg: WebSocketClientReply,
) -> Effect<WebSocketClientConnection<S>> {
    match request {
        Some(request) => reply_to(request, reply_msg),
        None => reply(reply_msg),
    }
}

fn client_key(seed: u64) -> String {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&seed.to_be_bytes());
    bytes[8..].copy_from_slice(&seed.rotate_left(17).to_be_bytes());
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn encode_upgrade_request(target: &WebSocketTarget, key: &str, subprotocols: &[String]) -> Vec<u8> {
    let (host, path) = match target {
        WebSocketTarget::Ws { host, path, .. } | WebSocketTarget::Wss { host, path, .. } => {
            (host, path)
        }
    };
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    );
    if !subprotocols.is_empty() {
        request.push_str("Sec-WebSocket-Protocol: ");
        request.push_str(&subprotocols.join(", "));
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request.into_bytes()
}

fn valid_target(target: &WebSocketTarget) -> bool {
    let (host, path) = match target {
        WebSocketTarget::Ws { host, path, .. } | WebSocketTarget::Wss { host, path, .. } => {
            (host, path)
        }
    };
    !host.is_empty()
        && !host.bytes().any(|b| matches!(b, b'\r' | b'\n'))
        && path.starts_with('/')
        && !path.bytes().any(|b| matches!(b, b'\r' | b'\n'))
}

fn header_has_token(headers: &http::HeaderMap, name: http::HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().ok().is_some_and(|s| {
            s.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + payload.len());
        out.push(if fin { 0x80 | opcode } else { opcode });
        out.push(payload.len() as u8);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn wrong_lane_message_is_counted_in_release_path() {
        let mut client =
            WebSocketClientConnection::<tina::SingleShard>::new(WebSocketLimits::default());

        let _ = client.wrong_lane_message();
        let _ = client.wrong_lane_message();

        assert_eq!(client.snapshot().wrong_lane_messages, 2);
    }

    #[test]
    fn fragmented_text_reassembles_with_interleaved_ping() {
        let mut client =
            WebSocketClientConnection::<tina::SingleShard>::new(WebSocketLimits::default());
        client.state = WebSocketClientState::Open;
        client
            .read_buf
            .extend_from_slice(&server_frame(false, 0x1, b"hel"));
        client
            .read_buf
            .extend_from_slice(&server_frame(true, 0x9, b"p"));
        client
            .read_buf
            .extend_from_slice(&server_frame(false, 0x0, b"lo"));
        client
            .read_buf
            .extend_from_slice(&server_frame(true, 0x0, b"!"));

        let _ = client.drain_frames();

        assert_eq!(client.state, WebSocketClientState::Open);
        assert!(client.fragmented_message.is_none());
        let text_events = client
            .events
            .iter()
            .filter(|event| matches!(event, WebSocketClientEvent::Text(_)))
            .count();
        assert_eq!(text_events, 1, "fragments produce one text message");
        assert!(matches!(
            client.events.pop_front(),
            Some(WebSocketClientEvent::Ping(bytes)) if bytes == b"p"
        ));
        assert!(matches!(
            client.events.pop_front(),
            Some(WebSocketClientEvent::Text(text)) if text == "hello!"
        ));
    }

    #[test]
    fn oversized_fragmented_message_is_protocol_error() {
        let limits = WebSocketLimits {
            max_message_bytes: 4,
            ..WebSocketLimits::default()
        };
        let mut client = WebSocketClientConnection::<tina::SingleShard>::new(limits);
        client.state = WebSocketClientState::Open;
        client
            .read_buf
            .extend_from_slice(&server_frame(false, 0x1, b"hel"));
        client
            .read_buf
            .extend_from_slice(&server_frame(true, 0x0, b"lo"));

        let _ = client.drain_frames();

        assert_eq!(client.state, WebSocketClientState::Closed);
        assert_eq!(client.snapshot().protocol_errors, 1);
        assert!(matches!(
            client.events.pop_front(),
            Some(WebSocketClientEvent::Close {
                code: Some(WebSocketCloseCode(1002)),
                ..
            })
        ));
    }
}
