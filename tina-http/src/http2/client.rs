//! Native HTTP/2 client connection (first form).
//!
//! One isolate owns one TCP stream to a single remote authority and
//! carries many admitted client streams over it. Admission is bounded by
//! `max_concurrent_streams`. Each stream completes with one typed
//! [`Http2ClientOutcome`] reply back to the caller's request slot.
//!
//! Scope of this first form:
//! - prior-knowledge cleartext h2c only
//! - buffered request body, buffered response body under explicit caps
//! - SETTINGS / PING / HEADERS / DATA / WINDOW_UPDATE / RST_STREAM /
//!   GOAWAY frame handling, sharing the helpers in `super::frame` /
//!   `super::headers` / `super::errors` with the server
//! - typed `Http2ClientOutcome` covers replied, full, closed, timeout,
//!   reset, protocol error, local cancel, and `TlsAlpnMismatch`
//!
//! `Http2Target::Tls { .. }` is recognized but resolves to
//! [`Http2ClientOutcome::TlsAlpnMismatch`] until the typed ALPN rail
//! lands on the runtime. The client never silently downgrades to h2c.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::marker::PhantomData;

use http::{HeaderMap, Method, StatusCode};
use tina::prelude::*;
use tina::reply_to_request;
use tina_runtime::{
    CallError, Http2CloseReason, Http2ResetReason, Http2StreamId, ProtocolConnectionId,
    ProtocolDirection, ProtocolFact, StreamId, tcp_close_stream, tcp_connect, tcp_read, tcp_write,
};

use super::errors::{
    ERR_CANCEL, ERR_FLOW_CONTROL_ERROR, ERR_FRAME_SIZE_ERROR, ERR_NO_ERROR, ERR_PROTOCOL_ERROR,
    ERR_SETTINGS_ERROR, ERR_STREAM_CLOSED, Http2ProtocolError, classify_h2_reset,
};
use super::frame::{
    CLIENT_PREFACE, DEFAULT_WINDOW, FLAG_ACK, FLAG_END_HEADERS, FLAG_END_STREAM, FRAME_DATA,
    FRAME_GOAWAY, FRAME_HEADERS, FRAME_PING, FRAME_RST_STREAM, FRAME_SETTINGS, FRAME_WINDOW_UPDATE,
    Frame, READ_CHUNK, WINDOW_CREDIT_FLUSH_THRESHOLD, add_window, data_frame, data_payload,
    goaway_frame, headers_frame, headers_payload, rst_stream_frame, settings_frame,
    try_decode_frame, window_update_frame,
};
use super::headers::{
    DEFAULT_HEADER_TABLE_SIZE, HeaderBlock, MAX_MAX_FRAME_SIZE, MIN_MAX_FRAME_SIZE,
    SETTINGS_ENABLE_PUSH, SETTINGS_HEADER_TABLE_SIZE, SETTINGS_INITIAL_WINDOW_SIZE,
    SETTINGS_MAX_CONCURRENT_STREAMS, SETTINGS_MAX_FRAME_SIZE, SETTINGS_MAX_HEADER_LIST_SIZE,
    decode_headers_block_with, encode_literal_header, validate_response_headers,
    validate_trailer_block,
};
use super::target::Http2Target;

/// Client connection limits. Mirrors the server's [`super::Http2Limits`]
/// shape but is typed separately so the client picks its own defaults.
///
/// Not `#[non_exhaustive]` because callers construct this directly with
/// struct-update syntax. New fields go through a major-version bump.
#[derive(Debug, Clone, Copy)]
pub struct Http2ClientLimits {
    pub max_frame_size: usize,
    pub max_header_bytes: usize,
    pub max_concurrent_streams: usize,
    pub max_response_body_bytes: usize,
    /// Bounded outbound frame queue length. Submits that arrive when the
    /// queue is full are rejected with [`Http2ClientOutcome::Full`] —
    /// this is the "bounded admission" guarantee for the write path.
    pub connection_outbound_queue_capacity: usize,
    /// Pre-connect submit queue. Submits that arrive before TCP+preface
    /// flush are queued here, up to this cap, then rejected with
    /// [`Http2ClientOutcome::Full`].
    pub pre_connect_submit_capacity: usize,
    pub initial_connection_window: i32,
    pub initial_stream_window: i32,
}

impl Default for Http2ClientLimits {
    fn default() -> Self {
        Self {
            max_frame_size: 16 * 1024,
            max_header_bytes: 16 * 1024,
            max_concurrent_streams: 64,
            max_response_body_bytes: 1024 * 1024,
            connection_outbound_queue_capacity: 64,
            pre_connect_submit_capacity: 64,
            initial_connection_window: DEFAULT_WINDOW,
            initial_stream_window: DEFAULT_WINDOW,
        }
    }
}

/// Per-connection report counters.
#[non_exhaustive]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Http2ClientReport {
    pub opened_streams: u64,
    pub closed_streams: u64,
    pub reset_streams: u64,
    pub admission_full: u64,
    pub flow_control_blocked: u64,
    pub protocol_errors: u64,
    pub goaway_received: u64,
    pub locally_cancelled: u64,
    /// Outbound HEADERS rejected because they would not fit a single
    /// frame and CONTINUATION is not implemented in this slice.
    pub request_too_large: u64,
    /// Submits rejected because the outbound write queue was at the
    /// `connection_outbound_queue_capacity` cap.
    pub outbound_queue_full: u64,
    /// Streams parked waiting on flow-control credit before being
    /// admitted to the wire. Counts events, not currently-parked count.
    pub flow_control_parks: u64,
}

/// Typed first-form HTTP/2 client outcomes.
///
/// Two variants the model will eventually grow are intentionally absent
/// because no code path constructs them yet — advertising an outcome the
/// implementation never produces is a lying API:
///
/// - `Timeout` lands with a real stream-level deadline. Today callers
///   enforce their own timeout through `call_blocking_with_host_timeout`,
///   which surfaces as `CallOutcome::TimedOut` at the host level.
/// - `FlowControlBlocked` lands with the same deadline mechanism: a
///   stream parked on send-window credit past its deadline will give up
///   the slot and report this. Today parked streams wait indefinitely
///   for `WINDOW_UPDATE` (visible via `Http2ClientReport.flow_control_parks`),
///   and never surface a per-stream `FlowControlBlocked` outcome.
///
/// The enum is `#[non_exhaustive]`, so adding either back is not a
/// breaking change.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Http2ClientOutcome {
    Replied(Http2ClientResponse),
    Full,
    Closed,
    Reset(Http2ResetReason),
    LocalCancel,
    ProtocolError(Http2ProtocolError),
    TlsAlpnMismatch,
}

