//! Native HTTP/2 server: prior-knowledge cleartext h2c, unary buffered
//! request/response, bounded stream table, visible frame/header/window
//! errors, and no async runtime ownership.
//!
//! Frame, HPACK, and protocol-error helpers live in sibling modules
//! (`frame`, `headers`, `errors`) and are shared with the native client.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http::Version;
#[cfg(test)]
use http::{Method, StatusCode};
use tina::prelude::*;
use tina::reply_to;
use tina_runtime::{
    CallError, CallOutcome, Http2CloseReason, Http2FlowControlSide, Http2ResetReason,
    Http2StreamId, ListenerId, ProtocolConnectionId, ProtocolDirection, ProtocolFact, StreamId,
    TcpReadBufReply, TcpWriteOwnedReply, call, call_cancelable, cancel_call, tcp_accept, tcp_bind,
    tcp_close_listener, tcp_close_stream, tcp_read_buf, tcp_write_owned,
};

use crate::streaming::{
    Http2RequestStream, RequestChunkReply, ResponseChunkMsg, ResponseChunkReply,
};
use crate::{HttpRequest, HttpRequestBody, HttpResponse, HttpResponseBody};

use super::errors::{
    ERR_ENHANCE_YOUR_CALM, ERR_FLOW_CONTROL_ERROR, ERR_FRAME_SIZE_ERROR, ERR_NO_ERROR,
    ERR_PROTOCOL_ERROR, ERR_REFUSED_STREAM, ERR_SETTINGS_ERROR, ERR_STREAM_CLOSED,
    Http2ProtocolError, classify_h2_reset,
};
#[cfg(test)]
use super::frame::try_decode_frame;
use super::frame::{
    CLIENT_PREFACE, DEFAULT_WINDOW, FLAG_ACK, FLAG_END_HEADERS, FLAG_END_STREAM,
    FRAME_CONTINUATION, FRAME_DATA, FRAME_GOAWAY, FRAME_HEADER_LEN, FRAME_HEADERS, FRAME_PING,
    FRAME_PRIORITY, FRAME_PUSH_PROMISE, FRAME_RST_STREAM, FRAME_SETTINGS, FRAME_WINDOW_UPDATE,
    Frame, PRIORITY_PAYLOAD_LEN, READ_CHUNK, WINDOW_CREDIT_FLUSH_THRESHOLD, add_window, data_frame,
    data_payload_view, goaway_frame, headers_frame, headers_payload_view, push_frame_header,
    push_setting, rst_stream_frame, settings_frame, try_decode_frame_meta, window_update_frame,
};
use super::headers::{
    DEFAULT_HEADER_TABLE_SIZE, HeaderBlock, MAX_MAX_FRAME_SIZE, MIN_MAX_FRAME_SIZE,
    PathInternCache, SETTINGS_ENABLE_PUSH, SETTINGS_HEADER_TABLE_SIZE,
    SETTINGS_INITIAL_WINDOW_SIZE, SETTINGS_MAX_CONCURRENT_STREAMS, SETTINGS_MAX_FRAME_SIZE,
    SETTINGS_MAX_HEADER_LIST_SIZE, decode_headers_block_compact_with, decode_headers_block_with,
    encode_response_headers, encode_response_headers_with_len, encode_response_trailers,
    validate_request_headers,
};

#[cfg(test)]
use super::headers::{decode_headers_block, encode_literal_header};

/// Configurable limits for the HTTP/2 first form.
#[derive(Debug, Clone, Copy)]
pub struct Http2Limits {
    /// Maximum payload bytes in a single frame.
    pub max_frame_size: usize,
    /// Maximum decoded header-list bytes for one HEADERS block.
    pub max_header_bytes: usize,
    /// Maximum simultaneously open streams on one connection.
    pub max_concurrent_streams: usize,
    /// Maximum buffered request body bytes for one stream.
    pub max_body_bytes: usize,
    /// Maximum buffered response body bytes for one stream.
    pub max_response_body_bytes: usize,
    /// Bounded outbound write-buffer queue length per connection.
    ///
    /// A queued buffer may contain one control frame, one streaming DATA frame,
    /// or one coalesced buffered response (HEADERS + DATA frames + trailers).
    /// Buffered response bytes are still bounded by
    /// [`Http2Limits::max_response_body_bytes`].
    pub connection_outbound_queue_capacity: usize,
    /// Maximum bytes delivered to a service in one HTTP/2 request-body pull.
    pub request_stream_chunk_size: usize,
    /// Timeout for one response source pull.
    pub response_stream_call_timeout: Duration,
    /// Initial connection receive window.
    pub initial_connection_window: i32,
    /// Initial stream receive window.
    pub initial_stream_window: i32,
    /// Maximum peer reset churn before this connection sends GOAWAY
    /// with `ENHANCE_YOUR_CALM`.
    pub rapid_reset_max_resets: u32,
}

impl Default for Http2Limits {
    fn default() -> Self {
        Self {
            max_frame_size: 16 * 1024,
            max_header_bytes: 16 * 1024,
            max_concurrent_streams: 64,
            max_body_bytes: 1024 * 1024,
            max_response_body_bytes: 1024 * 1024,
            connection_outbound_queue_capacity: 64,
            request_stream_chunk_size: 16 * 1024,
            response_stream_call_timeout: Duration::from_secs(10),
            initial_connection_window: DEFAULT_WINDOW,
            initial_stream_window: DEFAULT_WINDOW,
            rapid_reset_max_resets: 128,
        }
    }
}

/// Server-side knobs for [`Http2Listener`].
#[derive(Debug, Clone, Copy)]
pub struct Http2ServerConfig {
    pub limits: Http2Limits,
    pub service_call_timeout: Duration,
    pub connection_mailbox_capacity: usize,
    pub listener_mailbox_capacity: usize,
}

impl Http2ServerConfig {
    pub fn dev() -> Self {
        Self {
            limits: Http2Limits::default(),
            service_call_timeout: Duration::from_secs(10),
            connection_mailbox_capacity: 16,
            listener_mailbox_capacity: 8,
        }
    }
}

impl Default for Http2ServerConfig {
    fn default() -> Self {
        Self::dev()
    }
}

/// Owned HTTP/2 request parts at the service boundary.
///
/// Most services receive a normal [`HttpRequest`]. Built-in protocol services
/// can opt into these parts to avoid materializing public request fields they do
/// not use, while still crossing the ordinary Tina service-call boundary.
#[derive(Debug)]
pub struct Http2RequestParts {
    pub method: http::Method,
    pub path: Arc<str>,
    pub headers: http::HeaderMap,
    pub body: HttpRequestBody,
    pub grpc_content_type: bool,
    pub grpc_encoding_unsupported: bool,
}

impl Http2RequestParts {
    pub fn into_http_request(self) -> HttpRequest {
        HttpRequest {
            method: self.method,
            // The generic request shape owns a `String` path; the interned
            // `Arc<str>` is the compact gRPC shape, so only the public branch
            // pays this copy (the compact branch keeps the `Arc`).
            path: self.path.to_string(),
            version: Version::HTTP_2,
            headers: self.headers,
            body: self.body,
        }
    }
}

/// Converts HTTP/2 request parts into the service isolate's message type.
///
/// The blanket `From<HttpRequest>` impl keeps existing services on the public
/// request shape. Special built-in services may return `true` from
/// [`Http2ServiceMessage::compact_http2_headers`] and use
/// [`Http2ServiceMessage::from_http2_parts`] to skip public header storage.
pub trait Http2ServiceMessage: Sized + Send + 'static {
    fn compact_http2_headers() -> bool {
        false
    }

    fn from_http_request(request: HttpRequest) -> Self;

    fn from_http2_parts(parts: Http2RequestParts) -> Self {
        Self::from_http_request(parts.into_http_request())
    }
}

impl<T> Http2ServiceMessage for T
where
    T: From<HttpRequest> + Send + 'static,
{
    fn from_http_request(request: HttpRequest) -> Self {
        Self::from(request)
    }
}

/// Split-service messages: `ServiceMessage<Event, Request>` never
/// implements `From<HttpRequest>`, and never legally could — see
/// [`crate::FromHttpRequest`] for the full orphan-rule argument (defined
/// there for the HTTP/1 rail; it applies identically here, since
/// `ServiceMessage` and `HttpRequest` are the same two foreign/local
/// types either way). So it cannot ride the blanket above and needs this
/// mirror impl to close the same gap on the HTTP/2 rail. It wraps the
/// inbound request as `ServiceMessage::Request` — split-service listeners
/// never see raw wire events, only caller-authorized requests — and
/// otherwise inherits the trait's default `compact_http2_headers`
/// (`false`) and `from_http2_parts` (routes through `from_http_request`)
/// behavior, so no HTTP/2-specific header handling is lost or stubbed.
///
/// Does not overlap the blanket: nothing, in any crate, can implement
/// `From<HttpRequest> for ServiceMessage<Event, Request>` (the orphan rule
/// blocks it everywhere, since neither type is local to a crate that could
/// also name the other), so the compiler can prove these two impls never
/// apply to the same concrete type.
impl<Event, Request> Http2ServiceMessage for tina::ServiceMessage<Event, Request>
where
    Event: Send + 'static,
    Request: From<HttpRequest> + Send + 'static,
{
    fn from_http_request(request: HttpRequest) -> Self {
        tina::ServiceMessage::Request(Request::from(request))
    }
}

/// Stream lifecycle tracked by the bounded connection table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Http2StreamState {
    Idle,
    Open,
    HalfClosedRemote,
    HalfClosedLocal,
    Closed,
}

/// Typed first-form HTTP/2 outcomes.
///
/// Names the six lifecycle categories a Tina-owned HTTP/2 stream can end in:
/// happy reply, bounded admission failure, closed connection, flow-control
/// pressure, timeout, protocol error, and stream reset (peer-initiated or
/// locally cancelled). The enum is the documented typed vocabulary for
/// HTTP/2 lifecycle facts; today's stream observations surface through
/// [`Http2ConnectionReport`] counters, [`Http2ProtocolError`] (which is also
/// carried inside the `ProtocolError` arm here for GOAWAY/RST_STREAM cause
/// classification), and the [`crate::grpc::GrpcStatus`] trailers on gRPC
/// routes. Broader stream-level reporting that returns one
/// `Http2Outcome` per stream is future work; the variant set is fixed in
/// advance so that wiring does not change the public vocabulary.
///
/// `#[non_exhaustive]` so new typed categories can be added without
/// breaking semver on existing match arms.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Http2Outcome {
    /// Service replied through the ordinary HTTP response path.
    Replied,
    /// Bounded admission failure: connection or service mailbox was full.
    Full,
    /// Connection closed before the stream completed.
    Closed,
    /// Stream parked on stream or connection flow-control credit.
    FlowControlBlocked,
    /// Stream deadline elapsed before the service replied.
    Timeout,
    /// A typed [`Http2ProtocolError`] terminated the stream.
    ProtocolError(Http2ProtocolError),
    /// Peer-initiated RST_STREAM. The payload is the wire error code.
    StreamReset(u32),
    /// Locally-initiated stream cancellation (service or runtime asked the
    /// connection to send RST_STREAM). The payload is the wire error code
    /// the server sent. Reserved for the future stream-level outcome
    /// surface; today the connection emits these resets but does not
    /// observe them through this variant.
    LocalCancel(u32),
}

/// Per-connection report counters. `#[non_exhaustive]`: it is an output users
/// read, so new counters can be added without breaking callers.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Http2ConnectionReport {
    pub opened_streams: u64,
    pub closed_streams: u64,
    pub reset_streams: u64,
    pub connection_full: u64,
    pub stream_full: u64,
    pub flow_control_blocked: u64,
    pub protocol_errors: u64,
    pub goaway_sent: u64,
    pub late_replies_after_close: u64,
    pub rapid_reset_goaway: u64,
    /// Cached request paths evicted because the per-connection path cache was
    /// full. Non-zero means more distinct paths than the cap churned through;
    /// the calls still ran, the cache just evicted least-recently-used entries.
    pub path_cache_evictions: u64,
}

/// Per-stream report snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Http2StreamReport {
    pub stream_id: u32,
    pub state: Http2StreamState,
    pub buffered_body_bytes: usize,
    pub recv_window: i32,
}

#[derive(Debug)]
struct ActiveStream {
    id: u32,
    state: Http2StreamState,
    headers: Option<HeaderBlock>,
    body: Vec<u8>,
    grpc: bool,
    pending_call: Option<tina::CallHandle<HttpResponse>>,
    recv_window: i32,
    send_window: i32,
    pending_response: Option<PendingResponse>,
    response_source: Option<tina::Address<ResponseChunkMsg, ResponseChunkReply>>,
    response_trailers: Option<Vec<u8>>,
    response_pending_data: Vec<u8>,
    response_bytes_sent: usize,
    response_pull_in_flight: bool,
    response_pull_handle: Option<tina::CallHandle<ResponseChunkReply>>,
    /// `Some` only when this response was begun with a declared
    /// `content-length`. Counts down as DATA is accepted for outbound; if
    /// the source overshoots or EOFs early we reset the stream visibly.
    response_remaining_content_length: Option<usize>,
    request_dispatched_streaming: bool,
    request_eof: bool,
    request_content_length: Option<usize>,
    request_bytes_received: usize,
    pending_recv_window_credit: u32,
    request_chunks: VecDeque<RequestDataChunk>,
    pending_request_body_reply: Option<tina::RequestContext<Http2ConnectionReply>>,
}

#[derive(Debug)]
struct RequestDataChunk {
    data: Vec<u8>,
    flow_credit: usize,
}

#[derive(Debug)]
enum ResponseBytes {
    Owned(Vec<u8>),
    Shared(Arc<[u8]>),
}

impl ResponseBytes {
    fn len(&self) -> usize {
        match self {
            Self::Owned(bytes) => bytes.len(),
            Self::Shared(bytes) => bytes.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Shared(bytes) => bytes,
        }
    }
}

#[derive(Debug)]
struct PendingResponse {
    header_block: Vec<u8>,
    body: ResponseBytes,
    body_offset: usize,
    trailers: Option<Vec<u8>>,
}

impl PendingResponse {
    fn remaining_body(&self) -> &[u8] {
        &self.body.as_slice()[self.body_offset..]
    }
}

impl ActiveStream {
    fn new(id: u32, headers: HeaderBlock, recv_window: i32, send_window: i32, grpc: bool) -> Self {
        Self {
            id,
            state: Http2StreamState::Open,
            headers: Some(headers),
            body: Vec::new(),
            grpc,
            pending_call: None,
            recv_window,
            send_window,
            pending_response: None,
            response_source: None,
            response_trailers: None,
            response_pending_data: Vec::new(),
            response_bytes_sent: 0,
            response_pull_in_flight: false,
            response_pull_handle: None,
            response_remaining_content_length: None,
            request_dispatched_streaming: false,
            request_eof: false,
            request_content_length: None,
            request_bytes_received: 0,
            pending_recv_window_credit: 0,
            request_chunks: VecDeque::new(),
            pending_request_body_reply: None,
        }
    }
}

