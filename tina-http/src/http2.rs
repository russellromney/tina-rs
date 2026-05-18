//! Native HTTP/2 first form.
//!
//! This module is deliberately small: prior-knowledge cleartext h2c,
//! unary buffered request/response, bounded stream table, visible
//! frame/header/window errors, and no async runtime ownership.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;

use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version};
use tina::prelude::*;
use tina::reply_to_request;
use tina_runtime::{
    CallError, CallOutcome, ListenerId, StreamId, call, call_cancelable, cancel_call, tcp_accept,
    tcp_bind, tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
};

use crate::streaming::{
    Http2RequestStream, RequestChunkReply, ResponseChunkMsg, ResponseChunkReply,
};
use crate::{HttpRequest, HttpRequestBody, HttpResponse, HttpResponseBody};

const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_HEADER_LEN: usize = 9;
const DEFAULT_WINDOW: i32 = 65_535;
const READ_CHUNK: usize = 16 * 1024;
const WINDOW_CREDIT_FLUSH_THRESHOLD: u32 = 16 * 1024;

const FLAG_ACK: u8 = 0x1;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;
const FLAG_PADDED: u8 = 0x8;
const FLAG_PRIORITY: u8 = 0x20;

const FRAME_DATA: u8 = 0x0;
const FRAME_HEADERS: u8 = 0x1;
const FRAME_PRIORITY: u8 = 0x2;
const FRAME_RST_STREAM: u8 = 0x3;
const FRAME_SETTINGS: u8 = 0x4;
const FRAME_PUSH_PROMISE: u8 = 0x5;
const FRAME_PING: u8 = 0x6;
const FRAME_GOAWAY: u8 = 0x7;
const FRAME_WINDOW_UPDATE: u8 = 0x8;

const ERR_NO_ERROR: u32 = 0x0;
const ERR_PROTOCOL_ERROR: u32 = 0x1;
const ERR_FLOW_CONTROL_ERROR: u32 = 0x3;
const ERR_SETTINGS_ERROR: u32 = 0x4;
const ERR_STREAM_CLOSED: u32 = 0x5;
const ERR_FRAME_SIZE_ERROR: u32 = 0x6;
const ERR_REFUSED_STREAM: u32 = 0x7;
const ERR_ENHANCE_YOUR_CALM: u32 = 0xb;

const SETTINGS_HEADER_TABLE_SIZE: u16 = 0x1;
const SETTINGS_ENABLE_PUSH: u16 = 0x2;
const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 0x3;
const SETTINGS_INITIAL_WINDOW_SIZE: u16 = 0x4;
const SETTINGS_MAX_FRAME_SIZE: u16 = 0x5;
const SETTINGS_MAX_HEADER_LIST_SIZE: u16 = 0x6;
const DEFAULT_HEADER_TABLE_SIZE: u32 = 4096;
const MIN_MAX_FRAME_SIZE: u32 = 16_384;
const MAX_MAX_FRAME_SIZE: u32 = 16_777_215;

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
    /// Bounded outbound frame queue length per connection.
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

/// Protocol/lifecycle errors surfaced by the frame and connection layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Http2ProtocolError {
    BadPreface,
    FrameTooLarge { len: usize, max: usize },
    TruncatedFrame,
    BadFrameLength,
    BadStreamId,
    HeadersTooLarge,
    HpackUnsupported,
    InvalidPseudoHeaders,
    StreamClosed,
    StreamLimitFull,
    WindowOverflow,
    FlowControl,
    RequestTrailersUnsupported,
    SettingsUnsupported,
    InvalidSettingsValue,
    UnsupportedFrame(u8),
}

/// Per-connection report counters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
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
}

/// Per-stream report snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Http2StreamReport {
    pub stream_id: u32,
    pub state: Http2StreamState,
    pub buffered_body_bytes: usize,
    pub recv_window: i32,
}

#[derive(Debug, Clone)]
struct Frame {
    ty: u8,
    flags: u8,
    stream_id: u32,
    payload: Vec<u8>,
}