/// Buffered HTTP/2 client response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Http2ClientResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    /// Trailing HEADERS block. gRPC carries `grpc-status` here.
    pub trailers: HeaderMap,
}

/// One buffered request submitted to the client connection.
#[derive(Debug, Clone)]
pub struct Http2ClientRequest {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Http2ClientRequest {
    pub fn get(path: impl Into<String>) -> Self {
        Self {
            method: Method::GET,
            path: path.into(),
            headers: HeaderMap::new(),
            body: Vec::new(),
        }
    }

    pub fn post(path: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: Method::POST,
            path: path.into(),
            headers: HeaderMap::new(),
            body,
        }
    }
}

/// Messages handled by [`Http2ClientConnection`].
///
/// `Submit` and `Report` are **call-only**: they must be delivered with
/// `call` / `call_blocking`, which provide the reply channel the
/// connection answers on. Delivering them with `try_send` has no reply
/// channel — `Submit` would silently drop the request body — so the
/// connection `debug_assert!`s on the misuse and otherwise ignores it.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Http2ClientMsg {
    /// Begin the TCP connect, send client preface, send initial SETTINGS,
    /// start reading. Idempotent; later `Begin` messages are no-ops.
    Begin,
    /// Submit a buffered request as a new client stream. The connection
    /// captures the caller's request slot and replies later with one
    /// [`Http2ClientReply::Outcome`]. **Call-only** (see the type doc).
    Submit(Http2ClientRequest),
    /// Locally cancel an admitted stream by id. The connection emits
    /// RST_STREAM(CANCEL) on the wire and replies to the original
    /// submitter with [`Http2ClientOutcome::LocalCancel`].
    Cancel { stream_id: u32 },
    /// Snapshot the per-connection report.
    Report,
    /// Begin graceful shutdown (GOAWAY) and stop the isolate.
    Stop,
    /// Internal: TCP connect completion.
    Connected(Result<(StreamId, std::net::SocketAddr, std::net::SocketAddr), CallError>),
    /// Internal: TCP read completion.
    Read(Result<Vec<u8>, CallError>),
    /// Internal: TCP write completion.
    Wrote(Result<usize, CallError>),
    /// Internal: TCP close completion.
    Closed(Result<(), CallError>),
}

/// Replies returned to callers waiting on [`Http2ClientMsg::Submit`] /
/// [`Http2ClientMsg::Report`].
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Http2ClientReply {
    /// Final per-stream outcome. The `stream_id` is informational; the
    /// caller did not previously hand the connection a token.
    Outcome {
        stream_id: u32,
        outcome: Http2ClientOutcome,
    },
    /// Report snapshot.
    Report(Http2ClientReport),
}

#[derive(Debug)]
struct ActiveClientStream {
    id: u32,
    /// Caller awaiting the per-stream outcome. `Option` so we can take it
    /// when we replace the slot with `LocalCancel` or `Closed`.
    waiter: Option<tina::RequestContext<Http2ClientReply>>,
    response_status: Option<StatusCode>,
    response_headers: HeaderMap,
    response_body: Vec<u8>,
    response_trailers: HeaderMap,
    /// Set once response headers are parsed so a second HEADERS frame is
    /// interpreted as trailers.
    response_headers_seen: bool,
    recv_window: i32,
    send_window: i32,
    pending_recv_window_credit: u32,
    response_content_length: Option<usize>,
    /// Bytes of request body still to send. Drained as DATA frames are
    /// admitted under stream + connection flow-control credit.
    outbound_body: VecDeque<u8>,
}

impl ActiveClientStream {
    fn new(
        id: u32,
        recv_window: i32,
        send_window: i32,
        waiter: tina::RequestContext<Http2ClientReply>,
        outbound_body: Vec<u8>,
    ) -> Self {
        Self {
            id,
            waiter: Some(waiter),
            response_status: None,
            response_headers: HeaderMap::new(),
            response_body: Vec::new(),
            response_trailers: HeaderMap::new(),
            response_headers_seen: false,
            recv_window,
            send_window,
            pending_recv_window_credit: 0,
            response_content_length: None,
            outbound_body: VecDeque::from(outbound_body),
        }
    }
}

/// Native HTTP/2 client connection isolate.
pub struct Http2ClientConnection<S: Shard + 'static> {
    target: Http2Target,
    limits: Http2ClientLimits,
    stream: Option<StreamId>,
    /// Submits waiting for the TCP connect + preface to flush.
    queued_submits: VecDeque<(Http2ClientRequest, tina::RequestContext<Http2ClientReply>)>,
    streams: Vec<ActiveClientStream>,
    next_stream_id: u32,
    read_buf: Vec<u8>,
    hpack_decoder: hpack::Decoder<'static>,
    preface_sent: bool,
    peer_initial_stream_window: i32,
    peer_max_frame_size: usize,
    peer_max_concurrent_streams: Option<u32>,
    recv_window: i32,
    pending_recv_window_credit: u32,
    send_window: i32,
    goaway_received: bool,
    closing_after_write: bool,
    /// True from when a `tcp_write` is dispatched until its `Wrote`
    /// completion arrives. Mirror of the server's "no in-flight" pattern
    /// — back-to-back `write_more` while a write is in flight would
    /// return `CallError::ResourceBusy` from the driver and kill the
    /// connection.
    write_in_flight: bool,
    /// Stream-id space is exhausted; no further `admit_stream` calls
    /// will succeed. The connection still drains existing streams.
    stream_id_exhausted: bool,
    pending_write: Vec<u8>,
    write_queue: VecDeque<Vec<u8>>,
    report: Http2ClientReport,
    self_isolate_id: Option<tina::IsolateId>,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> Http2ClientConnection<S> {
    pub fn new(target: Http2Target, limits: Http2ClientLimits) -> Self {
        Self {
            target,
            limits,
            stream: None,
            queued_submits: VecDeque::new(),
            streams: Vec::with_capacity(limits.max_concurrent_streams),
            next_stream_id: 1,
            read_buf: Vec::new(),
            hpack_decoder: hpack::Decoder::new(),
            preface_sent: false,
            peer_initial_stream_window: DEFAULT_WINDOW,
            peer_max_frame_size: limits.max_frame_size,
            peer_max_concurrent_streams: None,
            recv_window: limits.initial_connection_window,
            pending_recv_window_credit: 0,
            send_window: DEFAULT_WINDOW,
            goaway_received: false,
            closing_after_write: false,
            write_in_flight: false,
            stream_id_exhausted: false,
            pending_write: Vec::new(),
            write_queue: VecDeque::new(),
            report: Http2ClientReport::default(),
            self_isolate_id: None,
            _shard: PhantomData,
        }
    }

    pub fn report(&self) -> &Http2ClientReport {
        &self.report
    }

    fn connection_fact_id(&self) -> ProtocolConnectionId {
        ProtocolConnectionId::new(self.self_isolate_id.map(|id| id.get()).unwrap_or_default())
    }
}