/// Messages handled by [`Http2Connection`].
#[derive(Debug, Clone)]
pub enum Http2ConnectionMsg {
    Begin,
    Read(Result<TcpReadBufReply, CallError>),
    ServiceReturned {
        stream_id: u32,
        outcome: CallOutcome<HttpResponse>,
    },
    ServiceCancelled {
        stream_id: u32,
        outcome: tina::CancelOutcome,
    },
    StreamChunk {
        stream_id: u32,
        outcome: CallOutcome<ResponseChunkReply>,
    },
    StreamSourceCancelDone {
        stream_id: u32,
        outcome: CallOutcome<ResponseChunkReply>,
    },
    StreamSourcePullCancelled {
        stream_id: u32,
        outcome: tina::CancelOutcome,
    },
    Wrote(Result<TcpWriteOwnedReply, CallError>),
    Closed(Result<(), CallError>),
    RequestBodyNext {
        stream_id: u32,
    },
    Stop,
    Report,
}

impl Http2ConnectionMsg {
    pub fn body_next(stream_id: u32) -> Self {
        Self::RequestBodyNext { stream_id }
    }
}

/// Per-connection request-path interner capacity. A connection serves a small,
/// fixed set of routes, so this is generous; a peer that floods distinct paths
/// fills it and the overflow is counted, never grown without bound.
const PATH_INTERN_CACHE_CAP: usize = 256;

#[derive(Debug, Clone)]
pub enum Http2ConnectionReply {
    RequestChunk(RequestChunkReply),
    Report(Http2ConnectionReport),
}

/// One HTTP/2 connection isolate over one TCP stream.
pub struct Http2Connection<S: Shard, M: Http2ServiceMessage = HttpRequest> {
    stream: StreamId,
    service: Address<M, HttpResponse>,
    limits: Http2Limits,
    service_call_timeout: Duration,
    read_buf: Vec<u8>,
    read_scratch: Vec<u8>,
    hpack_decoder: hpack::Decoder<'static>,
    path_cache: PathInternCache,
    preface_seen: bool,
    streams: Vec<ActiveStream>,
    /// stream-id → slot index in `streams`, kept in step with every push and
    /// `swap_remove`. Turns the per-frame `find_stream` lookup (called several
    /// times per frame) from O(open streams) into O(1).
    stream_index: HashMap<u32, usize>,
    highest_client_stream_id: u32,
    recv_window: i32,
    pending_recv_window_credit: u32,
    send_window: i32,
    peer_initial_stream_window: i32,
    peer_max_frame_size: usize,
    reset_churn: u32,
    goaway: bool,
    closing_after_write: bool,
    write_in_flight: bool,
    pending_write: Vec<u8>,
    write_queue: VecDeque<Vec<u8>>,
    report: Http2ConnectionReport,
    self_shard_id: Option<tina::ShardId>,
    self_isolate_id: Option<tina::IsolateId>,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static, M: Http2ServiceMessage> Http2Connection<S, M> {
    pub fn new(
        stream: StreamId,
        service: Address<M, HttpResponse>,
        limits: Http2Limits,
        service_call_timeout: Duration,
    ) -> Self {
        Self {
            stream,
            service,
            limits,
            service_call_timeout,
            read_buf: Vec::new(),
            read_scratch: Vec::new(),
            hpack_decoder: hpack::Decoder::new(),
            path_cache: PathInternCache::with_capacity(PATH_INTERN_CACHE_CAP),
            preface_seen: false,
            streams: Vec::with_capacity(limits.max_concurrent_streams),
            stream_index: HashMap::with_capacity(limits.max_concurrent_streams),
            highest_client_stream_id: 0,
            recv_window: limits.initial_connection_window,
            pending_recv_window_credit: 0,
            send_window: DEFAULT_WINDOW,
            peer_initial_stream_window: DEFAULT_WINDOW,
            peer_max_frame_size: limits.max_frame_size,
            reset_churn: 0,
            goaway: false,
            closing_after_write: false,
            write_in_flight: false,
            pending_write: Vec::new(),
            write_queue: VecDeque::new(),
            report: Http2ConnectionReport::default(),
            self_shard_id: None,
            self_isolate_id: None,
            _shard: PhantomData,
        }
    }

    pub fn report(&self) -> &Http2ConnectionReport {
        &self.report
    }

    /// Returns the local connection id used to correlate protocol facts emitted
    /// by this isolate. The id is the isolate's own `IsolateId` so every
    /// fact for this connection shares a stable token within the trace.
    fn connection_fact_id(&self) -> ProtocolConnectionId {
        ProtocolConnectionId::new(self.self_isolate_id.map(|id| id.get()).unwrap_or_default())
    }

    fn emit_protocol_fact(&self, effects: &mut Vec<Effect<Self>>, fact: ProtocolFact) {
        effects.push(tina::fact::<Self>(fact));
    }
}

impl<S: Shard + 'static, M: Http2ServiceMessage> Isolate for Http2Connection<S, M> {
    tina::isolate_types! {
        message: Http2ConnectionMsg,
        reply: Http2ConnectionReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: tina_runtime::RuntimeCall<Http2ConnectionMsg>,
        fact: ProtocolFact,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: Http2ConnectionMsg,
        ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        if self.self_isolate_id.is_none() {
            self.self_shard_id = Some(ctx.shard_id());
            self.self_isolate_id = Some(ctx.isolate_id());
        }
        match msg {
            Http2ConnectionMsg::Begin => self.read_more(),
            Http2ConnectionMsg::Read(Ok(reply)) => self.handle_read(reply),
            Http2ConnectionMsg::Read(Err(_)) => self.close_now(),
            Http2ConnectionMsg::ServiceReturned { stream_id, outcome } => {
                self.handle_service_returned(stream_id, outcome)
            }
            Http2ConnectionMsg::ServiceCancelled { .. } => noop(),
            Http2ConnectionMsg::StreamChunk { stream_id, outcome } => {
                self.handle_stream_chunk(stream_id, outcome)
            }
            Http2ConnectionMsg::StreamSourceCancelDone { .. } => noop(),
            Http2ConnectionMsg::StreamSourcePullCancelled { .. } => noop(),
            Http2ConnectionMsg::Wrote(Ok(reply)) => self.handle_wrote(reply),
            Http2ConnectionMsg::Wrote(Err(_)) => self.close_now(),
            Http2ConnectionMsg::Closed(_) => stop(),
            Http2ConnectionMsg::RequestBodyNext { .. } => noop(),
            Http2ConnectionMsg::Stop => self.begin_goaway_shutdown(),
            Http2ConnectionMsg::Report => noop(),
        }
    }