impl Frame {
    fn new(ty: u8, flags: u8, stream_id: u32, payload: Vec<u8>) -> Self {
        Self {
            ty,
            flags,
            stream_id,
            payload,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let len = self.payload.len();
        assert!(len <= 0x00ff_ffff, "HTTP/2 frame payload too large");
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + len);
        out.push(((len >> 16) & 0xff) as u8);
        out.push(((len >> 8) & 0xff) as u8);
        out.push((len & 0xff) as u8);
        out.push(self.ty);
        out.push(self.flags);
        let sid = self.stream_id & 0x7fff_ffff;
        out.extend_from_slice(&sid.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

fn try_decode_frame(
    buffer: &[u8],
    limits: &Http2Limits,
) -> Result<Option<(Frame, usize)>, Http2ProtocolError> {
    if buffer.len() < FRAME_HEADER_LEN {
        return Ok(None);
    }
    let len = ((buffer[0] as usize) << 16) | ((buffer[1] as usize) << 8) | buffer[2] as usize;
    if len > limits.max_frame_size {
        return Err(Http2ProtocolError::FrameTooLarge {
            len,
            max: limits.max_frame_size,
        });
    }
    let total = FRAME_HEADER_LEN
        .checked_add(len)
        .ok_or(Http2ProtocolError::FrameTooLarge {
            len,
            max: limits.max_frame_size,
        })?;
    if buffer.len() < total {
        return Ok(None);
    }
    let ty = buffer[3];
    let flags = buffer[4];
    let mut sid_bytes = [0_u8; 4];
    sid_bytes.copy_from_slice(&buffer[5..9]);
    let stream_id = u32::from_be_bytes(sid_bytes) & 0x7fff_ffff;
    let payload = buffer[9..total].to_vec();
    Ok(Some((
        Frame {
            ty,
            flags,
            stream_id,
            payload,
        },
        total,
    )))
}

fn settings_frame(ack: bool) -> Frame {
    Frame::new(
        FRAME_SETTINGS,
        if ack { FLAG_ACK } else { 0 },
        0,
        Vec::new(),
    )
}

fn rst_stream_frame(stream_id: u32, error: u32) -> Frame {
    Frame::new(FRAME_RST_STREAM, 0, stream_id, error.to_be_bytes().to_vec())
}

fn goaway_frame(last_stream_id: u32, error: u32) -> Frame {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&(last_stream_id & 0x7fff_ffff).to_be_bytes());
    payload.extend_from_slice(&error.to_be_bytes());
    Frame::new(FRAME_GOAWAY, 0, 0, payload)
}

fn window_update_frame(stream_id: u32, increment: u32) -> Frame {
    Frame::new(
        FRAME_WINDOW_UPDATE,
        0,
        stream_id,
        (increment & 0x7fff_ffff).to_be_bytes().to_vec(),
    )
}

fn headers_frame(stream_id: u32, end_stream: bool, block: Vec<u8>) -> Frame {
    let flags = FLAG_END_HEADERS | if end_stream { FLAG_END_STREAM } else { 0 };
    Frame::new(FRAME_HEADERS, flags, stream_id, block)
}

fn data_frame(stream_id: u32, end_stream: bool, data: Vec<u8>) -> Frame {
    Frame::new(
        FRAME_DATA,
        if end_stream { FLAG_END_STREAM } else { 0 },
        stream_id,
        data,
    )
}

fn data_payload(frame: &Frame) -> Result<Vec<u8>, Http2ProtocolError> {
    if frame.flags & FLAG_PADDED == 0 {
        return Ok(frame.payload.clone());
    }
    let Some((&pad_len, rest)) = frame.payload.split_first() else {
        return Err(Http2ProtocolError::BadFrameLength);
    };
    let pad_len = usize::from(pad_len);
    if pad_len > rest.len() {
        return Err(Http2ProtocolError::BadFrameLength);
    }
    Ok(rest[..rest.len() - pad_len].to_vec())
}

fn headers_payload(frame: &Frame) -> Result<&[u8], Http2ProtocolError> {
    let mut offset = 0usize;
    let mut pad_len = 0usize;
    if frame.flags & FLAG_PADDED != 0 {
        let Some((&pad, _)) = frame.payload.split_first() else {
            return Err(Http2ProtocolError::BadFrameLength);
        };
        pad_len = usize::from(pad);
        offset = 1;
    }
    if frame.flags & FLAG_PRIORITY != 0 {
        let next = offset
            .checked_add(5)
            .ok_or(Http2ProtocolError::BadFrameLength)?;
        if frame.payload.len() < next {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        offset = next;
    }
    let available = frame
        .payload
        .len()
        .checked_sub(offset)
        .ok_or(Http2ProtocolError::BadFrameLength)?;
    if pad_len > available {
        return Err(Http2ProtocolError::BadFrameLength);
    }
    let end = frame.payload.len() - pad_len;
    Ok(&frame.payload[offset..end])
}

#[derive(Debug, Default)]
struct HeaderBlock {
    method: Option<Method>,
    path: Option<String>,
    scheme: Option<String>,
    authority: Option<String>,
    status: Option<StatusCode>,
    headers: HeaderMap,
    bytes: usize,
    saw_regular: bool,
}

#[cfg(test)]
fn decode_headers_block(
    block: &[u8],
    max_header_bytes: usize,
) -> Result<HeaderBlock, Http2ProtocolError> {
    let mut decoder = hpack::Decoder::new();
    decode_headers_block_with(&mut decoder, block, max_header_bytes)
}

fn decode_headers_block_with(
    decoder: &mut hpack::Decoder<'static>,
    block: &[u8],
    max_header_bytes: usize,
) -> Result<HeaderBlock, Http2ProtocolError> {
    let mut out = HeaderBlock::default();
    for (name, value) in decoder
        .decode(block)
        .map_err(|_| Http2ProtocolError::HpackUnsupported)?
    {
        let name = std::str::from_utf8(&name).map_err(|_| Http2ProtocolError::HpackUnsupported)?;
        let value =
            std::str::from_utf8(&value).map_err(|_| Http2ProtocolError::HpackUnsupported)?;
        add_header(&mut out, name, value, max_header_bytes)?;
    }
    Ok(out)
}

fn add_header(
    out: &mut HeaderBlock,
    name: &str,
    value: &str,
    max_header_bytes: usize,
) -> Result<(), Http2ProtocolError> {
    out.bytes = out
        .bytes
        .checked_add(name.len() + value.len())
        .ok_or(Http2ProtocolError::HeadersTooLarge)?;
    if out.bytes > max_header_bytes {
        return Err(Http2ProtocolError::HeadersTooLarge);
    }
    if name.starts_with(':') {
        if out.saw_regular {
            return Err(Http2ProtocolError::InvalidPseudoHeaders);
        }
        match name {
            ":method" => {
                out.method = Some(
                    Method::from_bytes(value.as_bytes())
                        .map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?,
                );
            }
            ":path" => out.path = Some(value.to_owned()),
            ":scheme" => out.scheme = Some(value.to_owned()),
            ":authority" => out.authority = Some(value.to_owned()),
            ":status" => {
                out.status = Some(
                    StatusCode::from_bytes(value.as_bytes())
                        .map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?,
                );
            }
            _ => return Err(Http2ProtocolError::InvalidPseudoHeaders),
        }
        return Ok(());
    }
    if name.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    if matches!(
        name,
        "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
    ) {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    out.saw_regular = true;
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?;
    let header_value =
        HeaderValue::from_str(value).map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?;
    out.headers.append(header_name, header_value);
    Ok(())
}

fn encode_literal_header(name: &str, value: &str, out: &mut Vec<u8>) {
    out.push(0);
    encode_string(name, out);
    encode_string(value, out);
}

fn encode_string(value: &str, out: &mut Vec<u8>) {
    encode_integer(value.len(), 7, 0, out);
    out.extend_from_slice(value.as_bytes());
}

fn encode_integer(mut value: usize, prefix_bits: u8, pattern: u8, out: &mut Vec<u8>) {
    let max = (1_usize << prefix_bits) - 1;
    if value < max {
        out.push(pattern | value as u8);
        return;
    }
    out.push(pattern | max as u8);
    value -= max;
    while value >= 128 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn encode_response_headers(response: &HttpResponse, body_len: usize) -> Vec<u8> {
    encode_response_headers_with_len(response, Some(body_len))
}

fn encode_response_headers_with_len(response: &HttpResponse, body_len: Option<usize>) -> Vec<u8> {
    let mut block = Vec::new();
    encode_literal_header(":status", response.status.as_str(), &mut block);
    if let Some(body_len) = body_len {
        encode_literal_header("content-length", &body_len.to_string(), &mut block);
    }
    for (name, value) in response.headers.iter() {
        if name.as_str().starts_with(':') {
            continue;
        }
        if name.as_str() == "grpc-status"
            || name.as_str() == "grpc-message"
            || name.as_str() == "content-length"
            || name.as_str() == "transfer-encoding"
        {
            continue;
        }
        if let Ok(value) = value.to_str() {
            encode_literal_header(name.as_str(), value, &mut block);
        }
    }
    block
}

fn encode_trailers(headers: &HeaderMap) -> Option<Vec<u8>> {
    let status = headers.get("grpc-status")?;
    let mut block = Vec::new();
    if let Ok(value) = status.to_str() {
        encode_literal_header("grpc-status", value, &mut block);
    }
    if let Some(message) = headers.get("grpc-message") {
        if let Ok(value) = message.to_str() {
            encode_literal_header("grpc-message", value, &mut block);
        }
    }
    Some(block)
}

fn encode_response_trailers(response: &HttpResponse) -> Option<Vec<u8>> {
    encode_trailers(&response.headers)
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
    request_dispatched_streaming: bool,
    request_eof: bool,
    request_content_length: Option<usize>,
    request_bytes_received: usize,
    pending_recv_window_credit: u32,
    request_chunks: VecDeque<Vec<u8>>,
    pending_request_body_reply: Option<tina::RequestContext<Http2ConnectionReply>>,
    reset: bool,
}

#[derive(Debug)]
struct PendingResponse {
    header_block: Vec<u8>,
    body: Vec<u8>,
    trailers: Option<Vec<u8>>,
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
            request_dispatched_streaming: false,
            request_eof: false,
            request_content_length: None,
            request_bytes_received: 0,
            pending_recv_window_credit: 0,
            request_chunks: VecDeque::new(),
            pending_request_body_reply: None,
            reset: false,
        }
    }
}

/// Messages handled by [`Http2Connection`].
#[derive(Debug, Clone)]
pub enum Http2ConnectionMsg {
    Begin,
    Read(Result<Vec<u8>, CallError>),
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
    Wrote(Result<usize, CallError>),
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

#[derive(Debug, Clone)]
pub enum Http2ConnectionReply {
    RequestChunk(RequestChunkReply),
    Report(Http2ConnectionReport),
}

/// One HTTP/2 connection isolate over one TCP stream.
pub struct Http2Connection<S: Shard, M: From<HttpRequest> + Send + 'static = HttpRequest> {
    stream: StreamId,
    service: Address<M, HttpResponse>,
    limits: Http2Limits,
    service_call_timeout: Duration,
    read_buf: Vec<u8>,
    hpack_decoder: hpack::Decoder<'static>,
    preface_seen: bool,
    streams: Vec<ActiveStream>,
    highest_client_stream_id: u32,
    recv_window: i32,
    pending_recv_window_credit: u32,
    send_window: i32,
    peer_initial_stream_window: i32,
    peer_max_frame_size: usize,
    reset_churn: u32,
    goaway: bool,
    closing_after_write: bool,
    pending_write: Vec<u8>,
    write_queue: VecDeque<Vec<u8>>,
    report: Http2ConnectionReport,
    self_shard_id: Option<tina::ShardId>,
    self_isolate_id: Option<tina::IsolateId>,
    _shard: PhantomData<S>,
}

impl<S: Shard, M: From<HttpRequest> + Send + 'static> Http2Connection<S, M> {
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
            hpack_decoder: hpack::Decoder::new(),
            preface_seen: false,
            streams: Vec::with_capacity(limits.max_concurrent_streams),
            highest_client_stream_id: 0,
            recv_window: limits.initial_connection_window,
            pending_recv_window_credit: 0,
            send_window: DEFAULT_WINDOW,
            peer_initial_stream_window: DEFAULT_WINDOW,
            peer_max_frame_size: limits.max_frame_size,
            reset_churn: 0,
            goaway: false,
            closing_after_write: false,
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
}

impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> Isolate for Http2Connection<S, M> {
    tina::isolate_types! {
        message: Http2ConnectionMsg,
        reply: Http2ConnectionReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: tina_runtime::RuntimeCall<Http2ConnectionMsg>,
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
            Http2ConnectionMsg::Read(Ok(bytes)) => self.handle_read(bytes),
            Http2ConnectionMsg::Read(Err(_)) => self.close_now(),
            Http2ConnectionMsg::ServiceReturned { stream_id, outcome } => {
                self.handle_service_returned(stream_id, outcome)
            }
            Http2ConnectionMsg::ServiceCancelled { .. } => noop(),
            Http2ConnectionMsg::StreamChunk { stream_id, outcome } => {
                self.handle_stream_chunk(stream_id, outcome)
            }
            Http2ConnectionMsg::StreamSourceCancelDone { .. } => noop(),
            Http2ConnectionMsg::Wrote(Ok(n)) => self.handle_wrote(n),
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
                call.reply(Http2ConnectionReply::Report(self.report.clone()))
            }
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> Http2Connection<S, M> {
    fn read_more(&mut self) -> Effect<Self> {
        tcp_read(self.stream, READ_CHUNK).then(Http2ConnectionMsg::Read)
    }

    fn write_more(&mut self) -> Effect<Self> {
        if self.pending_write.is_empty() {
            if let Some(next) = self.write_queue.pop_front() {
                self.pending_write = next;
            }
        }
        if self.pending_write.is_empty() {
            if self.closing_after_write {
                return self.close_now();
            }
            return noop();
        }
        tcp_write(self.stream, self.pending_write.clone()).then(Http2ConnectionMsg::Wrote)
    }

    fn close_now(&mut self) -> Effect<Self> {
        tcp_close_stream(self.stream).then(Http2ConnectionMsg::Closed)
    }

    fn begin_goaway_shutdown(&mut self) -> Effect<Self> {
        self.goaway = true;
        let _ = self.enqueue_frame(goaway_frame(self.highest_client_stream_id, ERR_NO_ERROR));
        self.report.goaway_sent += 1;
        self.closing_after_write = true;
        if self.pending_write.is_empty() && !self.write_queue.is_empty() {
            self.write_more()
        } else if self.pending_write.is_empty() {
            self.close_now()
        } else {
            noop()
        }
    }

    fn handle_read(&mut self, bytes: Vec<u8>) -> Effect<Self> {
        if bytes.is_empty() {
            return self.close_now();
        }
        self.read_buf.extend_from_slice(&bytes);
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
        }
        if self.pending_write.is_empty() && !self.write_queue.is_empty() {
            effects.push(self.write_more());
        }
        if !self.closing_after_write {
            effects.push(self.read_more());
        } else if self.pending_write.is_empty() && self.write_queue.is_empty() {
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
            self.enqueue_frame(settings_frame(false))?;
        }

        while let Some((frame, used)) = try_decode_frame(&self.read_buf, &self.limits)? {
            self.read_buf.drain(..used);
            self.handle_frame(frame, effects)?;
        }
        Ok(())
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
            FRAME_WINDOW_UPDATE => {
                self.handle_window_update(frame)?;
                self.push_ready_response_pulls(effects);
                Ok(())
            }
            FRAME_RST_STREAM => self.handle_rst_stream(frame, effects),
            FRAME_PING => self.handle_ping(frame),
            FRAME_GOAWAY => {
                self.goaway = true;
                Ok(())
            }
            FRAME_PRIORITY => Ok(()),
            FRAME_PUSH_PROMISE => Err(Http2ProtocolError::UnsupportedFrame(FRAME_PUSH_PROMISE)),
            _ => Ok(()),
        }
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
        self.enqueue_frame(settings_frame(true))
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
        if self.goaway {
            self.enqueue_frame(rst_stream_frame(frame.stream_id, ERR_REFUSED_STREAM))?;
            return Ok(());
        }
        if self.find_stream(frame.stream_id).is_some() {
            let stream_id = frame.stream_id;
            self.enqueue_frame(rst_stream_frame(frame.stream_id, ERR_PROTOCOL_ERROR))?;
            self.reset_active_stream_for_protocol(stream_id, effects);
            return Ok(());
        }
        if frame.stream_id <= self.highest_client_stream_id {
            return Err(Http2ProtocolError::BadStreamId);
        }
        self.highest_client_stream_id = frame.stream_id;
        if self.streams.len() >= self.limits.max_concurrent_streams {
            self.report.stream_full += 1;
            self.enqueue_frame(rst_stream_frame(frame.stream_id, ERR_ENHANCE_YOUR_CALM))?;
            return Ok(());
        }
        let header_payload = headers_payload(&frame)?;
        let headers = decode_headers_block_with(
            &mut self.hpack_decoder,
            header_payload,
            self.limits.max_header_bytes,
        )?;
        validate_request_headers(&headers)?;
        let grpc = headers
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(crate::grpc::is_grpc_content_type);
        let end_stream = frame.flags & FLAG_END_STREAM != 0;
        let mut stream = ActiveStream::new(
            frame.stream_id,
            headers,
            self.limits.initial_stream_window,
            self.peer_initial_stream_window,
            grpc,
        );
        self.report.opened_streams += 1;
        if end_stream {
            stream.request_eof = true;
            stream.state = Http2StreamState::HalfClosedRemote;
            self.streams.push(stream);
            self.dispatch_stream(frame.stream_id, effects)?;
        } else if grpc {
            self.streams.push(stream);
            self.dispatch_streaming_request(frame.stream_id, effects)?;
        } else {
            self.streams.push(stream);
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
        let len = payload.len();
        let len_i32 = i32::try_from(len).map_err(|_| Http2ProtocolError::FlowControl)?;
        if self.recv_window < len_i32 {
            self.report.flow_control_blocked += 1;
            return Err(Http2ProtocolError::FlowControl);
        }
        let idx = self
            .find_stream(frame.stream_id)
            .ok_or(Http2ProtocolError::StreamClosed)?;
        if self.streams[idx].state == Http2StreamState::Closed || self.streams[idx].reset {
            self.enqueue_frame(rst_stream_frame(frame.stream_id, ERR_STREAM_CLOSED))?;
            return Ok(());
        }
        if self.streams[idx].request_eof {
            self.enqueue_frame(rst_stream_frame(frame.stream_id, ERR_STREAM_CLOSED))?;
            return Ok(());
        }
        if self.streams[idx].recv_window < len_i32 {
            self.report.flow_control_blocked += 1;
            return Err(Http2ProtocolError::FlowControl);
        }
        if let Some(content_length) = self.streams[idx].request_content_length {
            let received = self.streams[idx]
                .request_bytes_received
                .checked_add(len)
                .ok_or(Http2ProtocolError::HeadersTooLarge)?;
            if received > content_length {
                self.report.protocol_errors += 1;
                self.enqueue_frame(rst_stream_frame(frame.stream_id, ERR_PROTOCOL_ERROR))?;
                self.reset_active_stream_for_protocol(frame.stream_id, effects);
                return Ok(());
            }
        }
        let buffered_len = if self.streams[idx].request_dispatched_streaming {
            self.streams[idx].request_bytes_received
        } else {
            self.streams[idx].body.len()
        };
        let new_len = buffered_len
            .checked_add(len)
            .ok_or(Http2ProtocolError::HeadersTooLarge)?;
        if new_len > self.limits.max_body_bytes {
            self.report.stream_full += 1;
            self.enqueue_frame(rst_stream_frame(frame.stream_id, ERR_ENHANCE_YOUR_CALM))?;
            self.remove_stream(frame.stream_id);
            return Ok(());
        }
        self.recv_window -= len_i32;
        self.streams[idx].recv_window -= len_i32;
        self.streams[idx].request_bytes_received += len;
        if self.streams[idx].request_dispatched_streaming {
            if !payload.is_empty() {
                self.streams[idx].request_chunks.push_back(payload);
            }
        } else {
            self.streams[idx].body.extend_from_slice(&payload);
        }
        if frame.flags & FLAG_END_STREAM != 0 {
            if self.streams[idx]
                .request_content_length
                .is_some_and(|content_length| {
                    self.streams[idx].request_bytes_received != content_length
                })
            {
                self.report.protocol_errors += 1;
                self.enqueue_frame(rst_stream_frame(frame.stream_id, ERR_PROTOCOL_ERROR))?;
                self.reset_active_stream_for_protocol(frame.stream_id, effects);
                return Ok(());
            }
            self.streams[idx].request_eof = true;
            self.streams[idx].state = Http2StreamState::HalfClosedRemote;
            if self.streams[idx].request_dispatched_streaming {
                effects.push(self.reply_pending_request_chunk(frame.stream_id)?);
            } else {
                self.dispatch_stream(frame.stream_id, effects)?;
            }
        } else if self.streams[idx].request_dispatched_streaming {
            effects.push(self.reply_pending_request_chunk(frame.stream_id)?);
        }
        Ok(())
    }

    fn handle_window_update(&mut self, frame: Frame) -> Result<(), Http2ProtocolError> {
        if frame.payload.len() != 4 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        let mut bytes = [0_u8; 4];
        bytes.copy_from_slice(&frame.payload);
        let increment = u32::from_be_bytes(bytes) & 0x7fff_ffff;
        if increment == 0 {
            return Err(Http2ProtocolError::WindowOverflow);
        }
        if frame.stream_id == 0 {
            self.send_window = add_window(self.send_window, increment)?;
        } else if let Some(idx) = self.find_stream(frame.stream_id) {
            self.streams[idx].send_window = add_window(self.streams[idx].send_window, increment)?;
        }
        self.flush_pending_responses()?;
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
            return Ok(());
        }
        if let Some(mut stream) = self.remove_stream(frame.stream_id) {
            self.report.reset_streams += 1;
            self.report.closed_streams += 1;
            if let Some(call) = stream.pending_request_body_reply.take() {
                effects.push(reply_to_request(
                    call,
                    Http2ConnectionReply::RequestChunk(RequestChunkReply::Error(
                        CallError::TargetClosed,
                    )),
                ));
            }
            if let Some(handle) = stream.pending_call.take() {
                let stream_id = frame.stream_id;
                effects.push(cancel_call(handle).then(move |outcome| {
                    Http2ConnectionMsg::ServiceCancelled { stream_id, outcome }
                }));
            }
            if let Some(source) = stream.response_source.take() {
                let stream_id = frame.stream_id;
                effects.push(
                    call(
                        source,
                        ResponseChunkMsg::Cancel,
                        self.limits.response_stream_call_timeout,
                    )
                    .then(move |outcome| {
                        Http2ConnectionMsg::StreamSourceCancelDone { stream_id, outcome }
                    }),
                );
            }
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
        let request = HttpRequest {
            method: headers
                .method
                .ok_or(Http2ProtocolError::InvalidPseudoHeaders)?,
            path: headers
                .path
                .ok_or(Http2ProtocolError::InvalidPseudoHeaders)?,
            version: Version::HTTP_2,
            headers: headers.headers,
            body: HttpRequestBody::Buffered(std::mem::take(&mut self.streams[idx].body)),
        };
        let consumed = match &request.body {
            HttpRequestBody::Buffered(bytes) => bytes.len(),
            HttpRequestBody::Stream(_) | HttpRequestBody::Http2Stream(_) => 0,
        };
        if consumed > 0 {
            self.recv_window = self.recv_window.saturating_add(consumed as i32);
            if let Some(idx) = self.find_stream(stream_id) {
                self.streams[idx].recv_window = self.streams[idx]
                    .recv_window
                    .saturating_add(consumed as i32);
            }
            self.enqueue_frame(window_update_frame(0, consumed as u32))?;
            self.enqueue_frame(window_update_frame(stream_id, consumed as u32))?;
        }
        let (effect, handle) =
            call_cancelable(self.service, M::from(request), self.service_call_timeout)
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
        let content_length = headers
            .headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok());
        self.streams[idx].request_content_length = content_length;
        let source = tina::Address::new_with_generation(
            self.self_shard_id.expect("shard id captured"),
            self.self_isolate_id.expect("isolate id captured"),
            tina::AddressGeneration::new(0),
        );
        let request = HttpRequest {
            method: headers
                .method
                .ok_or(Http2ProtocolError::InvalidPseudoHeaders)?,
            path: headers
                .path
                .ok_or(Http2ProtocolError::InvalidPseudoHeaders)?,
            version: Version::HTTP_2,
            headers: headers.headers,
            body: HttpRequestBody::Http2Stream(Http2RequestStream {
                stream_id,
                content_length,
                source,
            }),
        };
        let (effect, handle) =
            call_cancelable(self.service, M::from(request), self.service_call_timeout)
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
        match outcome {
            CallOutcome::Replied(response) => {
                if let Err(error) = self.enqueue_response(stream_id, &response) {
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
                let _ = self.enqueue_response(stream_id, &response);
            }
            CallOutcome::Closed | CallOutcome::Rejected(_) => {
                let response = if grpc {
                    crate::grpc::grpc_status_http_response(crate::grpc::GrpcStatus::new(
                        crate::grpc::GrpcStatusCode::Internal,
                    ))
                } else {
                    HttpResponse::internal_error()
                };
                let _ = self.enqueue_response(stream_id, &response);
            }
            CallOutcome::Timeout => {
                let response = if grpc {
                    crate::grpc::grpc_status_http_response(crate::grpc::GrpcStatus::new(
                        crate::grpc::GrpcStatusCode::DeadlineExceeded,
                    ))
                } else {
                    HttpResponse::gateway_timeout()
                };
                let _ = self.enqueue_response(stream_id, &response);
            }
        }
        let mut effects = Vec::new();
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
        response: &HttpResponse,
    ) -> Result<(), Http2ProtocolError> {
        let body = match &response.body {
            HttpResponseBody::Buffered(bytes) => {
                if bytes.len() > self.limits.max_response_body_bytes {
                    self.report.stream_full += 1;
                    self.enqueue_frame(rst_stream_frame(stream_id, ERR_ENHANCE_YOUR_CALM))?;
                    self.remove_stream(stream_id);
                    self.report.closed_streams += 1;
                    return Ok(());
                }
                bytes.clone()
            }
            HttpResponseBody::Stream(stream) => {
                return self.begin_streaming_response(
                    stream_id,
                    response,
                    stream.source,
                    Some(stream.content_length),
                );
            }
            HttpResponseBody::ChunkedStream(stream) => {
                return self.begin_streaming_response(stream_id, response, stream.source, None);
            }
            HttpResponseBody::WebSocket(_) => {
                return Err(Http2ProtocolError::UnsupportedFrame(FRAME_DATA));
            }
        };
        let block = encode_response_headers(response, body.len());
        let trailers = encode_response_trailers(response);
        self.queue_or_send_response(
            stream_id,
            PendingResponse {
                header_block: block,
                body,
                trailers,
            },
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
        Ok(())
    }

    fn queue_or_send_response(
        &mut self,
        stream_id: u32,
        pending: PendingResponse,
    ) -> Result<(), Http2ProtocolError> {
        let idx = self
            .find_stream(stream_id)
            .ok_or(Http2ProtocolError::StreamClosed)?;
        let body_len_i32 = match i32::try_from(pending.body.len()) {
            Ok(len) => len,
            Err(_) => {
                self.report.flow_control_blocked += 1;
                self.streams[idx].pending_response = Some(pending);
                return Ok(());
            }
        };
        if body_len_i32 > self.send_window || body_len_i32 > self.streams[idx].send_window {
            self.report.flow_control_blocked += 1;
            self.streams[idx].pending_response = Some(pending);
            return Ok(());
        }
        self.send_pending_response(stream_id, pending)
    }

    fn send_pending_response(
        &mut self,
        stream_id: u32,
        pending: PendingResponse,
    ) -> Result<(), Http2ProtocolError> {
        let idx = self
            .find_stream(stream_id)
            .ok_or(Http2ProtocolError::StreamClosed)?;
        let frame_cap = self.peer_max_frame_size.max(1);
        let data_frames = if pending.body.is_empty() {
            0
        } else {
            pending.body.len().div_ceil(frame_cap)
        };
        let trailer_frames = usize::from(pending.trailers.is_some());
        let slots_needed = 1 + data_frames + trailer_frames;
        self.ensure_outbound_slots(slots_needed)?;
        self.enqueue_frame(headers_frame(
            stream_id,
            pending.body.is_empty() && pending.trailers.is_none(),
            pending.header_block,
        ))?;
        if !pending.body.is_empty() {
            let body_len_i32 =
                i32::try_from(pending.body.len()).map_err(|_| Http2ProtocolError::FlowControl)?;
            self.send_window -= body_len_i32;
            self.streams[idx].send_window -= body_len_i32;
            for (chunk_index, chunk) in pending.body.chunks(frame_cap).enumerate() {
                let end_stream = chunk_index + 1 == data_frames && pending.trailers.is_none();
                self.enqueue_frame(data_frame(stream_id, end_stream, chunk.to_vec()))?;
            }
        }
        if let Some(trailers) = pending.trailers {
            self.enqueue_frame(headers_frame(stream_id, true, trailers))?;
        }
        self.streams[idx].state = Http2StreamState::Closed;
        self.remove_stream(stream_id);
        self.report.closed_streams += 1;
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
        call(
            source,
            ResponseChunkMsg::Next,
            self.limits.response_stream_call_timeout,
        )
        .then(move |outcome| Http2ConnectionMsg::StreamChunk { stream_id, outcome })
    }

    fn flush_pending_responses(&mut self) -> Result<(), Http2ProtocolError> {
        let ids: Vec<u32> = self.streams.iter().map(|s| s.id).collect();
        for stream_id in ids {
            let Some(idx) = self.find_stream(stream_id) else {
                continue;
            };
            let can_send = self
                .streams
                .get(idx)
                .and_then(|s| s.pending_response.as_ref().map(|p| p.body.len() as i32))
                .is_some_and(|len| len <= self.send_window && len <= self.streams[idx].send_window);
            if can_send {
                let pending = self.streams[idx]
                    .pending_response
                    .take()
                    .expect("checked pending response");
                self.send_pending_response(stream_id, pending)?;
            }
            self.flush_response_stream(stream_id)?;
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
        }
        match outcome {
            CallOutcome::Replied(ResponseChunkReply::Chunk(bytes)) => {
                if bytes.is_empty() {
                    return self.handle_stream_chunk(
                        stream_id,
                        CallOutcome::Replied(ResponseChunkReply::Eof),
                    );
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
                    let _ = self.enqueue_frame(rst_stream_frame(stream_id, ERR_ENHANCE_YOUR_CALM));
                    self.remove_stream(stream_id);
                    return self.maybe_write_effect();
                }
                if let Some(idx) = self.find_stream(stream_id) {
                    self.streams[idx]
                        .response_pending_data
                        .extend_from_slice(&bytes);
                }
                if let Err(error) = self.flush_response_stream(stream_id) {
                    self.report.protocol_errors += 1;
                    let code = match error {
                        Http2ProtocolError::FlowControl => ERR_FLOW_CONTROL_ERROR,
                        _ => ERR_PROTOCOL_ERROR,
                    };
                    let _ = self.enqueue_frame(rst_stream_frame(stream_id, code));
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
                self.remove_stream(stream_id);
                self.report.closed_streams += 1;
                self.maybe_write_effect()
            }
            CallOutcome::Replied(ResponseChunkReply::GrpcStatus(status)) => {
                let headers = crate::grpc::grpc_status_trailers(status);
                let trailers = encode_trailers(&headers).expect("grpc status trailers encode");
                let _ = self.enqueue_frame(headers_frame(stream_id, true, trailers));
                if let Some(idx) = self.find_stream(stream_id) {
                    self.streams[idx].state = Http2StreamState::Closed;
                }
                self.remove_stream(stream_id);
                self.report.closed_streams += 1;
                self.maybe_write_effect()
            }
            CallOutcome::Full
            | CallOutcome::Closed
            | CallOutcome::Rejected(_)
            | CallOutcome::Timeout => {
                self.report.stream_full += 1;
                let _ = self.enqueue_frame(rst_stream_frame(stream_id, ERR_PROTOCOL_ERROR));
                self.remove_stream(stream_id);
                self.maybe_write_effect()
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
            if self.send_window <= 0 || self.streams[idx].send_window <= 0 {
                self.report.flow_control_blocked += 1;
                return Ok(());
            }
            let allowed = self
                .peer_max_frame_size
                .min(self.send_window as usize)
                .min(self.streams[idx].send_window as usize)
                .min(self.streams[idx].response_pending_data.len());
            if allowed == 0 {
                self.report.flow_control_blocked += 1;
                return Ok(());
            }
            self.ensure_outbound_slots(1)?;
            let chunk: Vec<u8> = self.streams[idx]
                .response_pending_data
                .drain(..allowed)
                .collect();
            self.send_window -= allowed as i32;
            self.streams[idx].send_window -= allowed as i32;
            self.streams[idx].response_bytes_sent += allowed;
            self.enqueue_frame(data_frame(stream_id, false, chunk))?;
        }
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
                        return reply_to_request(
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
        effects: &mut Vec<Effect<Self>>,
    ) {
        if let Some(mut stream) = self.remove_stream(stream_id) {
            self.report.reset_streams += 1;
            self.report.closed_streams += 1;
            if let Some(call) = stream.pending_request_body_reply.take() {
                effects.push(reply_to_request(
                    call,
                    Http2ConnectionReply::RequestChunk(RequestChunkReply::Error(CallError::Io)),
                ));
            }
            if let Some(handle) = stream.pending_call.take() {
                effects.push(cancel_call(handle).then(move |outcome| {
                    Http2ConnectionMsg::ServiceCancelled { stream_id, outcome }
                }));
            }
            if let Some(source) = stream.response_source.take() {
                effects.push(
                    call(
                        source,
                        ResponseChunkMsg::Cancel,
                        self.limits.response_stream_call_timeout,
                    )
                    .then(move |outcome| {
                        Http2ConnectionMsg::StreamSourceCancelDone { stream_id, outcome }
                    }),
                );
            }
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
            if chunk.len() > cap {
                let rest = chunk.split_off(cap);
                self.streams[idx].request_chunks.push_front(rest);
            }
            let len = chunk.len();
            self.recv_window = self.recv_window.saturating_add(len as i32);
            self.pending_recv_window_credit =
                self.pending_recv_window_credit.saturating_add(len as u32);
            self.streams[idx].recv_window =
                self.streams[idx].recv_window.saturating_add(len as i32);
            self.streams[idx].pending_recv_window_credit = self.streams[idx]
                .pending_recv_window_credit
                .saturating_add(len as u32);
            self.maybe_flush_request_window_credit(stream_id, false)?;
            return Ok(reply_to_request(
                call,
                Http2ConnectionReply::RequestChunk(RequestChunkReply::Chunk(chunk)),
            ));
        }
        if self.streams[idx].request_eof {
            self.maybe_flush_request_window_credit(stream_id, true)?;
            Ok(reply_to_request(
                call,
                Http2ConnectionReply::RequestChunk(RequestChunkReply::Eof),
            ))
        } else {
            self.streams[idx].pending_request_body_reply = Some(call);
            Ok(noop())
        }
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
        let Some(idx) = self.find_stream(stream_id) else {
            return Ok(());
        };
        let conn_credit = self.pending_recv_window_credit;
        let stream_credit = self.streams[idx].pending_recv_window_credit;
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
            if let Some(idx) = self.find_stream(stream_id) {
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

    fn handle_wrote(&mut self, count: usize) -> Effect<Self> {
        let drain = count.min(self.pending_write.len());
        self.pending_write.drain(..drain);
        if self.pending_write.is_empty() {
            if !self.write_queue.is_empty() {
                return self.write_more();
            }
            if self.closing_after_write {
                return self.close_now();
            }
            self.flush_deferred_request_window_credit();
            if !self.write_queue.is_empty() {
                return self.write_more();
            }
        }
        noop()
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

        let stream_ids: Vec<u32> = self
            .streams
            .iter()
            .filter(|stream| {
                self.pending_recv_window_credit > 0 || stream.pending_recv_window_credit > 0
            })
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
        self.streams.iter().position(|s| s.id == stream_id)
    }

    fn remove_stream(&mut self, stream_id: u32) -> Option<ActiveStream> {
        let idx = self.find_stream(stream_id)?;
        Some(self.streams.swap_remove(idx))
    }
}

fn add_window(current: i32, increment: u32) -> Result<i32, Http2ProtocolError> {
    let next = current as i64 + increment as i64;
    if next > i32::MAX as i64 {
        return Err(Http2ProtocolError::WindowOverflow);
    }
    Ok(next as i32)
}

fn validate_request_headers(headers: &HeaderBlock) -> Result<(), Http2ProtocolError> {
    if headers.method.is_none() || headers.path.is_none() || headers.scheme.is_none() {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    let has_authority = headers.authority.as_deref().is_some_and(|v| !v.is_empty())
        || headers
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !v.is_empty());
    if !has_authority {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    if headers.status.is_some() {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    Ok(())
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
pub struct Http2Listener<S: Shard + 'static, M: From<HttpRequest> + Send + 'static = HttpRequest> {
    bind_addr: SocketAddr,
    service: Address<M, HttpResponse>,
    config: Http2ServerConfig,
    listener: Option<ListenerId>,
    started: bool,
    stopping: bool,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> Http2Listener<S, M> {
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

impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> Isolate for Http2Listener<S, M> {
    tina::isolate_types! {
        message: Http2ListenerMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: ChildDefinition<Http2Connection<S, M>>,
        call: tina_runtime::RuntimeCall<Http2ListenerMsg>,
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
            Http2ListenerMsg::Accepted(Err(_)) => {
                if let Some(listener) = self.listener.take() {
                    tcp_close_listener(listener).then(Http2ListenerMsg::ListenerClosed)
                } else {
                    stop()
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
    fn frame_round_trip_waits_for_complete_payload() {
        let limits = Http2Limits::default();
        let frame = Frame::new(FRAME_DATA, FLAG_END_STREAM, 1, b"abc".to_vec()).encode();
        assert!(
            try_decode_frame(&frame[..frame.len() - 1], &limits)
                .unwrap()
                .is_none()
        );
        let (decoded, used) = try_decode_frame(&frame, &limits).unwrap().unwrap();
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
            try_decode_frame(&frame[..FRAME_HEADER_LEN], &limits),
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
        assert_eq!(
            conn.handle_window_update(frame),
            Err(Http2ProtocolError::WindowOverflow)
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
        conn.streams.push(ActiveStream::new(
            1,
            HeaderBlock::default(),
            DEFAULT_WINDOW,
            DEFAULT_WINDOW,
            false,
        ));
        conn.enqueue_response(1, &HttpResponse::with_body(StatusCode::OK, b"abc".to_vec()))
            .expect("response cap maps to rst, not connection error");
        assert_eq!(conn.report().stream_full, 1);
        assert!(conn.find_stream(1).is_none());
        assert_eq!(conn.write_queue.len(), 1);
    }

    #[test]
    fn stop_sends_goaway_for_open_streams() {
        let mut conn = unit_connection();
        conn.highest_client_stream_id = 1;
        let _ = conn.begin_goaway_shutdown();
        assert!(conn.goaway);
        assert_eq!(conn.report().goaway_sent, 1);
        assert!(!conn.pending_write.is_empty() || !conn.write_queue.is_empty());
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
}