impl<S: Shard + 'static> Isolate for Http2ClientConnection<S> {
    tina::isolate_types! {
        message: Http2ClientMsg,
        reply: Http2ClientReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: tina_runtime::RuntimeCall<Http2ClientMsg>,
        fact: ProtocolFact,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: Http2ClientMsg,
        ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        if self.self_isolate_id.is_none() {
            self.self_isolate_id = Some(ctx.isolate_id());
        }
        match msg {
            Http2ClientMsg::Begin => self.begin_connect(),
            Http2ClientMsg::Connected(Ok((stream, _, _))) => self.handle_connected(stream),
            Http2ClientMsg::Connected(Err(_)) => self.close_with(Http2ClientOutcome::Closed),
            Http2ClientMsg::Read(Ok(bytes)) => self.handle_read(bytes),
            Http2ClientMsg::Read(Err(_)) => self.close_with(Http2ClientOutcome::Closed),
            Http2ClientMsg::Wrote(Ok(n)) => self.handle_wrote(n),
            Http2ClientMsg::Wrote(Err(_)) => self.close_with(Http2ClientOutcome::Closed),
            Http2ClientMsg::Closed(_) => stop(),
            Http2ClientMsg::Cancel { stream_id } => self.handle_cancel(stream_id),
            Http2ClientMsg::Stop => self.begin_goaway_shutdown(),
            // `Report` and `Submit` are call-only: they carry no reply
            // channel when delivered via `try_send`, so the caller would
            // never learn the outcome. `Submit` in particular would
            // silently drop the request body. Catch the misuse in
            // dev/test; a stray `try_send` must not kill the connection
            // (and its in-flight streams) in production, so release
            // builds drop it. See the doc comment on `Http2ClientMsg`.
            Http2ClientMsg::Report | Http2ClientMsg::Submit(_) => {
                debug_assert!(
                    false,
                    "Http2ClientMsg::Submit / ::Report are call-only; use \
                     `call` / `call_blocking`, not `try_send`",
                );
                noop()
            }
        }
    }

    fn handle_call(
        &mut self,
        msg: Http2ClientMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            Http2ClientMsg::Submit(req) => self.handle_submit(req, call),
            Http2ClientMsg::Report => call.reply(Http2ClientReply::Report(self.report.clone())),
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl<S: Shard + 'static> Http2ClientConnection<S> {
    fn begin_connect(&mut self) -> Effect<Self> {
        if self.preface_sent || self.stream.is_some() {
            return noop();
        }
        match &self.target {
            Http2Target::Tls { .. } => {
                // TLS ALPN rail is not yet on the runtime. Stay alive so
                // each later `Submit` call returns the typed
                // `TlsAlpnMismatch` outcome through `handle_submit`. No
                // silent h2c fallback, no TCP rail consulted.
                noop()
            }
            Http2Target::H2c { addr, .. } => {
                let addr = *addr;
                tcp_connect(addr).then(Http2ClientMsg::Connected)
            }
        }
    }

    fn handle_connected(&mut self, stream: StreamId) -> Effect<Self> {
        self.stream = Some(stream);
        let mut preface = Vec::with_capacity(CLIENT_PREFACE.len() + 64);
        preface.extend_from_slice(CLIENT_PREFACE);
        let mut settings_payload = Vec::with_capacity(24);
        push_setting(
            &mut settings_payload,
            SETTINGS_INITIAL_WINDOW_SIZE,
            self.limits.initial_stream_window as u32,
        );
        push_setting(
            &mut settings_payload,
            SETTINGS_MAX_FRAME_SIZE,
            self.limits.max_frame_size as u32,
        );
        push_setting(
            &mut settings_payload,
            SETTINGS_MAX_CONCURRENT_STREAMS,
            self.limits.max_concurrent_streams as u32,
        );
        push_setting(&mut settings_payload, SETTINGS_ENABLE_PUSH, 0);
        preface.extend_from_slice(&Frame::new(FRAME_SETTINGS, 0, 0, settings_payload).encode());
        let extra = self.limits.initial_connection_window - DEFAULT_WINDOW;
        if extra > 0 {
            preface.extend_from_slice(&window_update_frame(0, extra as u32).encode());
        }
        self.preface_sent = true;
        self.pending_write = preface;
        let mut effects: Vec<Effect<Self>> = Vec::new();
        let queued = std::mem::take(&mut self.queued_submits);
        for (req, waiter) in queued {
            self.admit_stream(req, waiter, &mut effects);
        }
        self.maybe_write_more(&mut effects);
        effects.push(self.read_more());
        batch(effects)
    }

    fn handle_submit(
        &mut self,
        req: Http2ClientRequest,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        // Promote the call into a request context so we can reply later
        // from a different handler turn.
        let waiter = call.into_request_context();
        if self.stream.is_none() && !matches!(self.target, Http2Target::Tls { .. }) {
            if self.queued_submits.len() >= self.limits.max_concurrent_streams {
                self.report.admission_full += 1;
                return reply_to_request::<Self>(
                    waiter,
                    Http2ClientReply::Outcome {
                        stream_id: 0,
                        outcome: Http2ClientOutcome::Full,
                    },
                );
            }
            self.queued_submits.push_back((req, waiter));
            return noop();
        }
        if matches!(self.target, Http2Target::Tls { .. }) {
            return reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome {
                    stream_id: 0,
                    outcome: Http2ClientOutcome::TlsAlpnMismatch,
                },
            );
        }
        let mut effects: Vec<Effect<Self>> = Vec::new();
        self.admit_stream(req, waiter, &mut effects);
        self.maybe_write_more(&mut effects);
        batch(effects)
    }

    fn admit_stream(
        &mut self,
        req: Http2ClientRequest,
        waiter: tina::RequestContext<Http2ClientReply>,
        effects: &mut Vec<Effect<Self>>,
    ) {
        if self.goaway_received || self.closing_after_write {
            self.report.admission_full += 1;
            effects.push(reject_outcome(waiter, 0, Http2ClientOutcome::Closed));
            return;
        }
        if self.stream_id_exhausted {
            self.report.admission_full += 1;
            effects.push(reject_outcome(
                waiter,
                0,
                Http2ClientOutcome::ProtocolError(Http2ProtocolError::StreamIdExhausted),
            ));
            return;
        }
        if self.streams.len() >= self.limits.max_concurrent_streams {
            self.report.admission_full += 1;
            effects.push(reject_outcome(waiter, 0, Http2ClientOutcome::Full));
            return;
        }
        if let Some(peer_cap) = self.peer_max_concurrent_streams {
            if (self.streams.len() as u32) >= peer_cap {
                self.report.admission_full += 1;
                effects.push(reject_outcome(waiter, 0, Http2ClientOutcome::Full));
                return;
            }
        }
        if self.write_queue.len() >= self.limits.connection_outbound_queue_capacity {
            self.report.outbound_queue_full += 1;
            effects.push(reject_outcome(waiter, 0, Http2ClientOutcome::Full));
            return;
        }
        let header_block = encode_request_headers(&self.target, &req);
        if header_block.len() > self.peer_max_frame_size {
            // CONTINUATION is not in this first form. Refuse without
            // consuming a stream id so the next admitted stream uses the
            // same id we would have used; report as `request_too_large`
            // rather than `protocol_errors` because the peer is innocent.
            self.report.request_too_large += 1;
            effects.push(reject_outcome(
                waiter,
                0,
                Http2ClientOutcome::ProtocolError(Http2ProtocolError::OutboundHeadersTooLarge),
            ));
            return;
        }
        let stream_id = self.next_stream_id;
        match self.next_stream_id.checked_add(2) {
            Some(next) => self.next_stream_id = next,
            None => {
                // This admit succeeds; mark the connection so that the
                // next admission fails closed instead of silently reusing
                // the same id.
                self.stream_id_exhausted = true;
            }
        }
        let end_stream = req.body.is_empty();
        self.enqueue_frame(headers_frame(stream_id, end_stream, header_block));
        let stream = ActiveClientStream::new(
            stream_id,
            self.limits.initial_stream_window,
            self.peer_initial_stream_window,
            waiter,
            req.body,
        );
        self.report.opened_streams += 1;
        effects.push(emit_fact(ProtocolFact::Http2StreamOpened {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            direction: ProtocolDirection::Outbound,
        }));
        self.streams.push(stream);
        // The body bytes (if any) ride out as `flush_outbound_data` finds
        // stream + connection send-window credit. RFC 9113 §6.9: we must
        // not exceed either window.
        self.flush_outbound_data();
    }

    /// Drain queued outbound DATA from each active stream subject to
    /// stream and connection send-window credit. Called after admission,
    /// after handling a peer WINDOW_UPDATE, and after settings that
    /// resized the initial window. Idempotent; safe to over-call.
    fn flush_outbound_data(&mut self) {
        if self.send_window <= 0 {
            return;
        }
        let max_chunk = self.limits.max_frame_size.min(self.peer_max_frame_size);
        if max_chunk == 0 {
            return;
        }
        // Round-robin over streams in admission order. Streams with no
        // outbound body are skipped without "blocked" accounting.
        let mut progressed = true;
        while progressed && self.send_window > 0 {
            progressed = false;
            for idx in 0..self.streams.len() {
                if self.send_window <= 0 {
                    break;
                }
                let stream = &mut self.streams[idx];
                if stream.outbound_body.is_empty() {
                    continue;
                }
                if stream.send_window <= 0 {
                    self.report.flow_control_parks += 1;
                    continue;
                }
                let credit = stream
                    .send_window
                    .min(self.send_window)
                    .min(max_chunk as i32) as usize;
                let credit = credit.min(stream.outbound_body.len());
                if credit == 0 {
                    continue;
                }
                let mut chunk = Vec::with_capacity(credit);
                for _ in 0..credit {
                    if let Some(byte) = stream.outbound_body.pop_front() {
                        chunk.push(byte);
                    }
                }
                let is_last = stream.outbound_body.is_empty();
                let n = chunk.len() as i32;
                stream.send_window -= n;
                self.send_window -= n;
                let stream_id = stream.id;
                self.enqueue_frame(data_frame(stream_id, is_last, chunk));
                progressed = true;
            }
        }
    }

    /// Locally cancel an admitted stream: send RST_STREAM(CANCEL), reply
    /// to the original submitter with `LocalCancel`, free the slot.
    fn handle_cancel(&mut self, stream_id: u32) -> Effect<Self> {
        let Some(idx) = self.streams.iter().position(|s| s.id == stream_id) else {
            return noop();
        };
        let mut effects: Vec<Effect<Self>> = Vec::new();
        let mut stream = self.streams.swap_remove(idx);
        self.enqueue_frame(rst_stream_frame(stream_id, ERR_CANCEL));
        self.report.locally_cancelled += 1;
        self.report.closed_streams += 1;
        effects.push(emit_fact(ProtocolFact::Http2StreamReset {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            direction: ProtocolDirection::Outbound,
            reason: Http2ResetReason::Cancel,
        }));
        if let Some(waiter) = stream.waiter.take() {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome {
                    stream_id,
                    outcome: Http2ClientOutcome::LocalCancel,
                },
            ));
        }
        self.maybe_write_more(&mut effects);
        batch(effects)
    }

    fn handle_read(&mut self, bytes: Vec<u8>) -> Effect<Self> {
        if bytes.is_empty() {
            return self.close_with(Http2ClientOutcome::Closed);
        }
        self.read_buf.extend_from_slice(&bytes);
        let mut effects: Vec<Effect<Self>> = Vec::new();
        let max_frame_size = self.limits.max_frame_size;
        loop {
            match try_decode_frame(&self.read_buf, max_frame_size) {
                Ok(Some((frame, used))) => {
                    self.read_buf.drain(..used);
                    if let Err(err) = self.handle_frame(frame, &mut effects) {
                        return self.protocol_error(err, effects);
                    }
                }
                Ok(None) => break,
                Err(err) => return self.protocol_error(err, effects),
            }
        }
        if self.pending_recv_window_credit >= WINDOW_CREDIT_FLUSH_THRESHOLD {
            let credit = self.pending_recv_window_credit;
            self.pending_recv_window_credit = 0;
            self.recv_window = self.recv_window.saturating_add(credit as i32);
            self.enqueue_frame(window_update_frame(0, credit));
        }
        self.maybe_write_more(&mut effects);
        if !self.closing_after_write {
            effects.push(self.read_more());
        }
        batch(effects)
    }

    fn handle_frame(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        match frame.ty {
            FRAME_SETTINGS => self.handle_settings(frame),
            FRAME_HEADERS => self.handle_headers(frame, effects),
            FRAME_DATA => self.handle_data(frame, effects),
            FRAME_WINDOW_UPDATE => self.handle_window_update(frame),
            FRAME_RST_STREAM => self.handle_rst_stream(frame, effects),
            FRAME_PING => self.handle_ping(frame),
            FRAME_GOAWAY => self.handle_goaway(frame, effects),
            _ => Ok(()),
        }
    }

    /// Handle an inbound GOAWAY. RFC 9113 §6.8: GOAWAY is sent on stream
    /// 0x0 and carries the last stream id the peer processed plus an
    /// error code. Streams we opened with an id *greater* than
    /// `last_stream_id` were not processed and must be failed so the
    /// caller can retry them on a fresh connection. A non-`NO_ERROR`
    /// code is surfaced as a typed reset reason on those streams; a
    /// clean `NO_ERROR` GOAWAY just stops new admission and lets the
    /// already-processed streams settle.
    fn handle_goaway(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if frame.stream_id != 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        // last_stream_id (4) + error code (4); additional debug data is
        // allowed and ignored.
        if frame.payload.len() < 8 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        let last_stream_id = u32::from_be_bytes([
            frame.payload[0] & 0x7f,
            frame.payload[1],
            frame.payload[2],
            frame.payload[3],
        ]);
        let error_code = u32::from_be_bytes([
            frame.payload[4],
            frame.payload[5],
            frame.payload[6],
            frame.payload[7],
        ]);
        self.goaway_received = true;
        self.report.goaway_received += 1;
        // Fail every stream the peer did not process (id > last_stream_id).
        // Those are safe to retry on a new connection.
        let refused_outcome = if error_code == ERR_NO_ERROR {
            Http2ClientOutcome::Closed
        } else {
            Http2ClientOutcome::Reset(classify_h2_reset(error_code))
        };
        let mut idx = 0;
        while idx < self.streams.len() {
            if self.streams[idx].id > last_stream_id {
                self.fail_stream(idx, refused_outcome.clone(), effects);
                // fail_stream swap_removes, so do not advance idx.
            } else {
                idx += 1;
            }
        }
        Ok(())
    }

    fn handle_settings(&mut self, frame: Frame) -> Result<(), Http2ProtocolError> {
        if frame.stream_id != 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        if frame.flags & FLAG_ACK != 0 {
            if !frame.payload.is_empty() {
                return Err(Http2ProtocolError::BadFrameLength);
            }
            return Ok(());
        }
        if frame.payload.len() % 6 != 0 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        for setting in frame.payload.chunks_exact(6) {
            let id = u16::from_be_bytes([setting[0], setting[1]]);
            let value = u32::from_be_bytes([setting[2], setting[3], setting[4], setting[5]]);
            self.apply_setting(id, value)?;
        }
        self.enqueue_frame(settings_frame(true));
        Ok(())
    }

    fn apply_setting(&mut self, id: u16, value: u32) -> Result<(), Http2ProtocolError> {
        match id {
            SETTINGS_HEADER_TABLE_SIZE => {
                if value != DEFAULT_HEADER_TABLE_SIZE {
                    return Err(Http2ProtocolError::SettingsUnsupported);
                }
            }
            SETTINGS_ENABLE_PUSH => {
                if value > 1 {
                    return Err(Http2ProtocolError::InvalidSettingsValue);
                }
            }
            SETTINGS_MAX_CONCURRENT_STREAMS => {
                self.peer_max_concurrent_streams = Some(value);
            }
            SETTINGS_INITIAL_WINDOW_SIZE => {
                if value > i32::MAX as u32 {
                    return Err(Http2ProtocolError::FlowControl);
                }
                let new_window = value as i32;
                let delta = i64::from(new_window) - i64::from(self.peer_initial_stream_window);
                for stream in &mut self.streams {
                    let next = i64::from(stream.send_window) + delta;
                    if next < i64::from(i32::MIN) || next > i64::from(i32::MAX) {
                        return Err(Http2ProtocolError::FlowControl);
                    }
                    stream.send_window = next as i32;
                }
                self.peer_initial_stream_window = new_window;
                // A larger initial window may unblock parked outbound DATA.
                self.flush_outbound_data();
            }
            SETTINGS_MAX_FRAME_SIZE => {
                if !(MIN_MAX_FRAME_SIZE..=MAX_MAX_FRAME_SIZE).contains(&value) {
                    return Err(Http2ProtocolError::BadFrameLength);
                }
                self.peer_max_frame_size = value as usize;
            }
            SETTINGS_MAX_HEADER_LIST_SIZE => {}
            _ => {}
        }
        Ok(())
    }

    fn handle_headers(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if frame.stream_id == 0 || frame.stream_id % 2 == 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        if frame.flags & FLAG_END_HEADERS == 0 {
            return Err(Http2ProtocolError::HpackUnsupported);
        }
        let Some(idx) = self.streams.iter().position(|s| s.id == frame.stream_id) else {
            return Err(Http2ProtocolError::BadStreamId);
        };
        let payload = headers_payload(&frame)?;
        let header_block = decode_headers_block_with(
            &mut self.hpack_decoder,
            payload,
            self.limits.max_header_bytes,
        )?;
        let end_stream = frame.flags & FLAG_END_STREAM != 0;
        if !self.streams[idx].response_headers_seen {
            validate_response_headers(&header_block)?;
            apply_response_headers(&mut self.streams[idx], header_block);
            self.streams[idx].response_headers_seen = true;
        } else {
            // RFC 9113 §8.1: trailers must arrive with END_STREAM and
            // must not contain pseudo-headers, `content-length`, or
            // connection-control headers.
            if !end_stream {
                return Err(Http2ProtocolError::InvalidTrailerPseudoHeader);
            }
            validate_trailer_block(&header_block)?;
            for (name, value) in header_block.headers.iter() {
                self.streams[idx]
                    .response_trailers
                    .append(name.clone(), value.clone());
            }
        }
        if end_stream {
            self.complete_stream(idx, effects);
        }
        Ok(())
    }

    fn handle_data(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if frame.stream_id == 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        let payload = data_payload(&frame)?;
        let payload_len = payload.len();
        let len_i32 = i32::try_from(payload_len).map_err(|_| Http2ProtocolError::FlowControl)?;
        if self.recv_window < len_i32 {
            self.report.flow_control_blocked += 1;
            return Err(Http2ProtocolError::FlowControl);
        }
        // Always count DATA on the connection window per RFC 9113 §6.9.1,
        // even for closed streams; we still pay the connection-level
        // accounting and credit it back via WINDOW_UPDATE.
        self.recv_window -= len_i32;
        self.pending_recv_window_credit = self
            .pending_recv_window_credit
            .saturating_add(payload_len as u32);
        let stream_id = frame.stream_id;
        let Some(idx) = self.streams.iter().position(|s| s.id == stream_id) else {
            // DATA for an unknown / closed stream is a stream-level error,
            // not a connection-level one (RFC 9113 §6.9.1 / §5.1). Send
            // RST_STREAM and continue the connection.
            self.enqueue_frame(rst_stream_frame(stream_id, ERR_STREAM_CLOSED));
            return Ok(());
        };
        // Per RFC 9113 §8.1: DATA before HEADERS on this stream is a
        // connection-level PROTOCOL_ERROR. Fail closed.
        if !self.streams[idx].response_headers_seen {
            return Err(Http2ProtocolError::DataBeforeHeaders);
        }
        if self.streams[idx].recv_window < len_i32 {
            self.report.flow_control_blocked += 1;
            return Err(Http2ProtocolError::FlowControl);
        }
        self.streams[idx].recv_window -= len_i32;
        if self.streams[idx].response_body.len() + payload_len > self.limits.max_response_body_bytes
        {
            let cap_bytes = self.limits.max_response_body_bytes;
            self.enqueue_frame(rst_stream_frame(stream_id, ERR_PROTOCOL_ERROR));
            self.fail_stream(
                idx,
                Http2ClientOutcome::ProtocolError(Http2ProtocolError::BodyTooLarge { cap_bytes }),
                effects,
            );
            return Ok(());
        }
        self.streams[idx].response_body.extend_from_slice(&payload);
        self.streams[idx].pending_recv_window_credit = self.streams[idx]
            .pending_recv_window_credit
            .saturating_add(payload_len as u32);
        if self.streams[idx].pending_recv_window_credit >= WINDOW_CREDIT_FLUSH_THRESHOLD {
            let credit = self.streams[idx].pending_recv_window_credit;
            self.streams[idx].pending_recv_window_credit = 0;
            self.streams[idx].recv_window =
                self.streams[idx].recv_window.saturating_add(credit as i32);
            self.enqueue_frame(window_update_frame(stream_id, credit));
        }
        if frame.flags & FLAG_END_STREAM != 0 {
            if let Some(declared) = self.streams[idx].response_content_length {
                if declared != self.streams[idx].response_body.len() {
                    return Err(Http2ProtocolError::ContentLengthMismatch);
                }
            }
            self.complete_stream(idx, effects);
        }
        Ok(())
    }

    fn handle_window_update(&mut self, frame: Frame) -> Result<(), Http2ProtocolError> {
        if frame.payload.len() != 4 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        let increment = u32::from_be_bytes([
            frame.payload[0] & 0x7f,
            frame.payload[1],
            frame.payload[2],
            frame.payload[3],
        ]);
        if increment == 0 {
            return Err(Http2ProtocolError::WindowOverflow);
        }
        if frame.stream_id == 0 {
            self.send_window = add_window(self.send_window, increment)?;
        } else if let Some(idx) = self.streams.iter().position(|s| s.id == frame.stream_id) {
            self.streams[idx].send_window = add_window(self.streams[idx].send_window, increment)?;
        }
        // New credit may unblock parked outbound DATA on any stream.
        self.flush_outbound_data();
        Ok(())
    }

    fn handle_rst_stream(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        // RFC 9113 §6.4: RST_STREAM MUST be associated with a stream; a
        // RST_STREAM on stream 0x0 is a connection-level PROTOCOL_ERROR.
        if frame.stream_id == 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        if frame.payload.len() != 4 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        let code = u32::from_be_bytes([
            frame.payload[0],
            frame.payload[1],
            frame.payload[2],
            frame.payload[3],
        ]);
        let Some(idx) = self.streams.iter().position(|s| s.id == frame.stream_id) else {
            // RST_STREAM for an already-closed stream is allowed; ignore.
            return Ok(());
        };
        self.report.reset_streams += 1;
        let reason = classify_h2_reset(code);
        let stream_id = self.streams[idx].id;
        effects.push(emit_fact(ProtocolFact::Http2StreamReset {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            direction: ProtocolDirection::Inbound,
            reason,
        }));
        self.fail_stream(idx, Http2ClientOutcome::Reset(reason), effects);
        Ok(())
    }

    fn handle_ping(&mut self, frame: Frame) -> Result<(), Http2ProtocolError> {
        // RFC 9113 §6.7: PING is sent on stream 0x0 (a non-zero stream id
        // is a connection-level PROTOCOL_ERROR) and carries exactly 8
        // octets of opaque data (any other length is FRAME_SIZE_ERROR).
        // Distinguish the two so replay/observability sees the right
        // cause rather than collapsing both into BadFrameLength.
        if frame.stream_id != 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        if frame.payload.len() != 8 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        if frame.flags & FLAG_ACK == 0 {
            self.enqueue_frame(Frame::new(FRAME_PING, FLAG_ACK, 0, frame.payload));
        }
        Ok(())
    }

    fn complete_stream(&mut self, idx: usize, effects: &mut Vec<Effect<Self>>) {
        let mut stream = self.streams.swap_remove(idx);
        let stream_id = stream.id;
        self.report.closed_streams += 1;
        effects.push(emit_fact(ProtocolFact::Http2StreamClosed {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            reason: Http2CloseReason::EndStream,
        }));
        let outcome = match stream.response_status {
            Some(status) => Http2ClientOutcome::Replied(Http2ClientResponse {
                status,
                headers: stream.response_headers,
                body: stream.response_body,
                trailers: stream.response_trailers,
            }),
            None => Http2ClientOutcome::ProtocolError(Http2ProtocolError::InvalidPseudoHeaders),
        };
        if let Some(waiter) = stream.waiter.take() {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome { stream_id, outcome },
            ));
        }
    }

    fn fail_stream(
        &mut self,
        idx: usize,
        outcome: Http2ClientOutcome,
        effects: &mut Vec<Effect<Self>>,
    ) {
        let mut stream = self.streams.swap_remove(idx);
        let stream_id = stream.id;
        self.report.closed_streams += 1;
        let close_reason = match &outcome {
            Http2ClientOutcome::Reset(_) | Http2ClientOutcome::LocalCancel => {
                Http2CloseReason::LocalCloseOnly
            }
            Http2ClientOutcome::Closed => Http2CloseReason::GoAway,
            _ => Http2CloseReason::LocalCloseOnly,
        };
        effects.push(emit_fact(ProtocolFact::Http2StreamClosed {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            reason: close_reason,
        }));
        if let Some(waiter) = stream.waiter.take() {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome { stream_id, outcome },
            ));
        }
    }

    fn fail_all(&mut self, outcome: Http2ClientOutcome) -> Effect<Self> {
        let mut effects: Vec<Effect<Self>> = Vec::new();
        let queued = std::mem::take(&mut self.queued_submits);
        for (_, waiter) in queued {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome {
                    stream_id: 0,
                    outcome: outcome.clone(),
                },
            ));
        }
        let streams: Vec<_> = self.streams.drain(..).collect();
        for mut stream in streams {
            let stream_id = stream.id;
            if let Some(waiter) = stream.waiter.take() {
                effects.push(reply_to_request::<Self>(
                    waiter,
                    Http2ClientReply::Outcome {
                        stream_id,
                        outcome: outcome.clone(),
                    },
                ));
            }
        }
        effects.push(stop());
        batch(effects)
    }

    fn protocol_error(
        &mut self,
        err: Http2ProtocolError,
        mut effects: Vec<Effect<Self>>,
    ) -> Effect<Self> {
        self.report.protocol_errors += 1;
        let code = match &err {
            Http2ProtocolError::FrameTooLarge { .. } => ERR_FRAME_SIZE_ERROR,
            Http2ProtocolError::FlowControl | Http2ProtocolError::WindowOverflow => {
                ERR_FLOW_CONTROL_ERROR
            }
            Http2ProtocolError::SettingsUnsupported => ERR_SETTINGS_ERROR,
            _ => ERR_PROTOCOL_ERROR,
        };
        self.enqueue_frame(goaway_frame(self.next_stream_id, code));
        let streams: Vec<_> = self.streams.drain(..).collect();
        for mut stream in streams {
            let stream_id = stream.id;
            if let Some(waiter) = stream.waiter.take() {
                effects.push(reply_to_request::<Self>(
                    waiter,
                    Http2ClientReply::Outcome {
                        stream_id,
                        outcome: Http2ClientOutcome::ProtocolError(err.clone()),
                    },
                ));
            }
        }
        self.closing_after_write = true;
        self.maybe_write_more(&mut effects);
        batch(effects)
    }

    fn handle_wrote(&mut self, n: usize) -> Effect<Self> {
        // Wrote completion: drain the bytes we know flushed, clear the
        // in-flight flag, then schedule the next write if there is one.
        self.write_in_flight = false;
        if n > 0 && self.pending_write.len() >= n {
            self.pending_write.drain(..n);
        }
        if self.pending_write.is_empty() {
            if let Some(next) = self.write_queue.pop_front() {
                self.pending_write = next;
            }
        }
        if !self.pending_write.is_empty() {
            self.write_more()
        } else if self.closing_after_write {
            self.close_now()
        } else {
            noop()
        }
    }

    fn enqueue_frame(&mut self, frame: Frame) {
        let bytes = frame.encode();
        // Only fill `pending_write` if it is empty AND we are not
        // currently waiting on a write completion. Otherwise queue
        // behind it so `write_in_flight` correctly gates re-entry.
        if self.pending_write.is_empty() && !self.write_in_flight {
            self.pending_write = bytes;
        } else {
            self.write_queue.push_back(bytes);
        }
    }

    /// Idempotent "kick the writer if it is idle and there is work".
    /// Mirrors the server's `pending_write.is_empty() && !write_queue.is_empty()`
    /// guard so callers from many sites never double-arm `tcp_write` and
    /// trip the driver's per-stream `ResourceBusy` lane check.
    fn maybe_write_more(&mut self, effects: &mut Vec<Effect<Self>>) {
        if self.write_in_flight {
            return;
        }
        if self.pending_write.is_empty() && self.write_queue.is_empty() {
            if self.closing_after_write {
                effects.push(self.close_now());
            }
            return;
        }
        effects.push(self.write_more());
    }

    fn write_more(&mut self) -> Effect<Self> {
        if self.write_in_flight {
            // Another path already armed `tcp_write`; the eventual
            // `Wrote(...)` completion will re-enter this function.
            return noop();
        }
        if self.pending_write.is_empty() {
            if let Some(next) = self.write_queue.pop_front() {
                self.pending_write = next;
            }
        }
        let Some(stream) = self.stream else {
            return noop();
        };
        if self.pending_write.is_empty() {
            if self.closing_after_write {
                return self.close_now();
            }
            return noop();
        }
        self.write_in_flight = true;
        tcp_write(stream, self.pending_write.clone()).then(Http2ClientMsg::Wrote)
    }

    fn read_more(&mut self) -> Effect<Self> {
        let Some(stream) = self.stream else {
            return noop();
        };
        tcp_read(stream, READ_CHUNK).then(Http2ClientMsg::Read)
    }

    fn close_with(&mut self, outcome: Http2ClientOutcome) -> Effect<Self> {
        let mut effects: Vec<Effect<Self>> = Vec::new();
        let streams: Vec<_> = self.streams.drain(..).collect();
        for mut stream in streams {
            let stream_id = stream.id;
            if let Some(waiter) = stream.waiter.take() {
                effects.push(reply_to_request::<Self>(
                    waiter,
                    Http2ClientReply::Outcome {
                        stream_id,
                        outcome: outcome.clone(),
                    },
                ));
            }
        }
        let queued = std::mem::take(&mut self.queued_submits);
        for (_, waiter) in queued {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome {
                    stream_id: 0,
                    outcome: outcome.clone(),
                },
            ));
        }
        if self.stream.is_some() {
            effects.push(self.close_now());
        } else {
            effects.push(stop());
        }
        batch(effects)
    }

    fn close_now(&mut self) -> Effect<Self> {
        let Some(stream) = self.stream else {
            return stop();
        };
        tcp_close_stream(stream).then(Http2ClientMsg::Closed)
    }

    fn begin_goaway_shutdown(&mut self) -> Effect<Self> {
        if self.stream.is_none() {
            return self.fail_all(Http2ClientOutcome::Closed);
        }
        self.enqueue_frame(goaway_frame(self.next_stream_id, ERR_NO_ERROR));
        self.closing_after_write = true;
        let mut effects: Vec<Effect<Self>> = Vec::new();
        self.maybe_write_more(&mut effects);
        batch(effects)
    }
}