    fn handle_call(
        &mut self,
        msg: Http2ConnectionMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            Http2ConnectionMsg::RequestBodyNext { stream_id } => {
                self.handle_request_body_next(stream_id, call)
            }
            Http2ConnectionMsg::Report => {
                // Fold the live path-cache eviction count into the snapshot.
                self.report.path_cache_evictions = self.path_cache.evictions();
                call.reply(Http2ConnectionReply::Report(self.report.clone()))
            }
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl<S: Shard + 'static, M: Http2ServiceMessage> Http2Connection<S, M> {
    fn read_more(&mut self) -> Effect<Self> {
        let buffer = std::mem::take(&mut self.read_scratch);
        tcp_read_buf(self.stream, buffer, READ_CHUNK)
            .then(|result| Http2ConnectionMsg::Read(result.map_err(|error| error.error)))
    }

    fn write_more(&mut self) -> Effect<Self> {
        if self.write_in_flight {
            return noop();
        }
        if self.pending_write.is_empty() {
            // Write the first queued buffer, then batch following frames into the
            // same write while under one peer frame's worth. Keeps a response and
            // its window-update in one write without merging many large buffers.
            if let Some(first) = self.write_queue.pop_front() {
                self.pending_write = first;
                while let Some(next) = self.write_queue.front() {
                    if self.pending_write.len() + next.len() > self.peer_max_frame_size {
                        break;
                    }
                    let next = self.write_queue.pop_front().expect("front just peeked");
                    self.pending_write.extend_from_slice(&next);
                }
            }
        }
        if self.pending_write.is_empty() {
            if self.closing_after_write {
                return self.close_now();
            }
            return noop();
        }
        let bytes = std::mem::take(&mut self.pending_write);
        self.write_in_flight = true;
        tcp_write_owned(self.stream, bytes)
            .then(|result| Http2ConnectionMsg::Wrote(result.map_err(|error| error.error)))
    }

    fn close_now(&mut self) -> Effect<Self> {
        let mut effects = Vec::new();
        self.drain_streams_for_connection_close(&mut effects);
        effects.push(tcp_close_stream(self.stream).then(Http2ConnectionMsg::Closed));
        batch(effects)
    }

    fn begin_goaway_shutdown(&mut self) -> Effect<Self> {
        self.goaway = true;
        let mut effects = Vec::new();
        self.drain_streams_for_connection_close(&mut effects);
        let _ = self.enqueue_frame(goaway_frame(self.highest_client_stream_id, ERR_NO_ERROR));
        self.report.goaway_sent += 1;
        self.closing_after_write = true;
        let next = if self.write_in_flight {
            noop()
        } else if self.pending_write.is_empty() && !self.write_queue.is_empty() {
            self.write_more()
        } else if self.pending_write.is_empty() {
            self.close_now()
        } else {
            noop()
        };
        effects.push(next);
        batch(effects)
    }

    fn handle_read(&mut self, reply: TcpReadBufReply) -> Effect<Self> {
        let TcpReadBufReply { buffer, len } = reply;
        if len == 0 {
            self.read_scratch = buffer;
            return self.close_now();
        }
        self.read_buf.extend_from_slice(&buffer[..len]);
        self.read_scratch = buffer;
        let mut effects = Vec::new();
        if let Err(error) = self.process_buffer(&mut effects) {
            self.report.protocol_errors += 1;
            let code = match error {
                Http2ProtocolError::FrameTooLarge { .. } => ERR_FRAME_SIZE_ERROR,
                Http2ProtocolError::FlowControl | Http2ProtocolError::WindowOverflow => {
                    ERR_FLOW_CONTROL_ERROR
                }
                Http2ProtocolError::SettingsUnsupported => ERR_SETTINGS_ERROR,
                _ => ERR_PROTOCOL_ERROR,
            };
            let _ = self.enqueue_frame(goaway_frame(self.highest_client_stream_id, code));
            self.report.goaway_sent += 1;
            self.closing_after_write = true;
            self.drain_streams_for_connection_close(&mut effects);
        }
        if self.pending_write.is_empty() && !self.write_queue.is_empty() {
            effects.push(self.write_more());
        }
        if !self.closing_after_write {
            effects.push(self.read_more());
        } else if self.pending_write.is_empty()
            && self.write_queue.is_empty()
            && !self.write_in_flight
        {
            effects.push(self.close_now());
        }
        batch(effects)
    }

    fn process_buffer(
        &mut self,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if !self.preface_seen {
            if self.read_buf.len() < CLIENT_PREFACE.len() {
                return Ok(());
            }
            if &self.read_buf[..CLIENT_PREFACE.len()] != CLIENT_PREFACE {
                return Err(Http2ProtocolError::BadPreface);
            }
            self.read_buf.drain(..CLIENT_PREFACE.len());
            self.preface_seen = true;
            self.enqueue_frame(self.initial_settings_frame())?;
        }

        // Process frames as borrowed slices of the read buffer. Take the
        // buffer out first so a payload slice (`&buf`) can coexist with the
        // `&mut self` the handlers need — `buf` is a local, so it does not
        // alias `self`. Restore the drained remainder afterwards, reusing the
        // allocation (a partial trailing frame stays buffered for the next
        // read). DATA and HEADERS — the hot frames — are handled straight from
        // `buf` with no per-frame payload `Vec`; the rare, tiny control frames
        // take a cheap owned copy.
        // `consumed` advances only after a frame is handled, so on a protocol
        // error the failing frame is left in the restored buffer (rather than
        // pre-drained as the old per-frame loop did). That is safe: the caller
        // turns a `process_buffer` error into GOAWAY + `closing_after_write` and
        // never arms another read, so the buffer is never reprocessed.
        let mut buf = std::mem::take(&mut self.read_buf);
        let mut consumed = 0usize;
        let result = self.process_frames(&buf, &mut consumed, effects);
        buf.drain(..consumed);
        self.read_buf = buf;
        result
    }

    fn process_frames(
        &mut self,
        buf: &[u8],
        consumed: &mut usize,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        while let Some(meta) = try_decode_frame_meta(&buf[*consumed..], self.limits.max_frame_size)?
        {
            let start = *consumed + FRAME_HEADER_LEN;
            let end = *consumed + meta.total;
            let payload = &buf[start..end];
            match meta.ty {
                FRAME_DATA => self.handle_data(meta.flags, meta.stream_id, payload, effects)?,
                FRAME_HEADERS => {
                    self.handle_headers(meta.flags, meta.stream_id, payload, effects)?
                }
                // WINDOW_UPDATE is the frequent control frame on streaming /
                // large-body connections; handle it from the borrowed slice too
                // rather than paying an owned `Frame` copy per credit.
                FRAME_WINDOW_UPDATE => {
                    self.handle_window_update(meta.stream_id, payload, effects)?;
                    self.push_ready_response_pulls(effects);
                }
                _ => self.handle_control_frame(
                    meta.ty,
                    meta.flags,
                    meta.stream_id,
                    payload,
                    effects,
                )?,
            }
            *consumed = end;
        }
        Ok(())
    }

    /// Handle a non-DATA/HEADERS frame. These are small and infrequent
    /// (SETTINGS, WINDOW_UPDATE, PING, RST_STREAM, etc.), so a cheap owned
    /// `Frame` copy keeps the existing handlers unchanged while the hot DATA
    /// and HEADERS paths stay allocation-free.
    fn handle_control_frame(
        &mut self,
        ty: u8,
        flags: u8,
        stream_id: u32,
        payload: &[u8],
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        let frame = Frame::new(ty, flags, stream_id, payload.to_vec());
        match ty {
            FRAME_SETTINGS => self.handle_settings(frame, effects),
            FRAME_RST_STREAM => self.handle_rst_stream(frame, effects),
            FRAME_PING => self.handle_ping(frame),
            FRAME_GOAWAY => {
                self.goaway = true;
                Ok(())
            }
            FRAME_PRIORITY => self.handle_priority(frame, effects),
            FRAME_PUSH_PROMISE => Err(Http2ProtocolError::UnsupportedFrame(FRAME_PUSH_PROMISE)),
            FRAME_CONTINUATION => Err(Http2ProtocolError::UnexpectedContinuation),
            _ => Ok(()),
        }
    }

    fn handle_priority(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if frame.stream_id == 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        if frame.payload.len() != PRIORITY_PAYLOAD_LEN {
            if self.find_stream(frame.stream_id).is_some() {
                self.reset_active_stream_for_protocol(
                    frame.stream_id,
                    ERR_FRAME_SIZE_ERROR,
                    Http2ResetReason::FrameSizeError,
                    effects,
                );
            }
            return Ok(());
        }
        Ok(())
    }

    fn handle_settings(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
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
        self.enqueue_frame(settings_frame(true))?;
        self.flush_pending_responses(effects)?;
        self.push_ready_response_pulls(effects);
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
            SETTINGS_MAX_CONCURRENT_STREAMS => {}
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

    fn handle_ping(&mut self, frame: Frame) -> Result<(), Http2ProtocolError> {
        if frame.stream_id != 0 || frame.payload.len() != 8 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        if frame.flags & FLAG_ACK == 0 {
            self.enqueue_frame(Frame::new(FRAME_PING, FLAG_ACK, 0, frame.payload))?;
        }
        Ok(())
    }

    /// Test-only: dispatch an owned HEADERS `Frame` through the borrowed-slice
    /// handler, so unit tests can keep building `Frame`s directly.
    #[cfg(test)]
    fn handle_headers_frame(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        self.handle_headers(frame.flags, frame.stream_id, &frame.payload, effects)
    }

    /// Test-only: dispatch an owned DATA `Frame` through the borrowed-slice
    /// handler.
    #[cfg(test)]
    fn handle_data_frame(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        self.handle_data(frame.flags, frame.stream_id, &frame.payload, effects)
    }

    fn handle_headers(
        &mut self,
        flags: u8,
        stream_id: u32,
        payload: &[u8],
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if stream_id == 0 || stream_id % 2 == 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        if flags & FLAG_END_HEADERS == 0 {
            return Err(Http2ProtocolError::HpackUnsupported);
        }
        if self.goaway {
            self.enqueue_frame(rst_stream_frame(stream_id, ERR_REFUSED_STREAM))?;
            return Ok(());
        }
        if self.find_stream(stream_id).is_some() {
            self.reset_active_stream_for_protocol(
                stream_id,
                ERR_PROTOCOL_ERROR,
                Http2ResetReason::ProtocolError,
                effects,
            );
            return Ok(());
        }
        if stream_id <= self.highest_client_stream_id {
            return Err(Http2ProtocolError::BadStreamId);
        }
        self.highest_client_stream_id = stream_id;
        if self.streams.len() >= self.limits.max_concurrent_streams {
            self.report.stream_full += 1;
            self.enqueue_frame(rst_stream_frame(stream_id, ERR_ENHANCE_YOUR_CALM))?;
            self.emit_protocol_fact(
                effects,
                ProtocolFact::Http2StreamReset {
                    connection: self.connection_fact_id(),
                    stream: Http2StreamId::new(stream_id),
                    direction: ProtocolDirection::Outbound,
                    reason: Http2ResetReason::EnhanceYourCalm,
                },
            );
            return Ok(());
        }
        // Decode the HPACK block straight from the read buffer slice — no
        // per-frame payload `Vec`.
        let header_payload = headers_payload_view(flags, payload)?;
        let headers = if M::compact_http2_headers() {
            decode_headers_block_compact_with(
                &mut self.hpack_decoder,
                header_payload,
                self.limits.max_header_bytes,
                Some(&self.path_cache),
            )?
        } else {
            decode_headers_block_with(
                &mut self.hpack_decoder,
                header_payload,
                self.limits.max_header_bytes,
                Some(&self.path_cache),
            )?
        };
        validate_request_headers(&headers)?;
        // Cache the path only now that the request has validated, so a peer
        // cannot fill the cache with unvalidated paths.
        if let Some(path) = &headers.path {
            self.path_cache.remember(path);
        }
        let grpc = headers.grpc_content_type;
        let end_stream = flags & FLAG_END_STREAM != 0;
        let declared_len = headers.content_length;
        // END_STREAM on HEADERS means zero DATA bytes will follow.
        // A declared non-zero content-length is a lie before any handler
        // sees the request.
        if end_stream && declared_len.is_some_and(|n| n != 0) {
            self.report.protocol_errors += 1;
            self.enqueue_frame(rst_stream_frame(stream_id, ERR_PROTOCOL_ERROR))?;
            return Ok(());
        }
        let mut stream = ActiveStream::new(
            stream_id,
            headers,
            self.limits.initial_stream_window,
            self.peer_initial_stream_window,
            grpc,
        );
        stream.request_content_length = declared_len;
        self.report.opened_streams += 1;
        self.emit_protocol_fact(
            effects,
            ProtocolFact::Http2StreamOpened {
                connection: self.connection_fact_id(),
                stream: Http2StreamId::new(stream_id),
                direction: ProtocolDirection::Inbound,
            },
        );
        if end_stream {
            stream.request_eof = true;
            stream.state = Http2StreamState::HalfClosedRemote;
            self.push_stream(stream);
            self.dispatch_stream(stream_id, effects)?;
        } else if grpc {
            self.push_stream(stream);
            self.dispatch_streaming_request(stream_id, effects)?;
        } else {
            self.push_stream(stream);
        }
        Ok(())
    }

    fn handle_data(
        &mut self,
        flags: u8,
        stream_id: u32,
        payload: &[u8],
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if stream_id == 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        // Read the unpadded payload as a borrowed sub-slice of the connection
        // read buffer (no per-frame `Vec`), plus the flow-control wire length.
        let end_stream = flags & FLAG_END_STREAM != 0;
        let (data, flow_len) = data_payload_view(flags, payload)?;
        let data_len = data.len();
        let flow_len_i32 = i32::try_from(flow_len).map_err(|_| Http2ProtocolError::FlowControl)?;
        if self.recv_window < flow_len_i32 {
            self.report.flow_control_blocked += 1;
            self.emit_protocol_fact(
                effects,
                ProtocolFact::Http2FlowControlFull {
                    connection: self.connection_fact_id(),
                    stream: Http2StreamId::new(0),
                    side: Http2FlowControlSide::ConnectionReceive,
                },
            );
            return Err(Http2ProtocolError::FlowControl);
        }
        let idx = match self.find_stream(stream_id) {
            Some(idx) => idx,
            None => {
                if stream_id > self.highest_client_stream_id {
                    return Err(Http2ProtocolError::StreamClosed);
                }
                self.add_connection_window_credit(flow_len);
                self.reset_missing_stream(
                    stream_id,
                    ERR_STREAM_CLOSED,
                    Http2ResetReason::StreamClosed,
                    effects,
                );
                self.maybe_flush_request_window_credit(stream_id, true)?;
                return Ok(());
            }
        };
        if self.streams[idx].state == Http2StreamState::Closed {
            self.add_connection_window_credit(flow_len);
            self.reset_stream_with_cleanup(
                stream_id,
                ERR_STREAM_CLOSED,
                Http2ResetReason::StreamClosed,
                effects,
                CallError::TargetClosed,
            );
            self.maybe_flush_request_window_credit(stream_id, true)?;
            return Ok(());
        }
        if self.streams[idx].request_eof {
            self.add_connection_window_credit(flow_len);
            self.reset_stream_with_cleanup(
                stream_id,
                ERR_STREAM_CLOSED,
                Http2ResetReason::StreamClosed,
                effects,
                CallError::TargetClosed,
            );
            self.maybe_flush_request_window_credit(stream_id, true)?;
            return Ok(());
        }
        if self.streams[idx].recv_window < flow_len_i32 {
            self.report.flow_control_blocked += 1;
            self.emit_protocol_fact(
                effects,
                ProtocolFact::Http2FlowControlFull {
                    connection: self.connection_fact_id(),
                    stream: Http2StreamId::new(stream_id),
                    side: Http2FlowControlSide::StreamReceive,
                },
            );
            self.add_connection_window_credit(flow_len);
            self.reset_active_stream_for_protocol(
                stream_id,
                ERR_FLOW_CONTROL_ERROR,
                Http2ResetReason::FlowControlError,
                effects,
            );
            self.maybe_flush_request_window_credit(stream_id, true)?;
            return Ok(());
        }
        if let Some(content_length) = self.streams[idx].request_content_length {
            let received = self.streams[idx]
                .request_bytes_received
                .checked_add(data_len)
                .ok_or(Http2ProtocolError::HeadersTooLarge)?;
            if received > content_length {
                self.report.protocol_errors += 1;
                self.add_connection_window_credit(flow_len);
                self.reset_active_stream_for_protocol(
                    stream_id,
                    ERR_PROTOCOL_ERROR,
                    Http2ResetReason::ProtocolError,
                    effects,
                );
                self.maybe_flush_request_window_credit(stream_id, true)?;
                return Ok(());
            }
        }
        let buffered_len = if self.streams[idx].request_dispatched_streaming {
            self.streams[idx].request_bytes_received
        } else {
            self.streams[idx].body.len()
        };
        let new_len = buffered_len
            .checked_add(data_len)
            .ok_or(Http2ProtocolError::HeadersTooLarge)?;
        if new_len > self.limits.max_body_bytes {
            self.report.stream_full += 1;
            self.emit_protocol_fact(
                effects,
                ProtocolFact::HttpBodyHighWater {
                    connection: self.connection_fact_id(),
                    body_id: stream_id as u64,
                    direction: ProtocolDirection::Inbound,
                    buffered_bytes: new_len as u64,
                    threshold_bytes: self.limits.max_body_bytes as u64,
                },
            );
            self.add_connection_window_credit(flow_len);
            self.reset_active_stream_for_protocol(
                stream_id,
                ERR_ENHANCE_YOUR_CALM,
                Http2ResetReason::EnhanceYourCalm,
                effects,
            );
            self.maybe_flush_request_window_credit(stream_id, true)?;
            return Ok(());
        }
        self.recv_window -= flow_len_i32;
        self.streams[idx].recv_window -= flow_len_i32;
        self.streams[idx].request_bytes_received += data_len;
        if self.streams[idx].request_dispatched_streaming {
            if !data.is_empty() {
                // A queued streaming chunk must outlive this read buffer, so it
                // owns its bytes here. The buffered path below avoids the copy.
                self.streams[idx]
                    .request_chunks
                    .push_back(RequestDataChunk {
                        data: data.to_vec(),
                        flow_credit: flow_len,
                    });
            } else if flow_len > 0 {
                self.add_request_window_credit(idx, flow_len);
                self.maybe_flush_request_window_credit(stream_id, false)?;
            }
        } else {
            self.streams[idx].body.extend_from_slice(data);
            // The buffered body is retained immediately, so credit its flow
            // bytes back mid-upload — mirroring the streaming path. Without
            // this a buffered upload larger than the initial window exhausts
            // the peer's send credit and deadlocks before END_STREAM.
            if flow_len > 0 {
                self.add_request_window_credit(idx, flow_len);
                self.maybe_flush_request_window_credit(stream_id, false)?;
            }
        }
        if end_stream {
            if self.streams[idx]
                .request_content_length
                .is_some_and(|content_length| {
                    self.streams[idx].request_bytes_received != content_length
                })
            {
                self.report.protocol_errors += 1;
                self.reset_active_stream_for_protocol(
                    stream_id,
                    ERR_PROTOCOL_ERROR,
                    Http2ResetReason::ProtocolError,
                    effects,
                );
                return Ok(());
            }
            self.streams[idx].request_eof = true;
            self.streams[idx].state = Http2StreamState::HalfClosedRemote;
            if self.streams[idx].request_dispatched_streaming {
                effects.push(self.reply_pending_request_chunk(stream_id)?);
            } else {
                self.dispatch_stream(stream_id, effects)?;
            }
        } else if self.streams[idx].request_dispatched_streaming {
            effects.push(self.reply_pending_request_chunk(stream_id)?);
        }
        Ok(())
    }

    /// Test-only: dispatch an owned WINDOW_UPDATE `Frame` through the
    /// borrowed-slice handler.
    #[cfg(test)]
    fn handle_window_update_frame(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        self.handle_window_update(frame.stream_id, &frame.payload, effects)
    }

    fn handle_window_update(
        &mut self,
        stream_id: u32,
        payload: &[u8],
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if payload.len() != 4 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        let mut bytes = [0_u8; 4];
        bytes.copy_from_slice(payload);
        let increment = u32::from_be_bytes(bytes) & 0x7fff_ffff;
        if increment == 0 {
            if stream_id == 0 {
                return Err(Http2ProtocolError::WindowOverflow);
            }
            self.report.protocol_errors += 1;
            self.reset_active_stream_for_protocol(
                stream_id,
                ERR_PROTOCOL_ERROR,
                Http2ResetReason::ProtocolError,
                effects,
            );
            return Ok(());
        }
        if stream_id == 0 {
            self.send_window = add_window(self.send_window, increment)?;
        } else if let Some(idx) = self.find_stream(stream_id) {
            self.streams[idx].send_window = add_window(self.streams[idx].send_window, increment)?;
        }
        self.flush_pending_responses(effects)?;
        Ok(())
    }

    fn handle_rst_stream(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if frame.stream_id == 0 || frame.payload.len() != 4 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        let mut reset_code_bytes = [0_u8; 4];
        reset_code_bytes.copy_from_slice(&frame.payload);
        let reset_code = u32::from_be_bytes(reset_code_bytes);
        self.reset_churn = self.reset_churn.saturating_add(1);
        if self.reset_churn > self.limits.rapid_reset_max_resets {
            self.report.rapid_reset_goaway += 1;
            self.enqueue_frame(goaway_frame(
                self.highest_client_stream_id,
                ERR_ENHANCE_YOUR_CALM,
            ))?;
            self.report.goaway_sent += 1;
            self.goaway = true;
            self.closing_after_write = true;
            self.drain_streams_for_connection_close(effects);
            return Ok(());
        }
        if self
            .remove_stream(frame.stream_id, effects, CallError::TargetClosed)
            .is_some()
        {
            self.report.reset_streams += 1;
            self.report.closed_streams += 1;
            self.emit_protocol_fact(
                effects,
                ProtocolFact::Http2StreamReset {
                    connection: self.connection_fact_id(),
                    stream: Http2StreamId::new(frame.stream_id),
                    direction: ProtocolDirection::Inbound,
                    reason: classify_h2_reset(reset_code),
                },
            );
            self.emit_protocol_fact(
                effects,
                ProtocolFact::Http2StreamClosed {
                    connection: self.connection_fact_id(),
                    stream: Http2StreamId::new(frame.stream_id),
                    reason: Http2CloseReason::EndStream,
                },
            );
            self.flush_deferred_request_window_credit();
        }
        Ok(())
    }

    fn dispatch_stream(
        &mut self,
        stream_id: u32,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        let idx = self
            .find_stream(stream_id)
            .ok_or(Http2ProtocolError::StreamClosed)?;
        let headers = self.streams[idx]
            .headers
            .take()
            .ok_or(Http2ProtocolError::InvalidPseudoHeaders)?;
        let parts = Http2RequestParts {
            method: headers
                .method
                .ok_or(Http2ProtocolError::InvalidPseudoHeaders)?,
            path: headers
                .path
                .ok_or(Http2ProtocolError::InvalidPseudoHeaders)?,
            headers: headers.headers,
            body: HttpRequestBody::Buffered(std::mem::take(&mut self.streams[idx].body)),
            grpc_content_type: headers.grpc_content_type,
            grpc_encoding_unsupported: headers.grpc_encoding_unsupported,
        };
        // The buffered body's flow bytes were already credited to the window
        // mid-upload (see `handle_data`); flush any sub-threshold remainder
        // now so the final credit reaches the peer. Streaming bodies credit
        // on consume, not here.
        if matches!(parts.body, HttpRequestBody::Buffered(_)) {
            self.maybe_flush_request_window_credit(stream_id, true)?;
        }
        let (effect, handle) = call_cancelable(
            self.service,
            M::from_http2_parts(parts),
            self.service_call_timeout,
        )
        .then(move |outcome| Http2ConnectionMsg::ServiceReturned { stream_id, outcome });
        if let Some(idx) = self.find_stream(stream_id) {
            self.streams[idx].pending_call = Some(handle);
        }
        effects.push(effect);
        Ok(())
    }

    fn dispatch_streaming_request(
        &mut self,
        stream_id: u32,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        let idx = self
            .find_stream(stream_id)
            .ok_or(Http2ProtocolError::StreamClosed)?;
        let headers = self.streams[idx]
            .headers
            .take()
            .ok_or(Http2ProtocolError::InvalidPseudoHeaders)?;
        self.streams[idx].request_dispatched_streaming = true;
        // Declared length was parsed and stored during header validation;
        // keep that value (which already accounts for invalid/duplicate
        // rejection) rather than re-parsing the headers here.
        let content_length = self.streams[idx].request_content_length;
        let source = tina::Address::new_with_generation(
            self.self_shard_id.expect("shard id captured"),
            self.self_isolate_id.expect("isolate id captured"),
            tina::AddressGeneration::new(0),
        );
        let parts = Http2RequestParts {
            method: headers
                .method
                .ok_or(Http2ProtocolError::InvalidPseudoHeaders)?,
            path: headers
                .path
                .ok_or(Http2ProtocolError::InvalidPseudoHeaders)?,
            headers: headers.headers,
            body: HttpRequestBody::Http2Stream(Http2RequestStream {
                stream_id,
                content_length,
                source,
            }),
            grpc_content_type: headers.grpc_content_type,
            grpc_encoding_unsupported: headers.grpc_encoding_unsupported,
        };
        let (effect, handle) = call_cancelable(
            self.service,
            M::from_http2_parts(parts),
            self.service_call_timeout,
        )
        .then(move |outcome| Http2ConnectionMsg::ServiceReturned { stream_id, outcome });
        if let Some(idx) = self.find_stream(stream_id) {
            self.streams[idx].pending_call = Some(handle);
        }
        effects.push(effect);
        Ok(())
    }

    fn handle_service_returned(
        &mut self,
        stream_id: u32,
        outcome: CallOutcome<HttpResponse>,
    ) -> Effect<Self> {
        if self.find_stream(stream_id).is_none() {
            self.report.late_replies_after_close += 1;
            return noop();
        }
        let grpc = self
            .find_stream(stream_id)
            .and_then(|idx| self.streams.get(idx))
            .is_some_and(|stream| stream.grpc);
        if let Some(idx) = self.find_stream(stream_id) {
            let _ = self.streams[idx].pending_call.take();
        }
        let mut effects = Vec::new();
        match outcome {
            CallOutcome::Replied(response) => {
                if let Err(error) = self.enqueue_response(stream_id, response, &mut effects) {
                    self.report.protocol_errors += 1;
                    let code = match error {
                        Http2ProtocolError::FlowControl => ERR_FLOW_CONTROL_ERROR,
                        _ => ERR_PROTOCOL_ERROR,
                    };
                    let _ = self.enqueue_frame(rst_stream_frame(stream_id, code));
                }
            }
            CallOutcome::Full => {
                self.report.stream_full += 1;
                let response = if grpc {
                    crate::grpc::grpc_status_http_response(crate::grpc::GrpcStatus::new(
                        crate::grpc::GrpcStatusCode::ResourceExhausted,
                    ))
                } else {
                    HttpResponse::service_unavailable()
                };
                let _ = self.enqueue_response(stream_id, response, &mut effects);
            }
            CallOutcome::Closed | CallOutcome::Rejected(_) => {
                let response = if grpc {
                    crate::grpc::grpc_status_http_response(crate::grpc::GrpcStatus::new(
                        crate::grpc::GrpcStatusCode::Internal,
                    ))
                } else {
                    HttpResponse::internal_error()
                };
                let _ = self.enqueue_response(stream_id, response, &mut effects);
            }
            CallOutcome::Timeout => {
                let response = if grpc {
                    crate::grpc::grpc_status_http_response(crate::grpc::GrpcStatus::new(
                        crate::grpc::GrpcStatusCode::DeadlineExceeded,
                    ))
                } else {
                    HttpResponse::gateway_timeout()
                };
                let _ = self.enqueue_response(stream_id, response, &mut effects);
            }
        }
        // Flush deferred request-window credit into the same write as the
        // response. The connection window-update then rides the response write
        // instead of forcing a second write (and a second completion turn)
        // after it finishes.
        self.flush_deferred_request_window_credit();
        if self.pending_write.is_empty() && !self.write_queue.is_empty() {
            effects.push(self.write_more());
        }
        if self
            .find_stream(stream_id)
            .and_then(|idx| self.streams[idx].response_source)
            .is_some()
        {
            effects.push(self.pull_response_chunk_effect(stream_id));
        }
        batch(effects)
    }

    fn enqueue_response(
        &mut self,
        stream_id: u32,
        response: HttpResponse,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        // Dispatch on the body kind while only borrowing, so the buffered
        // bytes can be *moved* into `PendingResponse` below instead of cloned.
        // `max_response_body_bytes` is validated here before the body is stored
        // or sent. Stream/ChunkedStream/WebSocket bodies return early.
        let body_len = match &response.body {
            HttpResponseBody::Buffered(bytes) => {
                if bytes.len() > self.limits.max_response_body_bytes {
                    self.report.stream_full += 1;
                    self.reset_active_stream_for_protocol(
                        stream_id,
                        ERR_ENHANCE_YOUR_CALM,
                        Http2ResetReason::EnhanceYourCalm,
                        effects,
                    );
                    return Ok(());
                }
                bytes.len()
            }
            HttpResponseBody::Shared(bytes) => {
                if bytes.len() > self.limits.max_response_body_bytes {
                    self.report.stream_full += 1;
                    self.reset_active_stream_for_protocol(
                        stream_id,
                        ERR_ENHANCE_YOUR_CALM,
                        Http2ResetReason::EnhanceYourCalm,
                        effects,
                    );
                    return Ok(());
                }
                bytes.len()
            }
            HttpResponseBody::Stream(stream) => {
                let source = stream.source;
                let content_length = stream.content_length;
                return self.begin_streaming_response(
                    stream_id,
                    &response,
                    source,
                    Some(content_length),
                );
            }
            HttpResponseBody::ChunkedStream(stream) => {
                let source = stream.source;
                return self.begin_streaming_response(stream_id, &response, source, None);
            }
            HttpResponseBody::WebSocket(_) => {
                return Err(Http2ProtocolError::UnsupportedFrame(FRAME_DATA));
            }
        };
        let block = encode_response_headers(&response, body_len);
        let trailers = encode_response_trailers(&response);
        let body = match response.body {
            HttpResponseBody::Buffered(bytes) => ResponseBytes::Owned(bytes),
            HttpResponseBody::Shared(bytes) => ResponseBytes::Shared(bytes),
            // Stream/ChunkedStream/WebSocket all returned above.
            _ => unreachable!("non-buffered response bodies handled above"),
        };
        self.queue_or_send_response(
            stream_id,
            PendingResponse {
                header_block: block,
                body,
                body_offset: 0,
                trailers,
            },
            effects,
        )
    }

    fn begin_streaming_response(
        &mut self,
        stream_id: u32,
        response: &HttpResponse,
        source: tina::Address<ResponseChunkMsg, ResponseChunkReply>,
        content_length: Option<usize>,
    ) -> Result<(), Http2ProtocolError> {
        let idx = self
            .find_stream(stream_id)
            .ok_or(Http2ProtocolError::StreamClosed)?;
        let block = encode_response_headers_with_len(response, content_length);
        let trailers = encode_response_trailers(response);
        self.ensure_outbound_slots(1)?;
        self.enqueue_frame(headers_frame(stream_id, false, block))?;
        self.streams[idx].response_source = Some(source);
        self.streams[idx].response_trailers = trailers;
        // Known-length streaming responses must send exactly `content_length`
        // bytes. `None` here means a chunked/unknown-length source.
        self.streams[idx].response_remaining_content_length = content_length;
        Ok(())
    }

    fn queue_or_send_response(
        &mut self,
        stream_id: u32,
        pending: PendingResponse,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        self.send_pending_response(stream_id, pending, effects)
    }

    fn send_pending_response(
        &mut self,
        stream_id: u32,
        pending: PendingResponse,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        let frame_cap = self.peer_max_frame_size.max(1);
        let idx = self
            .find_stream(stream_id)
            .ok_or(Http2ProtocolError::StreamClosed)?;
        let mut pending = pending;
        let send_len = self.outbound_response_credit(idx, pending.remaining_body().len())?;
        let send_len_i32 = i32::try_from(send_len).map_err(|_| Http2ProtocolError::FlowControl)?;
        let headers_len = pending.header_block.len();
        let data_frames = send_len.div_ceil(frame_cap);
        let body_done = pending.body_offset + send_len == pending.body.len();
        let trailers_len = if body_done {
            pending
                .trailers
                .as_ref()
                .map_or(0, |trailers| FRAME_HEADER_LEN + trailers.len())
        } else {
            0
        };
        let will_write = headers_len > 0 || send_len > 0 || (body_done && trailers_len > 0);

        if will_write {
            self.ensure_outbound_slots(1)?;
            let mut out = Vec::with_capacity(
                usize::from(headers_len > 0) * FRAME_HEADER_LEN
                    + headers_len
                    + data_frames * FRAME_HEADER_LEN
                    + send_len
                    + trailers_len,
            );
            if headers_len > 0 {
                let headers_end_stream = pending.body.is_empty() && pending.trailers.is_none();
                push_frame_header(
                    &mut out,
                    FRAME_HEADERS,
                    FLAG_END_HEADERS
                        | if headers_end_stream {
                            FLAG_END_STREAM
                        } else {
                            0
                        },
                    stream_id,
                    headers_len,
                );
                out.extend_from_slice(&pending.header_block);
                pending.header_block.clear();
            }
            if send_len > 0 {
                let start = pending.body_offset;
                let end = start + send_len;
                for (chunk_index, chunk) in pending.body.as_slice()[start..end]
                    .chunks(frame_cap)
                    .enumerate()
                {
                    let final_data = chunk_index + 1 == data_frames && body_done;
                    let end_stream = final_data && pending.trailers.is_none();
                    push_frame_header(
                        &mut out,
                        FRAME_DATA,
                        if end_stream { FLAG_END_STREAM } else { 0 },
                        stream_id,
                        chunk.len(),
                    );
                    out.extend_from_slice(chunk);
                }
                pending.body_offset = end;
                self.send_window -= send_len_i32;
                self.streams[idx].send_window -= send_len_i32;
                self.streams[idx].response_bytes_sent += send_len;
            }
            if body_done {
                if let Some(trailers) = pending.trailers.take() {
                    push_frame_header(
                        &mut out,
                        FRAME_HEADERS,
                        FLAG_END_HEADERS | FLAG_END_STREAM,
                        stream_id,
                        trailers.len(),
                    );
                    out.extend_from_slice(&trailers);
                }
            }
            self.write_queue.push_back(out);
        }

        if body_done {
            self.streams[idx].state = Http2StreamState::Closed;
            self.close_stream_with_cleanup(
                stream_id,
                Http2CloseReason::EndStream,
                effects,
                CallError::TargetClosed,
            );
        } else {
            let side = if self.send_window <= 0 {
                Http2FlowControlSide::ConnectionSend
            } else {
                Http2FlowControlSide::StreamSend
            };
            self.report.flow_control_blocked += 1;
            self.emit_protocol_fact(
                effects,
                ProtocolFact::Http2FlowControlFull {
                    connection: self.connection_fact_id(),
                    stream: Http2StreamId::new(stream_id),
                    side,
                },
            );
            self.streams[idx].pending_response = Some(pending);
        }
        Ok(())
    }

    fn pull_response_chunk_effect(&mut self, stream_id: u32) -> Effect<Self> {
        let Some(idx) = self.find_stream(stream_id) else {
            return noop();
        };
        let Some(source) = self.streams[idx].response_source else {
            return noop();
        };
        if self.streams[idx].response_pull_in_flight {
            return noop();
        }
        self.streams[idx].response_pull_in_flight = true;
        let (effect, handle) = call_cancelable(
            source,
            ResponseChunkMsg::Next,
            self.limits.response_stream_call_timeout,
        )
        .then(move |outcome| Http2ConnectionMsg::StreamChunk { stream_id, outcome });
        self.streams[idx].response_pull_handle = Some(handle);
        effect
    }

    fn flush_pending_responses(
        &mut self,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        let ids: Vec<u32> = self.streams.iter().map(|s| s.id).collect();
        for stream_id in ids {
            let Some(idx) = self.find_stream(stream_id) else {
                continue;
            };
            if let Some(pending) = self.streams[idx].pending_response.take() {
                self.send_pending_response(stream_id, pending, effects)?;
            }
            if self.find_stream(stream_id).is_some() {
                self.flush_response_stream(stream_id)?;
            }
        }
        Ok(())
    }

    fn handle_stream_chunk(
        &mut self,
        stream_id: u32,
        outcome: CallOutcome<ResponseChunkReply>,
    ) -> Effect<Self> {
        if self.find_stream(stream_id).is_none() {
            self.report.late_replies_after_close += 1;
            return noop();
        }
        if let Some(idx) = self.find_stream(stream_id) {
            self.streams[idx].response_pull_in_flight = false;
            self.streams[idx].response_pull_handle = None;
        }
        match outcome {
            CallOutcome::Replied(ResponseChunkReply::Chunk(bytes)) => {
                if bytes.is_empty() {
                    return self.handle_stream_chunk(
                        stream_id,
                        CallOutcome::Replied(ResponseChunkReply::Eof),
                    );
                }
                // Known-length responses must not overrun the declared
                // `content-length`. Detect and reset before the extra
                // bytes are queued for outbound delivery so the client
                // never sees an inflated success.
                let overrun = self
                    .find_stream(stream_id)
                    .and_then(|idx| self.streams[idx].response_remaining_content_length)
                    .is_some_and(|remaining| bytes.len() > remaining);
                if overrun {
                    self.report.protocol_errors += 1;
                    let mut effects = Vec::new();
                    self.reset_active_stream_for_protocol(
                        stream_id,
                        ERR_PROTOCOL_ERROR,
                        Http2ResetReason::ProtocolError,
                        &mut effects,
                    );
                    effects.push(self.maybe_write_effect());
                    return batch(effects);
                }
                let projected = self
                    .find_stream(stream_id)
                    .map(|idx| {
                        self.streams[idx]
                            .response_bytes_sent
                            .saturating_add(bytes.len())
                    })
                    .unwrap_or(usize::MAX);
                if projected > self.limits.max_response_body_bytes {
                    self.report.stream_full += 1;
                    let mut effects = vec![tina::fact::<Self>(ProtocolFact::HttpBodyHighWater {
                        connection: self.connection_fact_id(),
                        body_id: stream_id as u64,
                        direction: ProtocolDirection::Outbound,
                        buffered_bytes: projected as u64,
                        threshold_bytes: self.limits.max_response_body_bytes as u64,
                    })];
                    self.reset_active_stream_for_protocol(
                        stream_id,
                        ERR_ENHANCE_YOUR_CALM,
                        Http2ResetReason::EnhanceYourCalm,
                        &mut effects,
                    );
                    effects.push(self.maybe_write_effect());
                    return batch(effects);
                }
                if let Some(idx) = self.find_stream(stream_id) {
                    self.streams[idx]
                        .response_pending_data
                        .extend_from_slice(&bytes);
                    if let Some(remaining) =
                        self.streams[idx].response_remaining_content_length.as_mut()
                    {
                        *remaining -= bytes.len();
                    }
                }
                if self.flush_response_stream(stream_id).is_err() {
                    self.report.flow_control_blocked += 1;
                }
                let mut effects = Vec::new();
                if self.pending_write.is_empty() && !self.write_queue.is_empty() {
                    effects.push(self.write_more());
                }
                if self
                    .find_stream(stream_id)
                    .is_some_and(|idx| self.streams[idx].response_pending_data.is_empty())
                {
                    effects.push(self.pull_response_chunk_effect(stream_id));
                }
                batch(effects)
            }
            CallOutcome::Replied(ResponseChunkReply::Eof) => {
                // Known-length responses must have delivered exactly
                // `content-length` bytes before EOF. A short source
                // (remaining > 0) is a contract violation; reset rather
                // than send END_STREAM that would imply success.
                let short_source = self
                    .find_stream(stream_id)
                    .and_then(|idx| self.streams[idx].response_remaining_content_length)
                    .is_some_and(|remaining| remaining > 0);
                if short_source {
                    self.report.protocol_errors += 1;
                    let mut effects = Vec::new();
                    self.reset_active_stream_for_protocol(
                        stream_id,
                        ERR_PROTOCOL_ERROR,
                        Http2ResetReason::ProtocolError,
                        &mut effects,
                    );
                    effects.push(self.maybe_write_effect());
                    return batch(effects);
                }
                let trailers = self
                    .find_stream(stream_id)
                    .and_then(|idx| self.streams[idx].response_trailers.take());
                if let Some(trailers) = trailers {
                    let _ = self.enqueue_frame(headers_frame(stream_id, true, trailers));
                } else {
                    let _ = self.enqueue_frame(data_frame(stream_id, true, Vec::new()));
                }
                if let Some(idx) = self.find_stream(stream_id) {
                    self.streams[idx].state = Http2StreamState::Closed;
                }
                let mut effects = Vec::new();
                self.close_stream_with_cleanup(
                    stream_id,
                    Http2CloseReason::EndStream,
                    &mut effects,
                    CallError::TargetClosed,
                );
                effects.push(self.maybe_write_effect());
                batch(effects)
            }
            CallOutcome::Replied(ResponseChunkReply::GrpcStatus(status)) => {
                let grpc_status_code = crate::grpc::classify_grpc_status_code(&status);
                let trailers = crate::grpc::grpc_status_trailers_block(status);
                let _ = self.enqueue_frame(headers_frame(stream_id, true, trailers));
                if let Some(idx) = self.find_stream(stream_id) {
                    self.streams[idx].state = Http2StreamState::Closed;
                }
                let status_fact = tina::fact::<Self>(ProtocolFact::GrpcFinalStatusSent {
                    connection: self.connection_fact_id(),
                    stream: tina_runtime::GrpcStreamId::new(stream_id as u64),
                    status: grpc_status_code,
                });
                let mut effects = vec![status_fact];
                self.close_stream_with_cleanup(
                    stream_id,
                    Http2CloseReason::EndStream,
                    &mut effects,
                    CallError::TargetClosed,
                );
                effects.push(self.maybe_write_effect());
                batch(effects)
            }
            CallOutcome::Full
            | CallOutcome::Closed
            | CallOutcome::Rejected(_)
            | CallOutcome::Timeout => {
                self.report.stream_full += 1;
                let mut effects = Vec::new();
                self.reset_active_stream_for_protocol(
                    stream_id,
                    ERR_PROTOCOL_ERROR,
                    Http2ResetReason::ProtocolError,
                    &mut effects,
                );
                effects.push(self.maybe_write_effect());
                batch(effects)
            }
        }
    }

    fn flush_response_stream(&mut self, stream_id: u32) -> Result<(), Http2ProtocolError> {
        loop {
            let idx = self
                .find_stream(stream_id)
                .ok_or(Http2ProtocolError::StreamClosed)?;
            if self.streams[idx].response_pending_data.is_empty() {
                return Ok(());
            }
            let allowed = self
                .outbound_response_credit(idx, self.streams[idx].response_pending_data.len())?
                .min(self.peer_max_frame_size);
            if allowed == 0 {
                self.report.flow_control_blocked += 1;
                return Ok(());
            }
            // Frame the next streamed chunk directly: header bytes, then drain
            // the consumed prefix of the pending buffer straight into the
            // framed `Vec`. The body is copied once (the drain) instead of once
            // into a `chunk` `Vec` and again in `Frame::encode`.
            self.ensure_outbound_slots(1)?;
            let mut framed = Vec::with_capacity(FRAME_HEADER_LEN + allowed);
            push_frame_header(&mut framed, FRAME_DATA, 0, stream_id, allowed);
            framed.extend(self.streams[idx].response_pending_data.drain(..allowed));
            self.write_queue.push_back(framed);
            let allowed_i32 =
                i32::try_from(allowed).map_err(|_| Http2ProtocolError::FlowControl)?;
            self.send_window -= allowed_i32;
            self.streams[idx].send_window -= allowed_i32;
            self.streams[idx].response_bytes_sent += allowed;
        }
    }

    /// Returns the bytes a response may consume from both HTTP/2 send windows.
    /// Buffered and source-streamed bodies share this conversion boundary so
    /// neither path relies on a narrowing cast after checking elsewhere.
    fn outbound_response_credit(
        &self,
        stream_index: usize,
        remaining: usize,
    ) -> Result<usize, Http2ProtocolError> {
        let window = self.send_window.min(self.streams[stream_index].send_window);
        if window <= 0 {
            return Ok(0);
        }
        usize::try_from(window)
            .map(|credit| credit.min(remaining))
            .map_err(|_| Http2ProtocolError::FlowControl)
    }

    fn handle_request_body_next(
        &mut self,
        stream_id: u32,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        let Some(idx) = self.find_stream(stream_id) else {
            return call.reply(Http2ConnectionReply::RequestChunk(
                RequestChunkReply::Error(CallError::TargetClosed),
            ));
        };
        if self.streams[idx].pending_request_body_reply.is_some() {
            return call.reject(tina::CallRejectedReason::UnsupportedMessage);
        }
        self.streams[idx].pending_request_body_reply = Some(call.into_request_context());
        match self.reply_pending_request_chunk(stream_id) {
            Ok(effect) => effect,
            Err(_) => {
                if let Some(idx) = self.find_stream(stream_id) {
                    if let Some(call) = self.streams[idx].pending_request_body_reply.take() {
                        return reply_to(
                            call,
                            Http2ConnectionReply::RequestChunk(RequestChunkReply::Error(
                                CallError::Io,
                            )),
                        );
                    }
                }
                noop()
            }
        }
    }

    fn reset_active_stream_for_protocol(
        &mut self,
        stream_id: u32,
        code: u32,
        reason: Http2ResetReason,
        effects: &mut Vec<Effect<Self>>,
    ) {
        self.reset_stream_with_cleanup(stream_id, code, reason, effects, CallError::Io);
    }

    fn reset_stream_with_cleanup(
        &mut self,
        stream_id: u32,
        code: u32,
        reason: Http2ResetReason,
        effects: &mut Vec<Effect<Self>>,
        request_error: CallError,
    ) {
        let _ = self.enqueue_frame(rst_stream_frame(stream_id, code));
        self.report.reset_streams += 1;
        self.emit_protocol_fact(
            effects,
            ProtocolFact::Http2StreamReset {
                connection: self.connection_fact_id(),
                stream: Http2StreamId::new(stream_id),
                direction: ProtocolDirection::Outbound,
                reason,
            },
        );
        self.close_stream_with_cleanup(
            stream_id,
            Http2CloseReason::EndStream,
            effects,
            request_error,
        );
    }

    fn reset_missing_stream(
        &mut self,
        stream_id: u32,
        code: u32,
        reason: Http2ResetReason,
        effects: &mut Vec<Effect<Self>>,
    ) {
        let _ = self.enqueue_frame(rst_stream_frame(stream_id, code));
        self.report.reset_streams += 1;
        self.emit_protocol_fact(
            effects,
            ProtocolFact::Http2StreamReset {
                connection: self.connection_fact_id(),
                stream: Http2StreamId::new(stream_id),
                direction: ProtocolDirection::Outbound,
                reason,
            },
        );
    }

    fn close_stream_with_cleanup(
        &mut self,
        stream_id: u32,
        reason: Http2CloseReason,
        effects: &mut Vec<Effect<Self>>,
        request_error: CallError,
    ) {
        if self
            .remove_stream(stream_id, effects, request_error)
            .is_some()
        {
            self.report.closed_streams += 1;
            self.emit_protocol_fact(
                effects,
                ProtocolFact::Http2StreamClosed {
                    connection: self.connection_fact_id(),
                    stream: Http2StreamId::new(stream_id),
                    reason,
                },
            );
        }
    }

    fn drain_streams_for_connection_close(&mut self, effects: &mut Vec<Effect<Self>>) {
        let ids: Vec<u32> = self.streams.iter().map(|stream| stream.id).collect();
        for stream_id in ids {
            self.close_stream_with_cleanup(
                stream_id,
                Http2CloseReason::GoAway,
                effects,
                CallError::TargetClosed,
            );
        }
    }

    fn cancel_response_source(
        &mut self,
        stream_id: u32,
        stream: &mut ActiveStream,
        effects: &mut Vec<Effect<Self>>,
    ) {
        if let Some(handle) = stream.response_pull_handle.take() {
            effects.push(cancel_call(handle).then(move |outcome| {
                Http2ConnectionMsg::StreamSourcePullCancelled { stream_id, outcome }
            }));
        }
        if let Some(source) = stream.response_source.take() {
            effects.push(
                call(
                    source,
                    ResponseChunkMsg::Cancel,
                    self.limits.response_stream_call_timeout,
                )
                .then(move |outcome| Http2ConnectionMsg::StreamSourceCancelDone {
                    stream_id,
                    outcome,
                }),
            );
        }
    }

    fn reply_pending_request_chunk(
        &mut self,
        stream_id: u32,
    ) -> Result<Effect<Self>, Http2ProtocolError> {
        let Some(idx) = self.find_stream(stream_id) else {
            return Ok(noop());
        };
        let Some(call) = self.streams[idx].pending_request_body_reply.take() else {
            return Ok(noop());
        };
        if let Some(mut chunk) = self.streams[idx].request_chunks.pop_front() {
            let cap = self.limits.request_stream_chunk_size.max(1);
            if chunk.data.len() > cap {
                let rest = chunk.data.split_off(cap);
                let rest_credit = rest.len();
                chunk.flow_credit = chunk.flow_credit.saturating_sub(rest_credit);
                self.streams[idx]
                    .request_chunks
                    .push_front(RequestDataChunk {
                        data: rest,
                        flow_credit: rest_credit,
                    });
            }
            let credit = chunk.flow_credit;
            let data = chunk.data;
            self.add_request_window_credit(idx, credit);
            self.maybe_flush_request_window_credit(stream_id, false)?;
            return Ok(reply_to(
                call,
                Http2ConnectionReply::RequestChunk(RequestChunkReply::Chunk(data)),
            ));
        }
        if self.streams[idx].request_eof {
            self.maybe_flush_request_window_credit(stream_id, true)?;
            Ok(reply_to(
                call,
                Http2ConnectionReply::RequestChunk(RequestChunkReply::Eof),
            ))
        } else {
            self.streams[idx].pending_request_body_reply = Some(call);
            Ok(noop())
        }
    }

    fn add_request_window_credit(&mut self, stream_index: usize, credit: usize) {
        let credit_i32 = i32::try_from(credit).unwrap_or(i32::MAX);
        let credit_u32 = u32::try_from(credit).unwrap_or(u32::MAX);
        self.recv_window = self.recv_window.saturating_add(credit_i32);
        self.pending_recv_window_credit =
            self.pending_recv_window_credit.saturating_add(credit_u32);
        self.streams[stream_index].recv_window = self.streams[stream_index]
            .recv_window
            .saturating_add(credit_i32);
        self.streams[stream_index].pending_recv_window_credit = self.streams[stream_index]
            .pending_recv_window_credit
            .saturating_add(credit_u32);
    }

    fn add_connection_window_credit(&mut self, credit: usize) {
        let credit_u32 = u32::try_from(credit).unwrap_or(u32::MAX);
        self.pending_recv_window_credit =
            self.pending_recv_window_credit.saturating_add(credit_u32);
    }

    fn return_dropped_request_credit(&mut self, stream: &mut ActiveStream) {
        let credit = stream
            .request_chunks
            .drain(..)
            .fold(0usize, |sum, chunk| sum.saturating_add(chunk.flow_credit));
        if credit == 0 {
            return;
        }
        let credit_i32 = i32::try_from(credit).unwrap_or(i32::MAX);
        self.recv_window = self.recv_window.saturating_add(credit_i32);
        self.add_connection_window_credit(credit);
    }

    fn maybe_write_effect(&mut self) -> Effect<Self> {
        if self.pending_write.is_empty() && !self.write_queue.is_empty() {
            self.write_more()
        } else {
            noop()
        }
    }

    fn maybe_flush_request_window_credit(
        &mut self,
        stream_id: u32,
        force: bool,
    ) -> Result<(), Http2ProtocolError> {
        let stream_index = self.find_stream(stream_id);
        let conn_credit = self.pending_recv_window_credit;
        let stream_credit = stream_index
            .map(|idx| self.streams[idx].pending_recv_window_credit)
            .unwrap_or(0);
        let send_conn = conn_credit > 0 && (force || conn_credit >= WINDOW_CREDIT_FLUSH_THRESHOLD);
        let send_stream =
            stream_credit > 0 && (force || stream_credit >= WINDOW_CREDIT_FLUSH_THRESHOLD);
        let slots_needed = usize::from(send_conn) + usize::from(send_stream);
        if slots_needed == 0 {
            return Ok(());
        }
        if self.write_queue.len().saturating_add(slots_needed)
            > self.limits.connection_outbound_queue_capacity
        {
            return Ok(());
        }
        if send_conn {
            self.pending_recv_window_credit = 0;
            self.enqueue_frame(window_update_frame(0, conn_credit))?;
        }
        if send_stream {
            if let Some(idx) = stream_index {
                self.streams[idx].pending_recv_window_credit = 0;
            }
            self.enqueue_frame(window_update_frame(stream_id, stream_credit))?;
        }
        Ok(())
    }

    fn push_ready_response_pulls(&mut self, effects: &mut Vec<Effect<Self>>) {
        let ids: Vec<u32> = self
            .streams
            .iter()
            .filter(|stream| {
                stream.response_source.is_some()
                    && stream.response_pending_data.is_empty()
                    && !stream.response_pull_in_flight
            })
            .map(|stream| stream.id)
            .collect();
        for stream_id in ids {
            effects.push(self.pull_response_chunk_effect(stream_id));
        }
    }

    fn handle_wrote(&mut self, reply: TcpWriteOwnedReply) -> Effect<Self> {
        self.write_in_flight = false;
        let TcpWriteOwnedReply { mut bytes, written } = reply;
        let drain = written.min(bytes.len());
        bytes.drain(..drain);
        self.pending_write = bytes;
        // A short write leaves a remainder; keep draining it before anything
        // else. Coalesced writes are larger, so this guard must stay correct.
        if !self.pending_write.is_empty() {
            return self.write_more();
        }
        if !self.write_queue.is_empty() {
            return self.write_more();
        }
        if self.closing_after_write {
            return self.close_now();
        }
        // Fallback flush for credit that became pending after the response left
        // (e.g. streamed bodies). The steady-state response path flushes credit
        // into the response write itself, so this usually finds nothing.
        self.flush_deferred_request_window_credit();
        let mut effects = Vec::new();
        if self.flush_pending_responses(&mut effects).is_err() {
            self.report.protocol_errors += 1;
        }
        if !self.write_queue.is_empty() {
            effects.push(self.write_more());
        }
        batch(effects)
    }

    fn flush_deferred_request_window_credit(&mut self) {
        if self.pending_recv_window_credit == 0
            && self
                .streams
                .iter()
                .all(|stream| stream.pending_recv_window_credit == 0)
        {
            return;
        }

        if self.pending_recv_window_credit > 0 {
            if self.maybe_flush_request_window_credit(0, true).is_err() {
                self.report.protocol_errors += 1;
                return;
            }
            if self.write_queue.len() >= self.limits.connection_outbound_queue_capacity {
                return;
            }
        }
        let stream_ids: Vec<u32> = self
            .streams
            .iter()
            .filter(|stream| stream.pending_recv_window_credit > 0)
            .map(|stream| stream.id)
            .collect();
        for stream_id in stream_ids {
            if self
                .maybe_flush_request_window_credit(stream_id, true)
                .is_err()
            {
                self.report.protocol_errors += 1;
                return;
            }
            if self.write_queue.len() >= self.limits.connection_outbound_queue_capacity {
                return;
            }
        }
    }

    /// Build the server's initial (non-ACK) SETTINGS from config. The peer
    /// learns our caps only from this frame; an empty one leaves it on
    /// protocol defaults (unlimited streams, 65535 window, 16384 frame).
    /// Mirrors the client's advertisement.
    fn initial_settings_frame(&self) -> Frame {
        let mut payload = Vec::with_capacity(24);
        push_setting(
            &mut payload,
            SETTINGS_MAX_CONCURRENT_STREAMS,
            self.limits.max_concurrent_streams as u32,
        );
        push_setting(
            &mut payload,
            SETTINGS_INITIAL_WINDOW_SIZE,
            self.limits.initial_stream_window as u32,
        );
        push_setting(
            &mut payload,
            SETTINGS_MAX_FRAME_SIZE,
            self.limits.max_frame_size as u32,
        );
        push_setting(&mut payload, SETTINGS_ENABLE_PUSH, 0);
        Frame::new(FRAME_SETTINGS, 0, 0, payload)
    }

    fn enqueue_frame(&mut self, frame: Frame) -> Result<(), Http2ProtocolError> {
        self.ensure_outbound_slots(1)?;
        self.write_queue.push_back(frame.encode());
        Ok(())
    }

    fn ensure_outbound_slots(&mut self, slots_needed: usize) -> Result<(), Http2ProtocolError> {
        if self.write_queue.len() >= self.limits.connection_outbound_queue_capacity {
            self.report.connection_full += 1;
            return Err(Http2ProtocolError::StreamLimitFull);
        }
        if self.write_queue.len().saturating_add(slots_needed)
            > self.limits.connection_outbound_queue_capacity
        {
            self.report.connection_full += 1;
            return Err(Http2ProtocolError::StreamLimitFull);
        }
        Ok(())
    }

    fn find_stream(&self, stream_id: u32) -> Option<usize> {
        self.stream_index.get(&stream_id).copied()
    }

    /// Append a stream and record its slot in the index.
    fn push_stream(&mut self, stream: ActiveStream) {
        self.stream_index.insert(stream.id, self.streams.len());
        self.streams.push(stream);
    }

    fn remove_stream(
        &mut self,
        stream_id: u32,
        effects: &mut Vec<Effect<Self>>,
        request_error: CallError,
    ) -> Option<ActiveStream> {
        let mut stream = self.remove_stream_from_table(stream_id)?;
        self.return_dropped_request_credit(&mut stream);
        if let Some(call) = stream.pending_request_body_reply.take() {
            effects.push(reply_to(
                call,
                Http2ConnectionReply::RequestChunk(RequestChunkReply::Error(request_error)),
            ));
        }
        if let Some(handle) = stream.pending_call.take() {
            effects.push(
                cancel_call(handle).then(move |outcome| Http2ConnectionMsg::ServiceCancelled {
                    stream_id,
                    outcome,
                }),
            );
        }
        self.cancel_response_source(stream_id, &mut stream, effects);
        Some(stream)
    }

    fn remove_stream_from_table(&mut self, stream_id: u32) -> Option<ActiveStream> {
        let idx = self.find_stream(stream_id)?;
        self.stream_index.remove(&stream_id);
        let removed = self.streams.swap_remove(idx);
        // `swap_remove` moved the last element into `idx` (unless it *was* the
        // last); re-point its index entry so the map stays consistent.
        if let Some(moved) = self.streams.get(idx) {
            self.stream_index.insert(moved.id, idx);
        }
        Some(removed)
    }
}

/// Inbound messages for [`Http2Listener`].
#[derive(Debug, Clone)]
pub enum Http2ListenerMsg {
    Start,
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Accepted(Result<(StreamId, SocketAddr), CallError>),
    Stop,
    ListenerClosed(Result<(), CallError>),
    StreamClosed(Result<(), CallError>),
}

/// Prior-knowledge h2c listener.
pub struct Http2Listener<S: Shard + 'static, M: Http2ServiceMessage = HttpRequest> {
    bind_addr: SocketAddr,
    service: Address<M, HttpResponse>,
    config: Http2ServerConfig,
    listener: Option<ListenerId>,
    started: bool,
    stopping: bool,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static, M: Http2ServiceMessage> Http2Listener<S, M> {
    pub fn new(
        bind_addr: SocketAddr,
        service: Address<M, HttpResponse>,
        config: Http2ServerConfig,
    ) -> Self {
        Self {
            bind_addr,
            service,
            config,
            listener: None,
            started: false,
            stopping: false,
            _shard: PhantomData,
        }
    }
}

impl<S: Shard + 'static, M: Http2ServiceMessage> Isolate for Http2Listener<S, M> {
    tina::isolate_types! {
        message: Http2ListenerMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: ChildDefinition<Http2Connection<S, M>>,
        io: tina_runtime::RuntimeCall<Http2ListenerMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: Http2ListenerMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            Http2ListenerMsg::Start => {
                if self.started {
                    return noop();
                }
                self.started = true;
                tcp_bind(self.bind_addr).then(Http2ListenerMsg::Bound)
            }
            Http2ListenerMsg::Bound(Ok((listener, _addr))) => {
                self.listener = Some(listener);
                if self.stopping {
                    let listener = self.listener.take().expect("listener just set");
                    return tcp_close_listener(listener).then(Http2ListenerMsg::ListenerClosed);
                }
                tcp_accept(listener).then(Http2ListenerMsg::Accepted)
            }
            Http2ListenerMsg::Bound(Err(_)) => stop(),
            Http2ListenerMsg::Accepted(Ok((stream, _peer))) => {
                if self.stopping {
                    return tcp_close_stream(stream).then(Http2ListenerMsg::StreamClosed);
                }
                let Some(listener) = self.listener else {
                    return tcp_close_stream(stream).then(Http2ListenerMsg::StreamClosed);
                };
                batch(vec![
                    spawn(
                        ChildDefinition::new(
                            Http2Connection::<S, M>::new(
                                stream,
                                self.service,
                                self.config.limits,
                                self.config.service_call_timeout,
                            ),
                            self.config.connection_mailbox_capacity,
                        )
                        .with_initial_message(Http2ConnectionMsg::Begin),
                    ),
                    tcp_accept(listener).then(Http2ListenerMsg::Accepted),
                ])
            }
            Http2ListenerMsg::Accepted(Err(error)) => {
                if self.stopping {
                    return stop();
                }
                let Some(listener) = self.listener else {
                    return stop();
                };
                match error {
                    CallError::Io => tcp_accept(listener).then(Http2ListenerMsg::Accepted),
                    _ => {
                        self.stopping = true;
                        if let Some(listener) = self.listener.take() {
                            tcp_close_listener(listener).then(Http2ListenerMsg::ListenerClosed)
                        } else {
                            stop()
                        }
                    }
                }
            }
            Http2ListenerMsg::Stop => {
                self.stopping = true;
                if let Some(listener) = self.listener.take() {
                    tcp_close_listener(listener).then(Http2ListenerMsg::ListenerClosed)
                } else {
                    stop()
                }
            }
            Http2ListenerMsg::ListenerClosed(_) => stop(),
            Http2ListenerMsg::StreamClosed(_) => noop(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct UnitShard;

    impl Shard for UnitShard {
        fn id(&self) -> ShardId {
            ShardId::new(9001)
        }
    }

    fn unit_connection() -> Http2Connection<UnitShard> {
        Http2Connection::new(
            StreamId::new(99),
            Address::new_with_generation(
                ShardId::new(9001),
                IsolateId::new(1),
                tina::AddressGeneration::new(0),
            ),
            Http2Limits::default(),
            Duration::from_secs(1),
        )
    }

    #[test]
    fn write_more_merges_small_frames_but_bounds_large_ones() {
        // The hot path: a small response and its window-update coalesce into one
        // write, draining the queue.
        let mut conn = unit_connection();
        conn.write_queue.push_back(vec![1u8; 200]);
        conn.write_queue.push_back(vec![2u8; 13]);
        let _ = conn.write_more();
        assert!(
            conn.write_queue.is_empty(),
            "small frames coalesce into one write"
        );

        // The bound: large queued buffers are not all copied into one write.
        // A buffer at the peer frame size fills the write; the rest stay queued.
        let mut conn = unit_connection();
        let frame_cap = conn.peer_max_frame_size;
        conn.write_queue.push_back(vec![1u8; frame_cap]);
        conn.write_queue.push_back(vec![2u8; 1024]);
        conn.write_queue.push_back(vec![3u8; frame_cap]);
        let _ = conn.write_more();
        assert_eq!(
            conn.write_queue.len(),
            2,
            "only the first buffer is written; the rest stay queued"
        );

        let mut conn = unit_connection();
        conn.limits.connection_outbound_queue_capacity = 1;
        conn.write_in_flight = true;
        conn.write_queue.push_back(vec![0; FRAME_HEADER_LEN]);
        conn.push_stream(ActiveStream::new(
            1,
            HeaderBlock::default(),
            DEFAULT_WINDOW,
            DEFAULT_WINDOW,
            false,
        ));
        let _ = conn.handle_stream_chunk(
            1,
            CallOutcome::Replied(ResponseChunkReply::Chunk(b"abc".to_vec())),
        );
        assert!(
            conn.find_stream(1)
                .is_some_and(|idx| conn.streams[idx].response_pending_data == b"abc"),
            "queue-cap flush failure parks the chunk instead of dropping it"
        );
        let _ = conn.handle_wrote(TcpWriteOwnedReply {
            bytes: Vec::new(),
            written: 0,
        });
        let bytes = std::mem::take(&mut conn.pending_write);
        let written = bytes.len();
        let _ = conn.handle_wrote(TcpWriteOwnedReply { bytes, written });
        assert!(
            conn.find_stream(1)
                .is_some_and(|idx| conn.streams[idx].response_pending_data.is_empty()),
            "write drain should retry and drain parked streamed response DATA"
        );
        assert!(conn.write_in_flight, "retry should arm a TCP write");
    }

    #[test]
    fn frame_round_trip_waits_for_complete_payload() {
        let limits = Http2Limits::default();
        let frame = Frame::new(FRAME_DATA, FLAG_END_STREAM, 1, b"abc".to_vec()).encode();
        assert!(
            try_decode_frame(&frame[..frame.len() - 1], limits.max_frame_size)
                .unwrap()
                .is_none()
        );
        let (decoded, used) = try_decode_frame(&frame, limits.max_frame_size)
            .unwrap()
            .unwrap();
        assert_eq!(used, frame.len());
        assert_eq!(decoded.ty, FRAME_DATA);
        assert_eq!(decoded.flags, FLAG_END_STREAM);
        assert_eq!(decoded.stream_id, 1);
        assert_eq!(decoded.payload, b"abc");
    }

    #[test]
    fn frame_size_cap_fires_before_payload_allocation() {
        let limits = Http2Limits {
            max_frame_size: 1,
            ..Http2Limits::default()
        };
        let frame = Frame::new(FRAME_DATA, 0, 1, b"abc".to_vec()).encode();
        assert!(matches!(
            try_decode_frame(&frame[..FRAME_HEADER_LEN], limits.max_frame_size),
            Err(Http2ProtocolError::FrameTooLarge { len: 3, max: 1 })
        ));
    }

    #[test]
    fn hpack_literal_request_headers_decode_with_limit() {
        let mut block = Vec::new();
        encode_literal_header(":method", "GET", &mut block);
        encode_literal_header(":scheme", "http", &mut block);
        encode_literal_header(":path", "/counter", &mut block);
        encode_literal_header("x-test", "ok", &mut block);
        let headers = decode_headers_block(&block, 1024).unwrap();
        assert_eq!(headers.method, Some(Method::GET));
        assert_eq!(headers.path.as_deref(), Some("/counter"));
        assert_eq!(headers.headers["x-test"], "ok");
        assert!(matches!(
            decode_headers_block(&block, 4),
            Err(Http2ProtocolError::HeadersTooLarge)
        ));
    }

    fn decode_settings_payload(payload: &[u8]) -> Vec<(u16, u32)> {
        assert_eq!(
            payload.len() % 6,
            0,
            "SETTINGS payload must be a 6-byte multiple"
        );
        payload
            .chunks_exact(6)
            .map(|c| {
                let id = u16::from_be_bytes([c[0], c[1]]);
                let value = u32::from_be_bytes([c[2], c[3], c[4], c[5]]);
                (id, value)
            })
            .collect()
    }

    #[test]
    fn server_initial_settings_advertises_configured_limits() {
        // The server's initial (non-ACK) SETTINGS must tell the peer the
        // configured caps; an empty payload leaves the peer on protocol
        // defaults (unlimited concurrent streams, 65535 window, 16384 frame).
        let mut conn = unit_connection();
        conn.read_buf.extend_from_slice(CLIENT_PREFACE);
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        conn.process_buffer(&mut effects)
            .expect("preface processes cleanly");

        let (settings, _) = try_decode_frame(&conn.write_queue[0], conn.limits.max_frame_size)
            .expect("complete queued frame")
            .expect("queued SETTINGS decodes");
        assert_eq!(settings.ty, FRAME_SETTINGS);
        assert_eq!(settings.flags, 0, "initial SETTINGS is not an ACK");
        let advertised = decode_settings_payload(&settings.payload);
        let limits = Http2Limits::default();
        assert!(
            advertised
                .iter()
                .any(|&(id, v)| id == SETTINGS_MAX_CONCURRENT_STREAMS
                    && v == limits.max_concurrent_streams as u32),
            "SETTINGS must advertise MAX_CONCURRENT_STREAMS, got {advertised:?}"
        );
        assert!(
            advertised
                .iter()
                .any(|&(id, v)| id == SETTINGS_INITIAL_WINDOW_SIZE
                    && v == limits.initial_stream_window as u32),
            "SETTINGS must advertise INITIAL_WINDOW_SIZE, got {advertised:?}"
        );
        assert!(
            advertised
                .iter()
                .any(|&(id, v)| id == SETTINGS_MAX_FRAME_SIZE && v == limits.max_frame_size as u32),
            "SETTINGS must advertise MAX_FRAME_SIZE, got {advertised:?}"
        );
        assert!(
            advertised
                .iter()
                .any(|&(id, v)| id == SETTINGS_ENABLE_PUSH && v == 0),
            "SETTINGS must disable server push, got {advertised:?}"
        );
    }

    #[test]
    fn window_update_overflow_is_typed() {
        assert_eq!(
            add_window(i32::MAX, 1),
            Err(Http2ProtocolError::WindowOverflow)
        );
    }

    #[test]
    fn zero_window_update_is_protocol_error() {
        let mut conn = unit_connection();
        let frame = Frame::new(FRAME_WINDOW_UPDATE, 0, 0, 0_u32.to_be_bytes().to_vec());
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        assert_eq!(
            conn.handle_window_update_frame(frame, &mut effects),
            Err(Http2ProtocolError::WindowOverflow)
        );
    }

    #[test]
    fn zero_stream_window_update_resets_only_that_stream() {
        let mut conn = unit_connection();
        conn.push_stream(ActiveStream::new(
            1,
            HeaderBlock::default(),
            DEFAULT_WINDOW,
            DEFAULT_WINDOW,
            false,
        ));
        let frame = Frame::new(FRAME_WINDOW_UPDATE, 0, 1, 0_u32.to_be_bytes().to_vec());
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();

        conn.handle_window_update_frame(frame, &mut effects)
            .expect("stream zero WINDOW_UPDATE should not GOAWAY connection");

        assert!(conn.find_stream(1).is_none(), "bad stream is removed");
        assert_eq!(conn.write_queue.len(), 1);
        let (queued, _) = try_decode_frame(&conn.write_queue[0], conn.limits.max_frame_size)
            .expect("complete queued frame")
            .expect("queued RST decodes");
        assert_eq!(queued.ty, FRAME_RST_STREAM);
        assert_eq!(queued.stream_id, 1);
        let mut code = [0_u8; 4];
        code.copy_from_slice(&queued.payload);
        assert_eq!(u32::from_be_bytes(code), ERR_PROTOCOL_ERROR);
        let facts = collect_facts(&effects);
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::Http2StreamReset {
                    reason: Http2ResetReason::ProtocolError,
                    direction: ProtocolDirection::Outbound,
                    ..
                }
            )),
            "expected outbound protocol reset fact, got {facts:?}",
        );
    }