/// Helper: produce a per-stream rejection effect. Used at every site
/// that admits-then-fails before the stream gets a slot in `self.streams`.
fn reject_outcome<S: Shard + 'static>(
    waiter: tina::RequestContext<Http2ClientReply>,
    stream_id: u32,
    outcome: Http2ClientOutcome,
) -> Effect<Http2ClientConnection<S>> {
    reply_to_request::<Http2ClientConnection<S>>(
        waiter,
        Http2ClientReply::Outcome { stream_id, outcome },
    )
}

fn push_setting(out: &mut Vec<u8>, id: u16, value: u32) {
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&value.to_be_bytes());
}

fn encode_request_headers(target: &Http2Target, req: &Http2ClientRequest) -> Vec<u8> {
    let mut block = Vec::new();
    encode_literal_header(":method", req.method.as_str(), &mut block);
    let scheme = if target.is_tls() { "https" } else { "http" };
    encode_literal_header(":scheme", scheme, &mut block);
    encode_literal_header(":path", &req.path, &mut block);
    encode_literal_header(":authority", target.authority(), &mut block);
    for (name, value) in req.headers.iter() {
        if name.as_str().starts_with(':') {
            continue;
        }
        if matches!(
            name.as_str(),
            "connection" | "transfer-encoding" | "upgrade" | "host" | "keep-alive"
        ) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            encode_literal_header(name.as_str(), value, &mut block);
        }
    }
    block
}