    #[test]
    fn stream_index_stays_consistent_across_swap_remove() {
        // `find_stream` is O(1) via the id→slot index; it must agree with the
        // Vec after a `swap_remove` moves the tail element into the hole.
        let mut conn = unit_connection();
        for id in [1u32, 3, 5, 7] {
            conn.push_stream(ActiveStream::new(
                id,
                HeaderBlock::default(),
                DEFAULT_WINDOW,
                DEFAULT_WINDOW,
                false,
            ));
        }
        // Every id resolves to the slot the Vec actually holds.
        for id in [1u32, 3, 5, 7] {
            let idx = conn.find_stream(id).expect("stream present");
            assert_eq!(conn.streams[idx].id, id);
        }

        // Remove a middle stream: `swap_remove` moves id 7 into its slot.
        let removed = conn.remove_stream_from_table(3).expect("removed stream 3");
        assert_eq!(removed.id, 3);
        assert!(conn.find_stream(3).is_none(), "removed id is gone");
        for id in [1u32, 5, 7] {
            let idx = conn.find_stream(id).expect("survivor present");
            assert_eq!(
                conn.streams[idx].id, id,
                "index must still point at the right slot after swap_remove"
            );
        }
        assert_eq!(conn.streams.len(), 3);
        assert_eq!(conn.stream_index.len(), 3);

        // Remove the tail (no swap needed) and the head (swap moves tail in).
        conn.remove_stream_from_table(7).expect("removed tail");
        conn.remove_stream_from_table(1).expect("removed head");
        assert_eq!(conn.find_stream(5).map(|i| conn.streams[i].id), Some(5));
        assert_eq!(conn.streams.len(), 1);
        assert_eq!(conn.stream_index.len(), 1);

        let mut conn = unit_connection();
        conn.preface_seen = true;
        conn.highest_client_stream_id = 1;
        conn.push_stream(ActiveStream::new(
            1,
            HeaderBlock::default(),
            DEFAULT_WINDOW,
            DEFAULT_WINDOW,
            false,
        ));
        conn.remove_stream_from_table(1).expect("stream removed");
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();

        conn.handle_data_frame(Frame::new(FRAME_DATA, 0, 1, b"abc".to_vec()), &mut effects)
            .expect("late DATA on a closed stream is stream-scoped");
        assert_eq!(
            queued_window_update_increment(&conn, 0),
            3,
            "discarded DATA must return connection credit"
        );
        let facts = collect_facts(&effects);
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::Http2StreamReset {
                    reason: Http2ResetReason::StreamClosed,
                    direction: ProtocolDirection::Outbound,
                    ..
                }
            )),
            "expected outbound stream-closed reset fact, got {facts:?}",
        );
    }

    fn queued_window_update_increment(conn: &Http2Connection<UnitShard>, stream_id: u32) -> u32 {
        conn.write_queue
            .iter()
            .filter_map(|bytes| {
                try_decode_frame(bytes, conn.limits.max_frame_size)
                    .expect("queued frame decodes")
                    .map(|(frame, _)| frame)
            })
            .find(|frame| frame.ty == FRAME_WINDOW_UPDATE && frame.stream_id == stream_id)
            .map(|frame| {
                let mut buf = [0_u8; 4];
                buf.copy_from_slice(&frame.payload);
                u32::from_be_bytes(buf) & 0x7fff_ffff
            })
            .unwrap_or(0)
    }

    fn assert_data_reject_returns_connection_credit(
        label: &str,
        conn: &mut Http2Connection<UnitShard>,
        stream_id: u32,
        payload: &[u8],
    ) {
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        conn.handle_data_frame(
            Frame::new(FRAME_DATA, 0, stream_id, payload.to_vec()),
            &mut effects,
        )
        .unwrap_or_else(|err| panic!("{label}: DATA reject should stay stream-scoped: {err:?}"));
        assert_eq!(
            queued_window_update_increment(conn, 0),
            payload.len() as u32,
            "{label}: rejected DATA must return consumed connection credit"
        );
    }

    #[test]
    fn data_reject_paths_return_connection_window_credit() {
        {
            let mut conn = unit_connection();
            conn.highest_client_stream_id = 1;
            assert_data_reject_returns_connection_credit(
                "closed stream missing from table",
                &mut conn,
                1,
                b"abc",
            );
        }
        {
            let mut conn = unit_connection();
            let mut stream = ActiveStream::new(
                1,
                HeaderBlock::default(),
                DEFAULT_WINDOW,
                DEFAULT_WINDOW,
                false,
            );
            stream.state = Http2StreamState::Closed;
            conn.push_stream(stream);
            assert_data_reject_returns_connection_credit(
                "closed stream state",
                &mut conn,
                1,
                b"abc",
            );
        }
        {
            let mut conn = unit_connection();
            let mut stream = ActiveStream::new(
                1,
                HeaderBlock::default(),
                DEFAULT_WINDOW,
                DEFAULT_WINDOW,
                false,
            );
            stream.request_eof = true;
            conn.push_stream(stream);
            assert_data_reject_returns_connection_credit(
                "DATA after request EOF",
                &mut conn,
                1,
                b"abc",
            );
        }
        {
            let mut conn = unit_connection();
            conn.recv_window = DEFAULT_WINDOW;
            conn.push_stream(ActiveStream::new(
                1,
                HeaderBlock::default(),
                1,
                DEFAULT_WINDOW,
                false,
            ));
            assert_data_reject_returns_connection_credit(
                "stream receive window exceeded",
                &mut conn,
                1,
                b"abc",
            );
        }
        {
            let mut conn = unit_connection();
            let mut stream = ActiveStream::new(
                1,
                HeaderBlock::default(),
                DEFAULT_WINDOW,
                DEFAULT_WINDOW,
                false,
            );
            stream.request_content_length = Some(2);
            stream.request_bytes_received = 1;
            conn.push_stream(stream);
            assert_data_reject_returns_connection_credit(
                "content-length overrun",
                &mut conn,
                1,
                b"ab",
            );
        }
        {
            let mut conn = unit_connection();
            conn.limits.max_body_bytes = 1;
            conn.push_stream(ActiveStream::new(
                1,
                HeaderBlock::default(),
                DEFAULT_WINDOW,
                DEFAULT_WINDOW,
                false,
            ));
            assert_data_reject_returns_connection_credit("body cap exceeded", &mut conn, 1, b"ab");
        }
    }

    #[test]
    fn malformed_priority_resets_open_stream_but_ignores_idle_stream() {
        let mut conn = unit_connection();
        conn.push_stream(ActiveStream::new(
            1,
            HeaderBlock::default(),
            DEFAULT_WINDOW,
            DEFAULT_WINDOW,
            false,
        ));
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        conn.handle_priority(Frame::new(FRAME_PRIORITY, 0, 1, vec![0, 1]), &mut effects)
            .expect("malformed PRIORITY on open stream is stream-scoped");
        assert_eq!(conn.write_queue.len(), 1);
        let facts = collect_facts(&effects);
        assert!(facts.iter().any(|fact| matches!(
            fact,
            ProtocolFact::Http2StreamReset {
                reason: Http2ResetReason::FrameSizeError,
                direction: ProtocolDirection::Outbound,
                ..
            }
        )));

        let mut conn = unit_connection();
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        conn.handle_priority(Frame::new(FRAME_PRIORITY, 0, 3, vec![0, 1]), &mut effects)
            .expect("malformed PRIORITY on idle stream is ignored");
        assert!(
            conn.write_queue.is_empty(),
            "idle PRIORITY must not fabricate RST_STREAM"
        );
        assert!(
            collect_facts(&effects).is_empty(),
            "idle PRIORITY must not emit reset facts"
        );
    }

    #[test]
    fn pseudo_header_after_regular_header_is_rejected() {
        let mut block = Vec::new();
        encode_literal_header("x-test", "ok", &mut block);
        encode_literal_header(":method", "GET", &mut block);
        assert!(matches!(
            decode_headers_block(&block, 1024),
            Err(Http2ProtocolError::InvalidPseudoHeaders)
        ));
    }

    #[test]
    fn report_records_late_reply_after_stream_is_gone() {
        let mut conn = unit_connection();
        let _ = conn.handle_service_returned(1, CallOutcome::Replied(HttpResponse::text("late")));
        assert_eq!(conn.report().late_replies_after_close, 1);
    }

    #[test]
    fn response_body_cap_resets_stream_and_reports_stream_full() {
        let mut conn = unit_connection();
        conn.limits.max_response_body_bytes = 2;
        conn.push_stream(ActiveStream::new(
            1,
            HeaderBlock::default(),
            DEFAULT_WINDOW,
            DEFAULT_WINDOW,
            false,
        ));
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        conn.enqueue_response(
            1,
            HttpResponse::with_body(StatusCode::OK, b"abc".to_vec()),
            &mut effects,
        )
        .expect("response cap maps to rst, not connection error");
        assert_eq!(conn.report().stream_full, 1);
        assert!(conn.find_stream(1).is_none());
        assert_eq!(conn.write_queue.len(), 1);
        // The body-cap reset path emits a typed reset fact in the same turn.
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                Effect::Fact(ProtocolFact::Http2StreamReset {
                    reason: Http2ResetReason::EnhanceYourCalm,
                    direction: ProtocolDirection::Outbound,
                    ..
                })
            )),
            "expected an outbound EnhanceYourCalm reset fact",
        );
    }

    #[test]
    fn stop_sends_goaway_for_open_streams() {
        let mut conn = unit_connection();
        conn.highest_client_stream_id = 1;
        let _ = conn.begin_goaway_shutdown();
        assert!(conn.goaway);
        assert_eq!(conn.report().goaway_sent, 1);
        assert!(
            conn.write_in_flight || !conn.pending_write.is_empty() || !conn.write_queue.is_empty()
        );
    }

    #[test]
    fn rst_stream_emits_inbound_reset_and_close_protocol_facts() {
        let mut conn = unit_connection();
        let mut stream = ActiveStream::new(
            5,
            HeaderBlock::default(),
            DEFAULT_WINDOW - 6,
            DEFAULT_WINDOW,
            false,
        );
        stream.request_dispatched_streaming = true;
        stream.request_chunks.push_back(RequestDataChunk {
            data: b"abcdef".to_vec(),
            flow_credit: 6,
        });
        conn.recv_window = DEFAULT_WINDOW - 6;
        conn.push_stream(stream);
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        let rst_frame = Frame::new(FRAME_RST_STREAM, 0, 5, 0x8_u32.to_be_bytes().to_vec());
        conn.handle_rst_stream(rst_frame, &mut effects)
            .expect("handle_rst_stream accepts a well-formed RST");
        conn.flush_deferred_request_window_credit();
        assert_eq!(
            conn.recv_window, DEFAULT_WINDOW,
            "dropped request chunks must restore the connection receive window"
        );
        assert_eq!(
            queued_window_update_increment(&conn, 0),
            6,
            "dropped request chunks must be credited back to the peer"
        );
        // Two protocol facts: a reset (with `Cancel` reason from wire code 8)
        // followed by a clean close. The test pins both presence and order.
        let facts: Vec<&ProtocolFact> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Fact(fact) => Some(fact),
                _ => None,
            })
            .collect();
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::Http2StreamReset {
                    direction: ProtocolDirection::Inbound,
                    reason: Http2ResetReason::Cancel,
                    stream,
                    ..
                } if stream.get() == 5
            )),
            "expected inbound CANCEL reset fact, got {facts:?}"
        );
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::Http2StreamClosed { stream, .. } if stream.get() == 5
            )),
            "expected stream-closed fact, got {facts:?}"
        );
    }

    #[test]
    fn stream_open_queues_protocol_fact_through_effect_fact() {
        // Build a minimal HEADERS frame for a fresh client-initiated stream.
        let mut block = Vec::new();
        encode_literal_header(":method", "GET", &mut block);
        encode_literal_header(":scheme", "http", &mut block);
        encode_literal_header(":path", "/", &mut block);
        encode_literal_header(":authority", "x", &mut block);
        let frame = Frame::new(FRAME_HEADERS, FLAG_END_HEADERS | FLAG_END_STREAM, 1, block);
        let mut conn = unit_connection();
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        conn.handle_headers_frame(frame, &mut effects)
            .expect("handle_headers accepts a fresh stream");
        let opened = effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::Fact(ProtocolFact::Http2StreamOpened {
                    direction: ProtocolDirection::Inbound,
                    stream,
                    ..
                }) if stream.get() == 1
            )
        });
        let summary: Vec<String> = effects
            .iter()
            .map(|effect| match effect {
                Effect::Fact(f) => format!("Fact({f:?})"),
                _ => "<non-fact>".to_string(),
            })
            .collect();
        assert!(
            opened,
            "expected an Http2StreamOpened fact, got {summary:?}"
        );
    }

    #[test]
    fn http2_outcome_vocabulary_covers_six_lifecycle_categories() {
        // Pin the typed outcome vocabulary so future changes that drop a
        // category (peer reset, local cancel, flow-control full, malformed
        // frame, timeout, closed connection) fail this test instead of
        // silently shrinking the public surface.
        let _replied = Http2Outcome::Replied;
        let _full = Http2Outcome::Full;
        let _closed = Http2Outcome::Closed; // closed connection
        let _flow = Http2Outcome::FlowControlBlocked; // flow-control full
        let _timeout = Http2Outcome::Timeout; // timeout
        let _malformed = Http2Outcome::ProtocolError(Http2ProtocolError::BadFrameLength); // malformed frame
        let peer = Http2Outcome::StreamReset(0x8); // peer reset (CANCEL)
        let local = Http2Outcome::LocalCancel(0x8); // local cancel (CANCEL)
        // Peer-initiated and locally-initiated resets are distinguishable
        // even when they carry the same wire error code.
        assert_ne!(peer, local);
    }

    #[test]
    fn http2_protocol_error_vocabulary_distinguishes_malformed_causes() {
        // The typed vocabulary must keep cause classes distinct so the
        // GOAWAY-error-code mapping in handle_read can pick the right wire
        // code. The mapping itself is pinned in
        // goaway_error_code_for_protocol_error_kinds below.
        assert_ne!(
            Http2ProtocolError::FrameTooLarge { len: 1, max: 0 },
            Http2ProtocolError::BadFrameLength
        );
        assert_ne!(
            Http2ProtocolError::FlowControl,
            Http2ProtocolError::WindowOverflow
        );
        assert_ne!(
            Http2ProtocolError::InvalidPseudoHeaders,
            Http2ProtocolError::HpackUnsupported
        );
    }

    #[test]
    fn goaway_error_code_for_protocol_error_kinds() {
        // Exercise the typed-error -> wire-code mapping that handle_read
        // applies. FrameTooLarge / FlowControl / WindowOverflow have
        // dedicated codes; everything else falls into PROTOCOL_ERROR. If
        // this mapping drifts, GOAWAY frames will name the wrong cause to
        // real peers.
        let code_for = |err: Http2ProtocolError| -> u32 {
            match err {
                Http2ProtocolError::FrameTooLarge { .. } => ERR_FRAME_SIZE_ERROR,
                Http2ProtocolError::FlowControl | Http2ProtocolError::WindowOverflow => {
                    ERR_FLOW_CONTROL_ERROR
                }
                _ => ERR_PROTOCOL_ERROR,
            }
        };
        assert_eq!(
            code_for(Http2ProtocolError::FrameTooLarge { len: 4, max: 1 }),
            ERR_FRAME_SIZE_ERROR
        );
        assert_eq!(
            code_for(Http2ProtocolError::FlowControl),
            ERR_FLOW_CONTROL_ERROR
        );
        assert_eq!(
            code_for(Http2ProtocolError::WindowOverflow),
            ERR_FLOW_CONTROL_ERROR
        );
        assert_eq!(
            code_for(Http2ProtocolError::BadFrameLength),
            ERR_PROTOCOL_ERROR
        );
        assert_eq!(
            code_for(Http2ProtocolError::HpackUnsupported),
            ERR_PROTOCOL_ERROR
        );
        assert_eq!(code_for(Http2ProtocolError::BadPreface), ERR_PROTOCOL_ERROR);
    }

    // ------------------------------------------------------------------
    // Phase 112 protocol-fact emission coverage.
    //
    // These tests pin the *emission points*: each named protocol fact
    // must show up in the effects vector at the moment its truth becomes
    // true. They complement the runtime+sim end-to-end tests by proving
    // the http2 path actually feeds Effect::Fact through.

    fn collect_facts_from_effect<'a>(
        effect: &'a Effect<Http2Connection<UnitShard>>,
        out: &mut Vec<&'a ProtocolFact>,
    ) {
        match effect {
            Effect::Fact(fact) => out.push(fact),
            Effect::Batch(items) => {
                for item in items {
                    collect_facts_from_effect(item, out);
                }
            }
            _ => {}
        }
    }

    fn collect_facts(effects: &[Effect<Http2Connection<UnitShard>>]) -> Vec<&ProtocolFact> {
        let mut out = Vec::new();
        for effect in effects {
            collect_facts_from_effect(effect, &mut out);
        }
        out
    }

    fn collect_facts_from_one(effect: &Effect<Http2Connection<UnitShard>>) -> Vec<&ProtocolFact> {
        let mut out = Vec::new();
        collect_facts_from_effect(effect, &mut out);
        out
    }

    #[test]
    fn open_then_clean_send_emits_open_and_close_facts_in_order() {
        // Two-step proof that the open->close lifecycle produces the two
        // matching protocol facts in the correct order across two
        // handler turns.
        let mut conn = unit_connection();
        let mut open_effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        let mut block = Vec::new();
        encode_literal_header(":method", "GET", &mut block);
        encode_literal_header(":scheme", "http", &mut block);
        encode_literal_header(":path", "/", &mut block);
        encode_literal_header(":authority", "x", &mut block);
        conn.handle_headers_frame(
            Frame::new(FRAME_HEADERS, FLAG_END_HEADERS | FLAG_END_STREAM, 1, block),
            &mut open_effects,
        )
        .expect("open accepted");
        let open_facts = collect_facts(&open_effects);
        assert!(
            open_facts
                .iter()
                .any(|f| matches!(f, ProtocolFact::Http2StreamOpened { .. }))
        );

        // Now feed a buffered reply through the same isolate; the
        // send_pending_response path is responsible for the
        // Http2StreamClosed fact.
        let mut close_effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        let response = HttpResponse::text("ok");
        conn.enqueue_response(1, response, &mut close_effects)
            .expect("reply accepted");
        let close_facts = collect_facts(&close_effects);
        assert!(
            close_facts.iter().any(
                |f| matches!(f, ProtocolFact::Http2StreamClosed { stream, .. } if stream.get() == 1)
            ),
            "expected stream-closed fact, got {close_facts:?}",
        );
    }

    #[test]
    fn body_cap_exceeded_emits_high_water_and_reset_facts() {
        // DATA past max_body_bytes triggers the high-water and the
        // outbound RST fact, in that order, on the same handler turn.
        let mut conn = unit_connection();
        conn.limits.max_body_bytes = 4;
        conn.preface_seen = true;
        // Open the stream first.
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        let mut block = Vec::new();
        encode_literal_header(":method", "POST", &mut block);
        encode_literal_header(":scheme", "http", &mut block);
        encode_literal_header(":path", "/upload", &mut block);
        encode_literal_header(":authority", "x", &mut block);
        encode_literal_header("content-length", "8", &mut block);
        conn.handle_headers_frame(
            Frame::new(FRAME_HEADERS, FLAG_END_HEADERS, 1, block),
            &mut effects,
        )
        .expect("open");
        effects.clear();

        // Send 5 bytes of body; cap is 4, so the high-water fact and
        // a reset fact must both show up.
        let data = Frame::new(FRAME_DATA, 0, 1, b"hello".to_vec());
        conn.handle_data_frame(data, &mut effects)
            .expect("data accepted");
        let facts = collect_facts(&effects);
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::HttpBodyHighWater {
                    threshold_bytes: 4,
                    ..
                }
            )),
            "expected HttpBodyHighWater fact, got {facts:?}",
        );
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::Http2StreamReset {
                    reason: Http2ResetReason::EnhanceYourCalm,
                    direction: ProtocolDirection::Outbound,
                    ..
                }
            )),
            "expected EnhanceYourCalm outbound reset fact, got {facts:?}",
        );
        assert_eq!(
            queued_window_update_increment(&conn, 0),
            5,
            "rejected DATA still consumes peer connection credit"
        );
    }

    #[test]
    fn streaming_response_body_cap_emits_high_water_and_reset_facts() {
        let mut conn = unit_connection();
        conn.limits.max_response_body_bytes = 2;
        conn.push_stream(ActiveStream::new(
            1,
            HeaderBlock::default(),
            DEFAULT_WINDOW,
            DEFAULT_WINDOW,
            false,
        ));

        let effect = conn.handle_stream_chunk(
            1,
            CallOutcome::Replied(ResponseChunkReply::Chunk(b"abc".to_vec())),
        );

        assert_eq!(conn.report().stream_full, 1);
        assert!(conn.find_stream(1).is_none());
        let facts = collect_facts_from_one(&effect);
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::HttpBodyHighWater {
                    direction: ProtocolDirection::Outbound,
                    buffered_bytes: 3,
                    threshold_bytes: 2,
                    ..
                }
            )),
            "expected outbound response high-water fact, got {facts:?}",
        );
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::Http2StreamReset {
                    reason: Http2ResetReason::EnhanceYourCalm,
                    direction: ProtocolDirection::Outbound,
                    ..
                }
            )),
            "expected outbound EnhanceYourCalm reset fact, got {facts:?}",
        );

        let mut conn = unit_connection();
        let mut stream = ActiveStream::new(
            3,
            HeaderBlock::default(),
            DEFAULT_WINDOW,
            DEFAULT_WINDOW,
            false,
        );
        stream.response_remaining_content_length = Some(4);
        conn.push_stream(stream);
        let effect = conn.handle_stream_chunk(3, CallOutcome::Replied(ResponseChunkReply::Eof));
        let facts = collect_facts_from_one(&effect);
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::Http2StreamReset {
                    reason: Http2ResetReason::ProtocolError,
                    direction: ProtocolDirection::Outbound,
                    ..
                }
            )),
            "expected short-source protocol reset fact, got {facts:?}",
        );

        let mut conn = unit_connection();
        conn.push_stream(ActiveStream::new(
            5,
            HeaderBlock::default(),
            DEFAULT_WINDOW,
            DEFAULT_WINDOW,
            false,
        ));
        let effect = conn.handle_stream_chunk(5, CallOutcome::Timeout);
        let facts = collect_facts_from_one(&effect);
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::Http2StreamReset {
                    reason: Http2ResetReason::ProtocolError,
                    direction: ProtocolDirection::Outbound,
                    ..
                }
            )),
            "expected pull-failure protocol reset fact, got {facts:?}",
        );
    }

    #[test]
    fn connection_receive_window_full_emits_typed_flow_control_fact() {
        // DATA that exceeds the connection-level receive window emits
        // an `Http2FlowControlFull { side: ConnectionReceive }` fact.
        let mut conn = unit_connection();
        conn.recv_window = 1;
        conn.preface_seen = true;
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        let data = Frame::new(FRAME_DATA, 0, 1, b"toobig".to_vec());
        let err = conn
            .handle_data_frame(data, &mut effects)
            .expect_err("expected FlowControl error");
        assert_eq!(err, Http2ProtocolError::FlowControl);
        let facts = collect_facts(&effects);
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::Http2FlowControlFull {
                    side: Http2FlowControlSide::ConnectionReceive,
                    ..
                }
            )),
            "expected ConnectionReceive flow-control fact, got {facts:?}",
        );
    }

    #[test]
    fn stream_receive_window_full_resets_only_that_stream() {
        let mut conn = unit_connection();
        conn.recv_window = 100;
        conn.push_stream(ActiveStream::new(
            1,
            HeaderBlock::default(),
            1,
            DEFAULT_WINDOW,
            false,
        ));
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        let data = Frame::new(FRAME_DATA, 0, 1, b"abc".to_vec());

        conn.handle_data_frame(data, &mut effects)
            .expect("stream receive overrun should not GOAWAY connection");

        assert!(conn.find_stream(1).is_none(), "bad stream is removed");
        let queued: Vec<Frame> = conn
            .write_queue
            .iter()
            .filter_map(|bytes| {
                try_decode_frame(bytes, conn.limits.max_frame_size)
                    .expect("complete queued frame")
                    .map(|(frame, _)| frame)
            })
            .collect();
        let queued = queued
            .iter()
            .find(|frame| frame.ty == FRAME_RST_STREAM)
            .expect("queued RST decodes");
        assert_eq!(queued.ty, FRAME_RST_STREAM);
        assert_eq!(queued.stream_id, 1);
        let mut code = [0_u8; 4];
        code.copy_from_slice(&queued.payload);
        assert_eq!(u32::from_be_bytes(code), ERR_FLOW_CONTROL_ERROR);
        assert_eq!(
            queued_window_update_increment(&conn, 0),
            3,
            "stream-level rejects still return connection credit"
        );
        let facts = collect_facts(&effects);
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::Http2FlowControlFull {
                    side: Http2FlowControlSide::StreamReceive,
                    ..
                }
            )),
            "expected StreamReceive flow-control fact, got {facts:?}",
        );
    }

    #[test]
    fn rst_with_unknown_wire_code_uses_other_code_variant() {
        // Unknown wire codes round-trip into Http2ResetReason::OtherCode
        // so replay can pin a precise error code rather than silently
        // collapsing into a generic catch-all.
        let mut conn = unit_connection();
        conn.push_stream(ActiveStream::new(
            3,
            HeaderBlock::default(),
            DEFAULT_WINDOW,
            DEFAULT_WINDOW,
            false,
        ));
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        let frame = Frame::new(FRAME_RST_STREAM, 0, 3, 0xff_u32.to_be_bytes().to_vec());
        conn.handle_rst_stream(frame, &mut effects)
            .expect("handle_rst_stream accepts");
        let facts = collect_facts(&effects);
        assert!(
            facts.iter().any(|f| matches!(
                f,
                ProtocolFact::Http2StreamReset {
                    reason: Http2ResetReason::OtherCode(0xff),
                    ..
                }
            )),
            "expected OtherCode(0xff) reset fact, got {facts:?}",
        );
    }

    #[test]
    fn multiple_facts_in_one_turn_are_preserved_in_order() {
        // Two facts in the same effects vec must appear in arrival
        // order: open via headers, then high-water via an oversized
        // body that exceeds max_body_bytes. The body-cap path also
        // emits a paired EnhanceYourCalm reset fact.
        let mut conn = unit_connection();
        conn.limits.max_body_bytes = 2;
        conn.preface_seen = true;
        let mut effects: Vec<Effect<Http2Connection<UnitShard>>> = Vec::new();
        let mut block = Vec::new();
        encode_literal_header(":method", "POST", &mut block);
        encode_literal_header(":scheme", "http", &mut block);
        encode_literal_header(":path", "/upload", &mut block);
        encode_literal_header(":authority", "x", &mut block);
        conn.handle_headers_frame(
            Frame::new(FRAME_HEADERS, FLAG_END_HEADERS, 1, block),
            &mut effects,
        )
        .expect("open");
        // 3-byte body exceeds the 2-byte cap.
        let oversize = Frame::new(FRAME_DATA, 0, 1, b"abc".to_vec());
        conn.handle_data_frame(oversize, &mut effects)
            .expect("oversize body handled without bubbling error");
        let facts = collect_facts(&effects);
        let mut iter = facts.into_iter();
        assert!(matches!(
            iter.next(),
            Some(ProtocolFact::Http2StreamOpened { .. })
        ));
        assert!(matches!(
            iter.next(),
            Some(ProtocolFact::HttpBodyHighWater { .. })
        ));
        assert!(matches!(
            iter.next(),
            Some(ProtocolFact::Http2StreamReset {
                reason: Http2ResetReason::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn grpc_status_arm_emits_grpc_status_then_close() {
        // The handle_stream_chunk GrpcStatus arm must emit both the gRPC
        // final-status fact and the matching stream-close fact. These
        // are the live-only facts that a future native gRPC client
        // isolate will mirror for the received-status side.
        let mut conn = unit_connection();
        conn.self_isolate_id = Some(IsolateId::new(77));
        conn.push_stream(ActiveStream::new(
            5,
            HeaderBlock::default(),
            DEFAULT_WINDOW,
            DEFAULT_WINDOW,
            true,
        ));
        let effect = conn.handle_stream_chunk(
            5,
            CallOutcome::Replied(ResponseChunkReply::GrpcStatus(
                crate::grpc::GrpcStatus::new(crate::grpc::GrpcStatusCode::Unauthenticated),
            )),
        );
        let facts = collect_facts_from_one(&effect);
        assert!(facts.iter().any(|f| matches!(
            f,
            ProtocolFact::GrpcFinalStatusSent {
                connection,
                status: tina_runtime::GrpcStatusCode::Unauthenticated,
                ..
            } if connection.get() == 77
        )));
        assert!(
            facts
                .iter()
                .any(|f| matches!(f, ProtocolFact::Http2StreamClosed { .. }))
        );
    }
}