fn apply_response_headers(stream: &mut ActiveClientStream, header_block: HeaderBlock) {
    stream.response_status = header_block.status;
    stream.response_content_length = header_block.content_length;
    for (name, value) in header_block.headers.iter() {
        stream.response_headers.append(name.clone(), value.clone());
    }
}

fn emit_fact<S: Shard + 'static>(fact: ProtocolFact) -> Effect<Http2ClientConnection<S>> {
    tina::fact::<Http2ClientConnection<S>>(fact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http2::AlpnProtocols;
    use std::net::Ipv4Addr;

    #[test]
    fn h2c_target_route_key_is_authority_qualified() {
        let target = Http2Target::H2c {
            authority: "x".into(),
            addr: (Ipv4Addr::LOCALHOST, 1234).into(),
        };
        assert_eq!(target.route_key(), "h2c::x");
        assert!(!target.is_tls());
    }

    #[test]
    fn tls_target_route_key_distinguishes_distinct_root_sets() {
        // Two TLS targets with the same authority / server_name / alpn
        // but different trust roots must NOT collide on the route key.
        // Without this property, Phase 119 pooling would share a
        // connection across security boundaries.
        let mk = |roots: Vec<Vec<u8>>| Http2Target::Tls {
            authority: "x".into(),
            addr: (Ipv4Addr::LOCALHOST, 443).into(),
            server_name: "x".into(),
            trust_roots: roots,
            alpn: AlpnProtocols::h2(),
        };
        let a = mk(vec![vec![0_u8; 4]]);
        let b = mk(vec![vec![1_u8; 4]]);
        // Same root-count, different bytes:
        assert!(a.is_tls());
        assert!(b.is_tls());
        assert_ne!(
            a.route_key(),
            b.route_key(),
            "route_key must hash the trust_root bytes, not just their count"
        );
        // Same inputs round-trip to the same key:
        let a2 = mk(vec![vec![0_u8; 4]]);
        assert_eq!(a.route_key(), a2.route_key());
        // h2c form is shorter and authority-tagged:
        let h2c = Http2Target::H2c {
            authority: "x".into(),
            addr: (Ipv4Addr::LOCALHOST, 80).into(),
        };
        assert_eq!(h2c.route_key(), "h2c::x");
    }

    #[test]
    fn request_helpers_set_method_and_path() {
        let req = Http2ClientRequest::get("/x");
        assert_eq!(req.method, Method::GET);
        assert!(req.body.is_empty());
        let req = Http2ClientRequest::post("/x", b"hi".to_vec());
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.body, b"hi");
    }

    #[test]
    fn outcome_surface_excludes_unimplemented_variants() {
        // Compile-shape proof that the outcome surface lists exactly the
        // variants the implementation can actually produce. `Timeout`
        // and `FlowControlBlocked` are deliberately absent until a real
        // stream-level deadline lands. If someone re-adds either without
        // wiring a construction site, this exhaustive match stops
        // compiling and forces the conversation.
        // Exhaustive match *within the crate* (where `#[non_exhaustive]`
        // does not relax exhaustiveness). Adding a variant breaks this
        // match's compilation and forces the author to decide whether the
        // implementation actually produces it.
        fn classify(o: &Http2ClientOutcome) -> &'static str {
            match o {
                Http2ClientOutcome::Replied(_) => "replied",
                Http2ClientOutcome::Full => "full",
                Http2ClientOutcome::Closed => "closed",
                Http2ClientOutcome::Reset(_) => "reset",
                Http2ClientOutcome::LocalCancel => "local-cancel",
                Http2ClientOutcome::ProtocolError(_) => "protocol-error",
                Http2ClientOutcome::TlsAlpnMismatch => "tls-alpn-mismatch",
            }
        }
        assert_eq!(classify(&Http2ClientOutcome::Full), "full");
        assert_eq!(classify(&Http2ClientOutcome::Closed), "closed");
        assert_eq!(
            classify(&Http2ClientOutcome::TlsAlpnMismatch),
            "tls-alpn-mismatch"
        );
    }
}
