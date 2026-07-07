//! Native HTTP/2 client connection (first form).
//!
//! One isolate owns one TCP stream to a single remote authority and
//! carries many admitted client streams over it. Admission is bounded by
//! `max_concurrent_streams`. Each stream completes with one typed
//! [`Http2ClientOutcome`] reply back to the caller's request slot.
//!
//! Scope of this first form:
//! - cleartext h2c (prior knowledge) or h2 over TLS (request-response /
//!   half-duplex; full-duplex h2/TLS needs a runtime TLS reactor)
//! - request body buffered (`Http2ClientRequest`) or streamed from a
//!   chunk source (`Http2ClientStreamingRequest`); response body
//!   buffered under an explicit cap, or pulled chunk-by-chunk with
//!   credit-on-consume backpressure (`Http2ClientMsg::OpenStream` +
//!   `ResponseNext`)
//! - SETTINGS / PING / HEADERS / DATA / WINDOW_UPDATE / RST_STREAM /
//!   GOAWAY frame handling, sharing the helpers in `super::frame` /
//!   `super::headers` / `super::errors` with the server
//! - typed `Http2ClientOutcome` covers replied, full, closed, reset,
//!   protocol error, local cancel, and `TlsAlpnMismatch` (timeout and
//!   flow-control-blocked land with a future stream-level deadline)
//!
//! An `Http2Target::Tls { .. }` target dials the TLS rail offering `h2`
//! ALPN; a server that declines `h2` resolves to
//! [`Http2ClientOutcome::TlsAlpnMismatch`]. The client never silently
//! downgrades a TLS target to h2c.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, Method, StatusCode};
use tina::prelude::*;
use tina::reply_to_request;
use tina_runtime::{
    CallError, CallOutcome, Http2CloseReason, Http2ResetReason, Http2StreamId,
    ProtocolConnectionId, ProtocolDirection, ProtocolFact, StreamId, TcpReadBufReply,
    TcpWriteOwnedReply, TlsReadBufReply, TlsStreamId, TlsWriteOwnedReply, call, sleep,
    tcp_close_stream, tcp_connect, tcp_read_buf, tcp_write_owned, tls_close, tls_connect_alpn,
    tls_read_buf, tls_write_owned,
};

use crate::streaming::{ResponseChunkMsg, ResponseChunkReply};

use super::errors::{
    ERR_CANCEL, ERR_FLOW_CONTROL_ERROR, ERR_FRAME_SIZE_ERROR, ERR_NO_ERROR, ERR_PROTOCOL_ERROR,
    ERR_SETTINGS_ERROR, ERR_STREAM_CLOSED, Http2ProtocolError, classify_h2_reset,
};
#[cfg(test)]
use super::frame::try_decode_frame;
use super::frame::{
    CLIENT_PREFACE, DEFAULT_WINDOW, FLAG_ACK, FLAG_END_HEADERS, FLAG_END_STREAM, FRAME_DATA,
    FRAME_GOAWAY, FRAME_HEADER_LEN, FRAME_HEADERS, FRAME_PING, FRAME_RST_STREAM, FRAME_SETTINGS,
    FRAME_WINDOW_UPDATE, Frame, READ_CHUNK, WINDOW_CREDIT_FLUSH_THRESHOLD, add_window, data_frame,
    goaway_frame, headers_frame, headers_payload_view, into_data_payload, push_frame_header,
    push_setting, rst_stream_frame, settings_frame, try_decode_frame_meta, window_update_frame,
};
use super::headers::{
    DEFAULT_HEADER_TABLE_SIZE, HeaderBlock, MAX_MAX_FRAME_SIZE, MIN_MAX_FRAME_SIZE,
    SETTINGS_ENABLE_PUSH, SETTINGS_HEADER_TABLE_SIZE, SETTINGS_INITIAL_WINDOW_SIZE,
    SETTINGS_MAX_CONCURRENT_STREAMS, SETTINGS_MAX_FRAME_SIZE, SETTINGS_MAX_HEADER_LIST_SIZE,
    decode_headers_block_compact_with, decode_headers_block_with, encode_literal_header,
    validate_response_headers, validate_trailer_block,
};
use super::target::Http2Target;

/// The transport stream this connection owns: cleartext TCP for an
/// `Http2Target::H2c`, or a TLS stream for `Http2Target::Tls`. All of the
/// connection's IO (read / write / close) branches on this so the
/// HTTP/2 framing code above is rail-agnostic.
#[derive(Debug, Clone, Copy)]
enum ClientStream {
    Tcp(StreamId),
    Tls(TlsStreamId),
}

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
    /// Per-call timeout for TLS rail read/write/close on an
    /// `Http2Target::Tls` connection. (The TCP rail's read/write are
    /// deadline-less in this runtime.) Also bounds the TLS connect +
    /// handshake.
    pub tls_io_timeout: Duration,
    /// Streamed responses whose caller stops pulling are cancelled after
    /// this idle period so they cannot pin a connection stream slot forever.
    pub response_stream_idle_timeout: Duration,
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
            tls_io_timeout: Duration::from_secs(30),
            response_stream_idle_timeout: Duration::from_secs(30),
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
    /// Call-only messages received through fire-and-forget `try_send`.
    /// This is a caller bug, but it must be visible in release builds:
    /// request bodies must not disappear as an uncounted `noop()`.
    pub wrong_lane_messages: u64,
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
    /// The buffered response to an [`Http2ClientMsg::SubmitGrpcUnary`] call,
    /// decoded compactly into gRPC facts — no public `HeaderMap`. `grpc_status`
    /// is the raw wire code (from trailers, or from headers for a trailers-only
    /// response) and `grpc_message` is the still-percent-encoded message, if
    /// any. Generic HTTP/2 callers keep the full-header [`Replied`](Self::Replied)
    /// outcome.
    GrpcUnaryReplied {
        status: StatusCode,
        grpc_status: Option<u16>,
        grpc_message: Option<String>,
        body: Vec<u8>,
    },
    /// The response head of an [`Http2ClientMsg::OpenStream`] call: the
    /// stream opened and the response status + headers arrived. Pull the
    /// body with [`Http2ClientMsg::ResponseNext`] using `stream_id`.
    ResponseStreaming {
        status: StatusCode,
        headers: HeaderMap,
    },
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

/// One buffered gRPC unary request submitted to the client connection.
///
/// The connection emits the fixed gRPC request headers directly
/// (`content-type: application/grpc+proto`, `te: trailers`) so warmed gRPC
/// callers do not rebuild the same public `HeaderMap` on every call.
#[derive(Debug, Clone)]
pub struct Http2ClientGrpcUnaryRequest {
    path: Arc<str>,
    body: GrpcUnaryBody,
}

#[derive(Debug, Clone)]
enum GrpcUnaryBody {
    Owned(Vec<u8>),
    Shared(Arc<[u8]>),
}

impl Http2ClientGrpcUnaryRequest {
    pub(crate) fn owned(path: Arc<str>, body: Vec<u8>) -> Self {
        Self {
            path,
            body: GrpcUnaryBody::Owned(body),
        }
    }

    pub(crate) fn shared(path: Arc<str>, body: Arc<[u8]>) -> Self {
        Self {
            path,
            body: GrpcUnaryBody::Shared(body),
        }
    }

    pub fn body_len(&self) -> usize {
        match &self.body {
            GrpcUnaryBody::Owned(bytes) => bytes.len(),
            GrpcUnaryBody::Shared(bytes) => bytes.len(),
        }
    }

    pub fn body_is_shared(&self) -> bool {
        matches!(self.body, GrpcUnaryBody::Shared(_))
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn path_arc(&self) -> &Arc<str> {
        &self.path
    }
}

impl GrpcUnaryBody {
    fn is_empty(&self) -> bool {
        match self {
            Self::Owned(bytes) => bytes.is_empty(),
            Self::Shared(bytes) => bytes.is_empty(),
        }
    }

    fn into_outbound(self) -> OutboundBody {
        match self {
            Self::Owned(bytes) => OutboundBody::owned(bytes),
            Self::Shared(bytes) => OutboundBody::shared(bytes),
        }
    }
}

/// A request whose body is streamed from a chunk source, rather than
/// buffered up front. The connection sends HEADERS (without END_STREAM),
/// then pulls body chunks from `source` via `ResponseChunkMsg::Next` —
/// the same source protocol the server uses for streaming responses, so
/// [`crate::IterBodySource`] works as a request source. The source ends
/// the body by replying `Eof` (or `GrpcStatus`, treated as end-of-body
/// for a request).
#[derive(Debug, Clone)]
pub struct Http2ClientStreamingRequest {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    /// Chunk source for the request body. The connection pulls from this
    /// address; bytes ride out under flow control as credit allows.
    pub source: tina::Address<ResponseChunkMsg, ResponseChunkReply>,
}

/// The request body of an [`Http2ClientStreamCall`]: either buffered up
/// front or streamed from a chunk source. Lets one streaming-response
/// call shape serve both server-streaming (buffered request) and bidi
/// (streamed request) gRPC.
#[derive(Debug, Clone)]
pub enum Http2ClientRequestBody {
    /// Whole body known up front. Empty `Vec` = no body (END_STREAM on
    /// the HEADERS frame).
    Buffered(Vec<u8>),
    /// Body pulled chunk-by-chunk from a source, same protocol as
    /// [`Http2ClientStreamingRequest`].
    Stream(tina::Address<ResponseChunkMsg, ResponseChunkReply>),
}

/// A request whose **response** body is delivered incrementally: the
/// caller pulls response chunks from the connection rather than getting
/// one buffered [`Http2ClientResponse`]. The request body itself is
/// either buffered or streamed ([`Http2ClientRequestBody`]).
///
/// Submit with the call-only [`Http2ClientMsg::OpenStream`]. The first
/// reply is an [`Http2ClientOutcome::ResponseStreaming`] carrying the
/// response head (status + headers) — or a terminal error outcome if the
/// stream never opened. The caller then pulls the body with
/// [`Http2ClientMsg::ResponseNext`], one [`Http2ResponseChunk`] per pull,
/// until `End`/`Reset`/`Closed`. Received DATA is held under the stream
/// flow-control window and only `WINDOW_UPDATE`-credited as the caller
/// consumes it, so a slow consumer backpressures the peer.
#[derive(Debug, Clone)]
pub struct Http2ClientStreamCall {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Http2ClientRequestBody,
}

/// One piece of a streamed response body, returned in
/// [`Http2ClientReply::ResponseChunk`] per [`Http2ClientMsg::ResponseNext`]
/// pull.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Http2ResponseChunk {
    /// A chunk of response body bytes. Pull again for more.
    Data(Vec<u8>),
    /// Clean end of the response (END_STREAM). Carries any trailing
    /// HEADERS block; gRPC reads `grpc-status` from here. The stream is
    /// now closed — do not pull again.
    End { trailers: HeaderMap },
    /// The peer reset the stream (RST_STREAM) before END_STREAM.
    Reset(Http2ResetReason),
    /// The connection closed (or GOAWAY refused the stream) before
    /// END_STREAM.
    Closed,
    /// A protocol error closed the stream/connection.
    ProtocolError(Http2ProtocolError),
}

/// Messages handled by [`Http2ClientConnection`].
///
/// `Submit` and `Report` are **call-only**: they must be delivered with
/// `call` / `call_blocking`, which provide the reply channel the
/// connection answers on. Delivering them with `try_send` has no reply
/// channel. The connection increments
/// [`Http2ClientReport::wrong_lane_messages`] so the bad send is visible
/// instead of disappearing as an uncounted `noop()`.
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
    /// Submit a buffered gRPC unary request with fixed gRPC request headers.
    /// **Call-only**.
    SubmitGrpcUnary(Http2ClientGrpcUnaryRequest),
    /// Submit a request whose body is streamed from a chunk source. Like
    /// `Submit`, replies once with an [`Http2ClientReply::Outcome`] when
    /// the response completes. **Call-only**.
    SubmitStreaming(Http2ClientStreamingRequest),
    /// Open a stream whose **response** is delivered incrementally. The
    /// first reply is an [`Http2ClientOutcome::ResponseStreaming`] head
    /// (or a terminal error outcome); the caller then pulls the body with
    /// [`ResponseNext`](Http2ClientMsg::ResponseNext). **Call-only**.
    OpenStream(Http2ClientStreamCall),
    /// Pull the next [`Http2ResponseChunk`] of a streamed response,
    /// identified by `stream_id` (from the `ResponseStreaming` head). One
    /// pull outstanding per stream; a second concurrent pull is rejected.
    /// **Call-only**; replies with [`Http2ClientReply::ResponseChunk`].
    ResponseNext { stream_id: u32 },
    /// Locally cancel an admitted stream by id. The connection emits
    /// RST_STREAM(CANCEL) on the wire and replies to the original
    /// submitter with [`Http2ClientOutcome::LocalCancel`].
    Cancel { stream_id: u32 },
    /// Internal: a streaming request body chunk pull completed.
    RequestChunk {
        stream_id: u32,
        outcome: CallOutcome<ResponseChunkReply>,
    },
    /// Internal: a `ResponseChunkMsg::Cancel` sent to an abandoned
    /// streaming request source completed. Absorbed and ignored — the
    /// stream is already gone; the cancel only released the source.
    RequestSourceCancelled,
    /// Internal: a streamed response has had no caller pull for the
    /// configured idle period.
    ResponseIdleTimeout { stream_id: u32 },
    /// Snapshot the per-connection report.
    Report,
    /// Begin graceful shutdown (GOAWAY) and stop the isolate.
    Stop,
    /// Internal: TCP connect completion (h2c targets).
    Connected(Result<(StreamId, std::net::SocketAddr, std::net::SocketAddr), CallError>),
    /// Internal: TLS connect completion (h2/TLS targets), carrying the
    /// negotiated ALPN protocol.
    TlsConnected(Result<(TlsStreamId, Option<Vec<u8>>), CallError>),
    /// Internal: read completion (either rail).
    Read(Result<TcpReadBufReply, CallError>),
    /// Internal: write completion (either rail).
    Wrote(Result<TcpWriteOwnedReply, CallError>),
    /// Internal: close completion (either rail).
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
    /// One chunk of a streamed response body, replying to
    /// [`Http2ClientMsg::ResponseNext`].
    ResponseChunk {
        stream_id: u32,
        chunk: Http2ResponseChunk,
    },
    /// Report snapshot.
    Report(Http2ClientReport),
}

#[derive(Debug)]
enum OutboundBody {
    Owned { bytes: Vec<u8>, cursor: usize },
    Shared { bytes: Arc<[u8]>, cursor: usize },
}

impl OutboundBody {
    fn owned(bytes: Vec<u8>) -> Self {
        Self::Owned { bytes, cursor: 0 }
    }

    fn shared(bytes: Arc<[u8]>) -> Self {
        Self::Shared { bytes, cursor: 0 }
    }

    fn remaining(&self) -> usize {
        match self {
            Self::Owned { bytes, cursor } => bytes.len() - *cursor,
            Self::Shared { bytes, cursor } => bytes.len() - *cursor,
        }
    }

    fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn slice(&self, len: usize) -> &[u8] {
        match self {
            Self::Owned { bytes, cursor } => &bytes[*cursor..*cursor + len],
            Self::Shared { bytes, cursor } => &bytes[*cursor..*cursor + len],
        }
    }

    fn append(&mut self, chunk: &[u8]) {
        match self {
            Self::Owned { bytes, cursor } => {
                if *cursor > 0 {
                    bytes.drain(..*cursor);
                    *cursor = 0;
                }
                bytes.extend_from_slice(chunk);
            }
            Self::Shared { bytes, cursor } => {
                let mut owned = bytes[*cursor..].to_vec();
                owned.extend_from_slice(chunk);
                *self = Self::Owned {
                    bytes: owned,
                    cursor: 0,
                };
            }
        }
    }

    fn advance(&mut self, n: usize) {
        match self {
            Self::Owned { bytes, cursor } => {
                *cursor += n;
                if *cursor >= bytes.len() {
                    bytes.clear();
                    *cursor = 0;
                }
            }
            Self::Shared { bytes, cursor } => {
                *cursor += n;
                if *cursor >= bytes.len() {
                    *self = Self::Owned {
                        bytes: Vec::new(),
                        cursor: 0,
                    };
                }
            }
        }
    }
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
    /// Bytes of request body still to send. Consumed as DATA frames are
    /// admitted under stream + connection flow-control credit. Ordinary
    /// buffered/streaming requests use owned bytes; hot gRPC templates can use
    /// shared preframed bytes without cloning the body per call. A cursor
    /// replaces the old per-byte `VecDeque<u8>` drain; sent owned prefixes are
    /// compacted/dropped so a long request body does not stay resident.
    outbound_body: OutboundBody,
    /// Streaming request body source. `None` for a buffered request.
    /// The connection pulls chunks via `ResponseChunkMsg::Next`.
    request_source: Option<tina::Address<ResponseChunkMsg, ResponseChunkReply>>,
    /// True once all request body bytes are known (a buffered request, or
    /// a streaming source that returned `Eof`). The final DATA frame sets
    /// END_STREAM only when this is true and `outbound_body` is empty.
    request_complete: bool,
    /// True once END_STREAM has been emitted for the request (on the
    /// HEADERS frame for an empty body, or on a DATA frame). Prevents a
    /// double END_STREAM.
    request_end_sent: bool,
    /// A `ResponseChunkMsg::Next` pull is awaiting its reply.
    request_pull_in_flight: bool,
    /// True when the caller asked for the response body to be delivered
    /// incrementally (`OpenStream`). The buffered path leaves this false.
    response_streamed: bool,
    /// True once the `ResponseStreaming` head has been replied to the
    /// `OpenStream` waiter. Guards against re-sending it.
    response_head_sent: bool,
    /// Received response DATA payloads not yet pulled by the caller. Only
    /// used for a streamed response. Bounded by the stream recv window
    /// because credit is held until the caller consumes.
    response_chunks: VecDeque<Vec<u8>>,
    /// A caller `ResponseNext` pull parked because no chunk is buffered
    /// yet and END_STREAM has not arrived. Satisfied by the next DATA /
    /// trailers / teardown.
    response_pull: Option<tina::RequestContext<Http2ClientReply>>,
    /// END_STREAM has been seen for the response. Once the buffered
    /// chunks drain, the next pull gets `End`.
    response_eof: bool,
    /// Total received response body bytes on a streamed response, compared
    /// against `response_content_length` at END_STREAM. The buffered path
    /// uses `response_body.len()`; the streamed path drains its chunks, so it
    /// needs its own running counter to tell the same content-length truth.
    response_body_received: usize,
    /// True for a stream opened by [`Http2ClientMsg::SubmitGrpcUnary`]. Such a
    /// stream decodes its response head/trailers compactly (gRPC facts only, no
    /// public `HeaderMap`) and completes with
    /// [`Http2ClientOutcome::GrpcUnaryReplied`].
    grpc_unary: bool,
    /// Compact gRPC response facts, captured for a `grpc_unary` stream instead
    /// of building `response_headers`/`response_trailers`.
    grpc_status: Option<u16>,
    grpc_message: Option<String>,
}

impl ActiveClientStream {
    fn new(
        id: u32,
        recv_window: i32,
        send_window: i32,
        waiter: tina::RequestContext<Http2ClientReply>,
        outbound_body: OutboundBody,
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
            outbound_body,
            // A buffered request has its whole body up front.
            request_source: None,
            request_complete: true,
            request_end_sent: false,
            request_pull_in_flight: false,
            response_streamed: false,
            response_head_sent: false,
            response_chunks: VecDeque::new(),
            response_pull: None,
            response_eof: false,
            response_body_received: 0,
            grpc_unary: false,
            grpc_status: None,
            grpc_message: None,
        }
    }

    fn new_streaming(
        id: u32,
        recv_window: i32,
        send_window: i32,
        waiter: tina::RequestContext<Http2ClientReply>,
        source: tina::Address<ResponseChunkMsg, ResponseChunkReply>,
    ) -> Self {
        let mut stream = Self::new(
            id,
            recv_window,
            send_window,
            waiter,
            OutboundBody::owned(Vec::new()),
        );
        stream.request_source = Some(source);
        stream.request_complete = false;
        stream
    }

    /// Unsent request-body bytes remaining after the outbound cursor.
    fn outbound_remaining(&self) -> usize {
        self.outbound_body.remaining()
    }

    /// True while request-body bytes remain to be framed.
    fn has_outbound(&self) -> bool {
        !self.outbound_body.is_empty()
    }

    /// Append a streaming request-body chunk, compacting the already-sent
    /// prefix first so a long streaming request does not keep sent bytes
    /// resident.
    fn append_outbound(&mut self, bytes: &[u8]) {
        self.outbound_body.append(bytes);
    }

    /// Mark `n` more request-body bytes consumed; drop the buffer once it is
    /// fully drained so a finished request body does not stay resident.
    fn advance_outbound(&mut self, n: usize) {
        self.outbound_body.advance(n);
    }

    fn outbound_slice(&self, len: usize) -> &[u8] {
        self.outbound_body.slice(len)
    }
}

/// Native HTTP/2 client connection isolate.
pub struct Http2ClientConnection<S: Shard + 'static> {
    target: Http2Target,
    limits: Http2ClientLimits,
    stream: Option<ClientStream>,
    /// ALPN protocol negotiated on a TLS connection (raw wire bytes),
    /// `None` for h2c or when no ALPN was negotiated.
    negotiated_alpn: Option<Vec<u8>>,
    /// Submits waiting for the connect + preface to flush.
    queued_submits: VecDeque<(Http2ClientRequest, tina::RequestContext<Http2ClientReply>)>,
    /// Compact gRPC unary submits waiting for the connect + preface to flush.
    queued_grpc_unary: VecDeque<(
        Http2ClientGrpcUnaryRequest,
        tina::RequestContext<Http2ClientReply>,
    )>,
    /// Streaming submits waiting for the connect + preface to flush.
    queued_streaming: VecDeque<(
        Http2ClientStreamingRequest,
        tina::RequestContext<Http2ClientReply>,
    )>,
    /// Streaming-response opens waiting for the connect + preface to flush.
    queued_open: VecDeque<(
        Http2ClientStreamCall,
        tina::RequestContext<Http2ClientReply>,
    )>,
    streams: Vec<ActiveClientStream>,
    /// stream-id → slot index in `streams`, kept in step with every push and
    /// `swap_remove`. Turns the per-frame stream lookup (several per frame)
    /// from O(open streams) into O(1), mirroring the server.
    stream_index: HashMap<u32, usize>,
    next_stream_id: u32,
    read_buf: Vec<u8>,
    read_scratch: Vec<u8>,
    /// Reused buffer for inline (non-DATA) inbound frame payloads, so decoding
    /// HEADERS/SETTINGS/WINDOW_UPDATE/etc. does not allocate a fresh `Vec` per
    /// frame. DATA frames keep their own owned buffer (they are queued for the
    /// caller to pull and must outlive this scratch).
    frame_scratch: Vec<u8>,
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
    /// True from when a read is dispatched until its `Read` completion.
    /// On the TCP rail read and write are independent lanes, so this is
    /// just bookkeeping; on the TLS rail read and write share one lane
    /// (and one blocking worker), so the connection runs half-duplex —
    /// at most one of read/write may be in flight at a time.
    read_in_flight: bool,
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
        // These are public caller-supplied budgets. Keep bad values loud
        // in release too; otherwise a service can ship with a client that
        // silently rejects every request or advertises an invalid HTTP/2
        // frame size.
        assert!(
            limits.max_concurrent_streams >= 1,
            "Http2ClientLimits::max_concurrent_streams must be >= 1 (got {})",
            limits.max_concurrent_streams,
        );
        assert!(
            limits.connection_outbound_queue_capacity >= 1,
            "Http2ClientLimits::connection_outbound_queue_capacity must be >= 1 (got {})",
            limits.connection_outbound_queue_capacity,
        );
        assert!(
            limits.pre_connect_submit_capacity >= 1,
            "Http2ClientLimits::pre_connect_submit_capacity must be >= 1 (got {})",
            limits.pre_connect_submit_capacity,
        );
        assert!(
            limits.max_frame_size >= MIN_MAX_FRAME_SIZE as usize
                && limits.max_frame_size <= MAX_MAX_FRAME_SIZE as usize,
            "Http2ClientLimits::max_frame_size must be in HTTP/2 range {MIN_MAX_FRAME_SIZE}..={MAX_MAX_FRAME_SIZE} (got {})",
            limits.max_frame_size,
        );
        assert!(
            !limits.tls_io_timeout.is_zero(),
            "Http2ClientLimits::tls_io_timeout must be non-zero",
        );
        Self {
            target,
            limits,
            stream: None,
            negotiated_alpn: None,
            queued_submits: VecDeque::new(),
            queued_grpc_unary: VecDeque::new(),
            queued_streaming: VecDeque::new(),
            queued_open: VecDeque::new(),
            streams: Vec::with_capacity(limits.max_concurrent_streams),
            stream_index: HashMap::with_capacity(limits.max_concurrent_streams),
            next_stream_id: 1,
            read_buf: Vec::new(),
            read_scratch: Vec::new(),
            frame_scratch: Vec::new(),
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
            read_in_flight: false,
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
        // Every fact-emitting path runs inside `handle` / `handle_call`,
        // which set `self_isolate_id` on first entry. A `None` here means
        // a fact was emitted before the first handler turn — a bug that
        // would silently tag the fact with connection id 0 and break
        // replay correlation. Catch it in dev/test.
        debug_assert!(
            self.self_isolate_id.is_some(),
            "connection_fact_id() called before self_isolate_id was set",
        );
        ProtocolConnectionId::new(self.self_isolate_id.map(|id| id.get()).unwrap_or_default())
    }

    fn queued_pre_connect_len(&self) -> usize {
        self.queued_submits.len()
            + self.queued_grpc_unary.len()
            + self.queued_streaming.len()
            + self.queued_open.len()
    }

    fn pre_connect_queue_full(&self) -> bool {
        self.queued_pre_connect_len() >= self.limits.pre_connect_submit_capacity
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
            Http2ClientMsg::Connected(Ok((stream, _, _))) => {
                self.handle_connected(ClientStream::Tcp(stream), None)
            }
            Http2ClientMsg::Connected(Err(_)) => self.close_with(Http2ClientOutcome::Closed),
            Http2ClientMsg::TlsConnected(Ok((stream, selected_alpn))) => {
                self.handle_tls_connected(stream, selected_alpn)
            }
            Http2ClientMsg::TlsConnected(Err(CallError::TlsAlpnMismatch)) => {
                self.close_with(Http2ClientOutcome::TlsAlpnMismatch)
            }
            Http2ClientMsg::TlsConnected(Err(_)) => self.close_with(Http2ClientOutcome::Closed),
            Http2ClientMsg::Read(Ok(reply)) => self.handle_read(reply),
            Http2ClientMsg::Read(Err(_)) => self.close_with(Http2ClientOutcome::Closed),
            Http2ClientMsg::Wrote(Ok(reply)) => self.handle_wrote(reply),
            Http2ClientMsg::Wrote(Err(_)) => self.close_with(Http2ClientOutcome::Closed),
            Http2ClientMsg::Closed(_) => stop(),
            Http2ClientMsg::Cancel { stream_id } => self.handle_cancel(stream_id),
            Http2ClientMsg::RequestChunk { stream_id, outcome } => {
                self.handle_request_chunk(stream_id, outcome)
            }
            Http2ClientMsg::RequestSourceCancelled => noop(),
            Http2ClientMsg::ResponseIdleTimeout { stream_id } => {
                self.handle_response_idle_timeout(stream_id)
            }
            Http2ClientMsg::Stop => self.begin_goaway_shutdown(),
            Http2ClientMsg::Report
            | Http2ClientMsg::Submit(_)
            | Http2ClientMsg::SubmitGrpcUnary(_)
            | Http2ClientMsg::SubmitStreaming(_)
            | Http2ClientMsg::OpenStream(_)
            | Http2ClientMsg::ResponseNext { .. } => self.wrong_lane_message(),
        }
    }

    fn handle_call(
        &mut self,
        msg: Http2ClientMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            Http2ClientMsg::Submit(req) => self.handle_submit(req, call),
            Http2ClientMsg::SubmitGrpcUnary(req) => self.handle_submit_grpc_unary(req, call),
            Http2ClientMsg::SubmitStreaming(req) => self.handle_submit_streaming(req, call),
            Http2ClientMsg::OpenStream(req) => self.handle_open_stream(req, call),
            Http2ClientMsg::ResponseNext { stream_id } => {
                self.handle_response_next(stream_id, call)
            }
            Http2ClientMsg::Report => call.reply(Http2ClientReply::Report(self.report.clone())),
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl<S: Shard + 'static> Http2ClientConnection<S> {
    fn handle_response_idle_timeout(&mut self, stream_id: u32) -> Effect<Self> {
        let Some(idx) = self.find_stream(stream_id) else {
            return noop();
        };
        if !self.streams[idx].response_streamed
            || !self.streams[idx].response_head_sent
            || self.streams[idx].response_pull.is_some()
        {
            return noop();
        }
        let mut effects = Vec::new();
        if self.streams[idx].response_eof {
            self.complete_streaming_stream(idx, &mut effects);
            self.pump_io(&mut effects);
            return batch(effects);
        }
        let mut stream = self.swap_remove_stream_at(idx);
        self.cancel_request_source(&stream, &mut effects);
        self.enqueue_frame(rst_stream_frame(stream_id, ERR_CANCEL));
        self.report.locally_cancelled += 1;
        self.report.closed_streams += 1;
        effects.push(emit_fact(ProtocolFact::Http2StreamReset {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            direction: ProtocolDirection::Outbound,
            reason: Http2ResetReason::Cancel,
        }));
        self.settle_stream_terminal(&mut stream, Http2ClientOutcome::LocalCancel, &mut effects);
        self.pump_io(&mut effects);
        batch(effects)
    }

    fn begin_connect(&mut self) -> Effect<Self> {
        if self.preface_sent || self.stream.is_some() {
            return noop();
        }
        match &self.target {
            Http2Target::Tls {
                addr,
                server_name,
                trust_roots,
                alpn,
                ..
            } => tls_connect_alpn(
                *addr,
                server_name.clone(),
                trust_roots.clone(),
                alpn.wire(),
                self.limits.tls_io_timeout,
            )
            .then(Http2ClientMsg::TlsConnected),
            Http2Target::H2c { addr, .. } => {
                let addr = *addr;
                tcp_connect(addr).then(Http2ClientMsg::Connected)
            }
        }
    }

    fn wrong_lane_message(&mut self) -> Effect<Self> {
        self.report.wrong_lane_messages = self.report.wrong_lane_messages.saturating_add(1);
        noop()
    }

    /// TLS connect completed. If the target asked for `h2` ALPN, require
    /// the server to have selected `h2` — otherwise fail closed with the
    /// typed mismatch rather than speaking h2 on an unnegotiated protocol.
    fn handle_tls_connected(
        &mut self,
        stream: TlsStreamId,
        selected_alpn: Option<Vec<u8>>,
    ) -> Effect<Self> {
        let wants_h2 = matches!(&self.target, Http2Target::Tls { alpn, .. } if alpn.is_h2());
        if wants_h2 && selected_alpn.as_deref() != Some(b"h2") {
            // The rail already maps offered-but-none to TlsAlpnMismatch;
            // this guards the (rustls-impossible but defensive) case of a
            // different selection.
            return self.close_with(Http2ClientOutcome::TlsAlpnMismatch);
        }
        self.negotiated_alpn = selected_alpn.clone();
        self.handle_connected(ClientStream::Tls(stream), selected_alpn)
    }

    fn handle_connected(
        &mut self,
        stream: ClientStream,
        _selected_alpn: Option<Vec<u8>>,
    ) -> Effect<Self> {
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
        let queued_grpc_unary = std::mem::take(&mut self.queued_grpc_unary);
        for (req, waiter) in queued_grpc_unary {
            self.admit_grpc_unary_stream(req, waiter, &mut effects);
        }
        let queued_streaming = std::mem::take(&mut self.queued_streaming);
        for (req, waiter) in queued_streaming {
            self.admit_streaming_stream(req, waiter, &mut effects);
        }
        let queued_open = std::mem::take(&mut self.queued_open);
        for (req, waiter) in queued_open {
            self.admit_open_stream(req, waiter, &mut effects);
        }
        self.pump_io(&mut effects);
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
        // Until the connect (TCP or TLS) + preface flush, queue the
        // submit. Once `Connected`/`TlsConnected` resolves, the queued
        // submits flush in order. A failed TLS connect (incl. ALPN
        // mismatch) drains the queue with the typed outcome.
        if self.stream.is_none() {
            if self.pre_connect_queue_full() {
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
        let mut effects: Vec<Effect<Self>> = Vec::new();
        self.admit_stream(req, waiter, &mut effects);
        self.pump_io(&mut effects);
        batch(effects)
    }

    fn handle_submit_grpc_unary(
        &mut self,
        req: Http2ClientGrpcUnaryRequest,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        let waiter = call.into_request_context();
        if self.stream.is_none() {
            if self.pre_connect_queue_full() {
                self.report.admission_full += 1;
                return reply_to_request::<Self>(
                    waiter,
                    Http2ClientReply::Outcome {
                        stream_id: 0,
                        outcome: Http2ClientOutcome::Full,
                    },
                );
            }
            self.queued_grpc_unary.push_back((req, waiter));
            return noop();
        }
        let mut effects: Vec<Effect<Self>> = Vec::new();
        self.admit_grpc_unary_stream(req, waiter, &mut effects);
        self.pump_io(&mut effects);
        batch(effects)
    }

    fn handle_submit_streaming(
        &mut self,
        req: Http2ClientStreamingRequest,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        let waiter = call.into_request_context();
        if self.stream.is_none() {
            if self.pre_connect_queue_full() {
                self.report.admission_full += 1;
                return reply_to_request::<Self>(
                    waiter,
                    Http2ClientReply::Outcome {
                        stream_id: 0,
                        outcome: Http2ClientOutcome::Full,
                    },
                );
            }
            self.queued_streaming.push_back((req, waiter));
            return noop();
        }
        let mut effects: Vec<Effect<Self>> = Vec::new();
        self.admit_streaming_stream(req, waiter, &mut effects);
        self.pump_io(&mut effects);
        batch(effects)
    }

    /// A streaming request-body chunk pull completed.
    fn handle_request_chunk(
        &mut self,
        stream_id: u32,
        outcome: CallOutcome<ResponseChunkReply>,
    ) -> Effect<Self> {
        let Some(idx) = self.find_stream(stream_id) else {
            // Stream gone (reset/cancelled/closed); drop the late chunk.
            return noop();
        };
        self.streams[idx].request_pull_in_flight = false;
        match outcome {
            CallOutcome::Replied(ResponseChunkReply::Chunk(bytes)) => {
                self.streams[idx].append_outbound(&bytes);
            }
            CallOutcome::Replied(ResponseChunkReply::Eof)
            | CallOutcome::Replied(ResponseChunkReply::GrpcStatus(_)) => {
                // End of request body. `flush_outbound_data` will emit the
                // final/empty END_STREAM DATA. (A request body has no gRPC
                // trailers; `GrpcStatus` from a source is treated as Eof.)
                self.streams[idx].request_complete = true;
            }
            // The source failed or the call could not be delivered. Abort
            // the request stream with a local RST_STREAM(CANCEL).
            CallOutcome::Full
            | CallOutcome::Closed
            | CallOutcome::Timeout
            | CallOutcome::Rejected(_) => {
                self.enqueue_frame(rst_stream_frame(stream_id, ERR_CANCEL));
                let mut effects: Vec<Effect<Self>> = Vec::new();
                self.fail_stream(
                    idx,
                    Http2ClientOutcome::LocalCancel,
                    Http2CloseReason::LocalCloseOnly,
                    &mut effects,
                );
                self.pump_io(&mut effects);
                return batch(effects);
            }
        }
        let mut effects: Vec<Effect<Self>> = Vec::new();
        self.flush_outbound_data();
        self.pump_request_pulls(&mut effects);
        self.pump_io(&mut effects);
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
        let mut stream = ActiveClientStream::new(
            stream_id,
            self.limits.initial_stream_window,
            self.peer_initial_stream_window,
            waiter,
            OutboundBody::owned(req.body),
        );
        // An empty buffered body ended the request on the HEADERS frame.
        stream.request_end_sent = end_stream;
        self.report.opened_streams += 1;
        effects.push(emit_fact(ProtocolFact::Http2StreamOpened {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            direction: ProtocolDirection::Outbound,
        }));
        self.push_stream(stream);
        // The body bytes (if any) ride out as `flush_outbound_data` finds
        // stream + connection send-window credit. RFC 9113 §6.9: we must
        // not exceed either window.
        self.flush_outbound_data();
    }

    fn admit_grpc_unary_stream(
        &mut self,
        req: Http2ClientGrpcUnaryRequest,
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

        let header_block = encode_grpc_unary_request_header_block(&self.target, &req.path);
        if header_block.len() > self.peer_max_frame_size {
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
            None => self.stream_id_exhausted = true,
        }
        let end_stream = req.body.is_empty();
        self.enqueue_frame(headers_frame(stream_id, end_stream, header_block));
        let mut stream = ActiveClientStream::new(
            stream_id,
            self.limits.initial_stream_window,
            self.peer_initial_stream_window,
            waiter,
            req.body.into_outbound(),
        );
        stream.request_end_sent = end_stream;
        // Decode this stream's response compactly into gRPC facts.
        stream.grpc_unary = true;
        self.report.opened_streams += 1;
        effects.push(emit_fact(ProtocolFact::Http2StreamOpened {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            direction: ProtocolDirection::Outbound,
        }));
        self.push_stream(stream);
        self.flush_outbound_data();
    }

    /// Admit a streaming-request stream: send HEADERS without END_STREAM
    /// and pull the first body chunk from the source.
    fn admit_streaming_stream(
        &mut self,
        req: Http2ClientStreamingRequest,
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
        if self.streams.len() >= self.limits.max_concurrent_streams
            || self
                .peer_max_concurrent_streams
                .is_some_and(|cap| self.streams.len() as u32 >= cap)
            || self.write_queue.len() >= self.limits.connection_outbound_queue_capacity
        {
            self.report.admission_full += 1;
            effects.push(reject_outcome(waiter, 0, Http2ClientOutcome::Full));
            return;
        }
        let header_block =
            encode_request_header_block(&self.target, &req.method, &req.path, &req.headers);
        if header_block.len() > self.peer_max_frame_size {
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
            None => self.stream_id_exhausted = true,
        }
        // HEADERS without END_STREAM — the body streams as DATA frames.
        self.enqueue_frame(headers_frame(stream_id, false, header_block));
        let stream = ActiveClientStream::new_streaming(
            stream_id,
            self.limits.initial_stream_window,
            self.peer_initial_stream_window,
            waiter,
            req.source,
        );
        self.report.opened_streams += 1;
        effects.push(emit_fact(ProtocolFact::Http2StreamOpened {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            direction: ProtocolDirection::Outbound,
        }));
        self.push_stream(stream);
        // Pull the first body chunk.
        self.pump_request_pulls(effects);
    }

    fn handle_open_stream(
        &mut self,
        req: Http2ClientStreamCall,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        let waiter = call.into_request_context();
        if self.stream.is_none() {
            if self.pre_connect_queue_full() {
                self.report.admission_full += 1;
                return reply_to_request::<Self>(
                    waiter,
                    Http2ClientReply::Outcome {
                        stream_id: 0,
                        outcome: Http2ClientOutcome::Full,
                    },
                );
            }
            self.queued_open.push_back((req, waiter));
            return noop();
        }
        let mut effects: Vec<Effect<Self>> = Vec::new();
        self.admit_open_stream(req, waiter, &mut effects);
        self.pump_io(&mut effects);
        batch(effects)
    }

    /// Admit a streaming-response stream. Sends HEADERS (END_STREAM only
    /// for an empty buffered body), starts the request body (buffered
    /// flush or source pull), and marks the stream so its response head
    /// is delivered as [`Http2ClientOutcome::ResponseStreaming`] and its
    /// body is pulled chunk-by-chunk.
    fn admit_open_stream(
        &mut self,
        req: Http2ClientStreamCall,
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
        if self.streams.len() >= self.limits.max_concurrent_streams
            || self
                .peer_max_concurrent_streams
                .is_some_and(|cap| self.streams.len() as u32 >= cap)
            || self.write_queue.len() >= self.limits.connection_outbound_queue_capacity
        {
            self.report.admission_full += 1;
            effects.push(reject_outcome(waiter, 0, Http2ClientOutcome::Full));
            return;
        }
        let Http2ClientStreamCall {
            method,
            path,
            headers,
            body,
        } = req;
        let header_block = encode_request_header_block(&self.target, &method, &path, &headers);
        if header_block.len() > self.peer_max_frame_size {
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
            None => self.stream_id_exhausted = true,
        }
        let end_on_headers = matches!(&body, Http2ClientRequestBody::Buffered(b) if b.is_empty());
        self.enqueue_frame(headers_frame(stream_id, end_on_headers, header_block));
        let mut stream = match body {
            Http2ClientRequestBody::Buffered(b) => ActiveClientStream::new(
                stream_id,
                self.limits.initial_stream_window,
                self.peer_initial_stream_window,
                waiter,
                OutboundBody::owned(b),
            ),
            Http2ClientRequestBody::Stream(source) => ActiveClientStream::new_streaming(
                stream_id,
                self.limits.initial_stream_window,
                self.peer_initial_stream_window,
                waiter,
                source,
            ),
        };
        stream.response_streamed = true;
        stream.request_end_sent = end_on_headers;
        self.report.opened_streams += 1;
        effects.push(emit_fact(ProtocolFact::Http2StreamOpened {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            direction: ProtocolDirection::Outbound,
        }));
        self.push_stream(stream);
        // Buffered body rides out under flow control; a streamed body is
        // pulled. Exactly one of these does work for a given stream.
        self.flush_outbound_data();
        self.pump_request_pulls(effects);
    }

    /// O(1) stream-id → slot lookup via the index.
    fn find_stream(&self, stream_id: u32) -> Option<usize> {
        self.stream_index.get(&stream_id).copied()
    }

    /// Append a stream and record its slot in the index.
    fn push_stream(&mut self, stream: ActiveClientStream) {
        self.stream_index.insert(stream.id, self.streams.len());
        self.streams.push(stream);
    }

    /// Remove the stream at `idx` with `swap_remove`, keeping the index
    /// consistent: drop the removed id and re-point the tail element the swap
    /// moved into the hole.
    fn swap_remove_stream_at(&mut self, idx: usize) -> ActiveClientStream {
        let removed = self.streams.swap_remove(idx);
        self.stream_index.remove(&removed.id);
        if let Some(moved) = self.streams.get(idx) {
            self.stream_index.insert(moved.id, idx);
        }
        removed
    }

    /// A caller pulled the next chunk of a streamed response. One pull is
    /// outstanding per stream; a second concurrent pull is rejected.
    fn handle_response_next(
        &mut self,
        stream_id: u32,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        let Some(idx) = self.find_stream(stream_id) else {
            // Stream gone (completed, reset, or connection closed). The
            // terminal chunk was delivered to an earlier pull if one was
            // parked; a fresh pull gets `Closed`.
            return call.reply(Http2ClientReply::ResponseChunk {
                stream_id,
                chunk: Http2ResponseChunk::Closed,
            });
        };
        if !self.streams[idx].response_streamed {
            return call.reject(tina::CallRejectedReason::UnsupportedMessage);
        }
        if self.streams[idx].response_pull.is_some() {
            // One pull at a time. The caller broke the contract; reject
            // the new pull rather than dropping the parked one.
            return call.reject(tina::CallRejectedReason::UnsupportedMessage);
        }
        self.streams[idx].response_pull = Some(call.into_request_context());
        let mut effects: Vec<Effect<Self>> = Vec::new();
        self.deliver_to_parked_pull(idx, &mut effects);
        self.pump_io(&mut effects);
        batch(effects)
    }

    /// If a `ResponseNext` pull is parked on stream `idx`, satisfy it with
    /// the next available chunk: a buffered DATA payload (crediting its
    /// bytes back to the peer as the caller consumes — the backpressure
    /// lever), or `End` once the body has drained and END_STREAM arrived.
    /// Leaves the pull parked when neither is ready.
    fn deliver_to_parked_pull(&mut self, idx: usize, effects: &mut Vec<Effect<Self>>) {
        if self.streams[idx].response_pull.is_none() {
            return;
        }
        if let Some(bytes) = self.streams[idx].response_chunks.pop_front() {
            // Consume credit for the bytes handed to the caller, reopening
            // the stream window so the peer may send more.
            let n = bytes.len() as i32;
            self.streams[idx].recv_window = self.streams[idx].recv_window.saturating_add(n);
            let stream_id = self.streams[idx].id;
            self.enqueue_frame(window_update_frame(stream_id, bytes.len() as u32));
            let pull = self.streams[idx]
                .response_pull
                .take()
                .expect("pull present");
            effects.push(reply_to_request::<Self>(
                pull,
                Http2ClientReply::ResponseChunk {
                    stream_id,
                    chunk: Http2ResponseChunk::Data(bytes),
                },
            ));
            self.arm_response_idle_timeout(stream_id, effects);
            return;
        }
        if self.streams[idx].response_eof {
            self.complete_streaming_stream(idx, effects);
        }
    }

    fn arm_response_idle_timeout(&self, stream_id: u32, effects: &mut Vec<Effect<Self>>) {
        effects.push(
            sleep(self.limits.response_stream_idle_timeout)
                .then(move |_| Http2ClientMsg::ResponseIdleTimeout { stream_id }),
        );
    }

    /// Deliver the terminal `End` chunk of a streamed response and close
    /// the stream: emit the gRPC final-status fact (if `grpc-status` is in
    /// the trailers/headers) and the stream-closed fact, then remove the
    /// slot. Called once the buffered body has drained and END_STREAM has
    /// been seen, with a pull parked.
    fn complete_streaming_stream(&mut self, idx: usize, effects: &mut Vec<Effect<Self>>) {
        // A declared content-length the body did not honor is a malformed
        // response (RFC 9113 §8.1.1). Mirror the buffered branch's terminal
        // cause: RST the peer and settle the parked pull as a protocol error
        // instead of handing back a clean `End`.
        if let Some(declared) = self.streams[idx].response_content_length {
            if declared != self.streams[idx].response_body_received {
                let stream_id = self.streams[idx].id;
                self.enqueue_frame(rst_stream_frame(stream_id, ERR_PROTOCOL_ERROR));
                self.fail_stream(
                    idx,
                    Http2ClientOutcome::ProtocolError(Http2ProtocolError::ContentLengthMismatch),
                    Http2CloseReason::LocalCloseOnly,
                    effects,
                );
                return;
            }
        }
        let mut stream = self.swap_remove_stream_at(idx);
        let stream_id = stream.id;
        self.report.closed_streams += 1;
        if let Some(status) = grpc_status_from_headers(&stream.response_trailers)
            .or_else(|| grpc_status_from_headers(&stream.response_headers))
        {
            effects.push(emit_fact(ProtocolFact::GrpcFinalStatusReceived {
                connection: self.connection_fact_id(),
                stream: tina_runtime::GrpcStreamId::new(stream_id as u64),
                status,
            }));
        }
        effects.push(emit_fact(ProtocolFact::Http2StreamClosed {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            reason: Http2CloseReason::EndStream,
        }));
        if let Some(pull) = stream.response_pull.take() {
            effects.push(reply_to_request::<Self>(
                pull,
                Http2ClientReply::ResponseChunk {
                    stream_id,
                    chunk: Http2ResponseChunk::End {
                        trailers: std::mem::take(&mut stream.response_trailers),
                    },
                },
            ));
        }
    }

    /// Reply a terminal outcome to whichever caller channel a torn-down
    /// stream still holds. A buffered stream — or a streamed-response
    /// stream whose `ResponseStreaming` head was never delivered — replies
    /// on its `waiter` as an `Outcome`. A streamed-response stream that
    /// already delivered its head and has a caller parked on
    /// `ResponseNext` replies on `response_pull` as a terminal
    /// `ResponseChunk`. A stream with neither live channel has nothing to
    /// notify; the caller's next `ResponseNext` finds the slot gone and
    /// gets `Closed`.
    fn settle_stream_terminal(
        &self,
        stream: &mut ActiveClientStream,
        outcome: Http2ClientOutcome,
        effects: &mut Vec<Effect<Self>>,
    ) {
        let stream_id = stream.id;
        if let Some(waiter) = stream.waiter.take() {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome { stream_id, outcome },
            ));
        } else if let Some(pull) = stream.response_pull.take() {
            effects.push(reply_to_request::<Self>(
                pull,
                Http2ClientReply::ResponseChunk {
                    stream_id,
                    chunk: response_chunk_from_outcome(outcome),
                },
            ));
        }
    }

    /// Issue a `ResponseChunkMsg::Next` pull for any streaming stream that
    /// needs more body and has none buffered (one chunk in flight at a
    /// time — natural backpressure). Buffered streams are skipped.
    fn pump_request_pulls(&mut self, effects: &mut Vec<Effect<Self>>) {
        let timeout = self.limits.tls_io_timeout;
        for stream in &mut self.streams {
            let Some(source) = stream.request_source else {
                continue;
            };
            if stream.request_complete || stream.request_pull_in_flight || stream.has_outbound() {
                continue;
            }
            stream.request_pull_in_flight = true;
            let stream_id = stream.id;
            effects.push(
                call(source, ResponseChunkMsg::Next, timeout)
                    .then(move |outcome| Http2ClientMsg::RequestChunk { stream_id, outcome }),
            );
        }
    }

    /// Tell a streaming request body source to stop, when the connection
    /// is done pulling from it — the stream completed, reset, was
    /// cancelled, or the connection is closing. Per the chunk-source
    /// contract a source releases its resources on `Cancel`, and
    /// `IterBodySource` only stops then (a clean `Eof` reply leaves it
    /// alive), so a dropped-but-not-cancelled source would orphan. The
    /// reply is absorbed by `RequestSourceCancelled` and ignored;
    /// duplicate/late cancels are harmless by contract. No-op for a
    /// buffered request (no source).
    fn cancel_request_source(&self, stream: &ActiveClientStream, effects: &mut Vec<Effect<Self>>) {
        if let Some(source) = stream.request_source {
            effects.push(
                call(source, ResponseChunkMsg::Cancel, self.limits.tls_io_timeout)
                    .then(|_| Http2ClientMsg::RequestSourceCancelled),
            );
        }
    }

    /// Drain queued outbound DATA from each active stream subject to
    /// stream and connection send-window credit. Called after admission,
    /// after handling a peer WINDOW_UPDATE, and after settings that
    /// resized the initial window. Idempotent; safe to over-call.
    fn flush_outbound_data(&mut self) {
        let max_chunk = self.limits.max_frame_size.min(self.peer_max_frame_size);
        // Round-robin over streams in admission order. Streams with no
        // outbound body are skipped without "blocked" accounting. When the
        // connection window is exhausted (or the peer's max frame size is
        // 0) we cannot drain payload bytes, but we still fall through to the
        // END_STREAM block below: an empty DATA(END_STREAM) carries no
        // payload and so consumes no flow-control credit.
        let mut progressed = max_chunk > 0;
        while progressed && self.send_window > 0 {
            progressed = false;
            for idx in 0..self.streams.len() {
                if self.send_window <= 0 {
                    break;
                }
                let stream = &mut self.streams[idx];
                if !stream.has_outbound() {
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
                let credit = credit.min(stream.outbound_remaining());
                if credit == 0 {
                    continue;
                }
                // END_STREAM only once the whole request body is known
                // (buffered, or a streaming source that hit `Eof`) and this
                // chunk drains the remaining bytes.
                let is_last = credit == stream.outbound_remaining() && stream.request_complete;
                // Frame the DATA payload directly: 9-byte header then the
                // unsent body slice. One copy here, versus the old per-byte
                // `pop_front` loop plus a re-encode in `Frame::encode`.
                let mut framed = Vec::with_capacity(FRAME_HEADER_LEN + credit);
                push_frame_header(
                    &mut framed,
                    FRAME_DATA,
                    if is_last { FLAG_END_STREAM } else { 0 },
                    stream.id,
                    credit,
                );
                framed.extend_from_slice(stream.outbound_slice(credit));
                stream.advance_outbound(credit);
                if is_last {
                    stream.request_end_sent = true;
                }
                let n = credit as i32;
                stream.send_window -= n;
                self.send_window -= n;
                self.enqueue_bytes(framed);
                progressed = true;
            }
        }
        // A streaming request that ended with no trailing bytes (its last
        // pulled chunk already flushed, then `Eof`) still needs an
        // END_STREAM. Emit an empty DATA(END_STREAM) for those.
        let mut pending_end: Vec<u32> = Vec::new();
        for stream in &self.streams {
            if stream.request_complete && !stream.has_outbound() && !stream.request_end_sent {
                pending_end.push(stream.id);
            }
        }
        for stream_id in pending_end {
            if let Some(stream) = self.streams.iter_mut().find(|s| s.id == stream_id) {
                stream.request_end_sent = true;
            }
            self.enqueue_frame(data_frame(stream_id, true, Vec::new()));
        }
    }

    /// Locally cancel an admitted stream: send RST_STREAM(CANCEL), reply
    /// to the original submitter with `LocalCancel`, free the slot.
    fn handle_cancel(&mut self, stream_id: u32) -> Effect<Self> {
        let Some(idx) = self.find_stream(stream_id) else {
            return noop();
        };
        let mut effects: Vec<Effect<Self>> = Vec::new();
        let mut stream = self.swap_remove_stream_at(idx);
        self.cancel_request_source(&stream, &mut effects);
        self.enqueue_frame(rst_stream_frame(stream_id, ERR_CANCEL));
        self.report.locally_cancelled += 1;
        self.report.closed_streams += 1;
        effects.push(emit_fact(ProtocolFact::Http2StreamReset {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            direction: ProtocolDirection::Outbound,
            reason: Http2ResetReason::Cancel,
        }));
        self.settle_stream_terminal(&mut stream, Http2ClientOutcome::LocalCancel, &mut effects);
        self.pump_io(&mut effects);
        batch(effects)
    }

    fn handle_read(&mut self, reply: TcpReadBufReply) -> Effect<Self> {
        // This read completed; free the read lane before pumping.
        self.read_in_flight = false;
        let TcpReadBufReply { buffer, len } = reply;
        if len == 0 {
            self.read_scratch = buffer;
            return self.close_with(Http2ClientOutcome::Closed);
        }
        self.read_buf.extend_from_slice(&buffer[..len]);
        self.read_scratch = buffer;
        let mut effects: Vec<Effect<Self>> = Vec::new();
        let max_frame_size = self.limits.max_frame_size;
        loop {
            let meta = match try_decode_frame_meta(&self.read_buf, max_frame_size) {
                Ok(Some(meta)) => meta,
                Ok(None) => break,
                Err(err) => return self.protocol_error(err, effects),
            };
            let body = FRAME_HEADER_LEN..meta.total;
            let result = if meta.ty == FRAME_DATA {
                // DATA is buffered for the caller to pull and must outlive the
                // read buffer, so it owns a fresh payload `Vec`.
                let payload = self.read_buf[body].to_vec();
                self.read_buf.drain(..meta.total);
                let frame = Frame {
                    ty: meta.ty,
                    flags: meta.flags,
                    stream_id: meta.stream_id,
                    payload,
                };
                self.handle_data(frame, &mut effects)
            } else {
                // Inline frames are decoded now and discarded, so they reuse one
                // connection-owned scratch buffer instead of allocating per frame.
                let mut scratch = std::mem::take(&mut self.frame_scratch);
                scratch.clear();
                scratch.extend_from_slice(&self.read_buf[body]);
                self.read_buf.drain(..meta.total);
                let result = self.handle_inline_frame(
                    meta.ty,
                    meta.flags,
                    meta.stream_id,
                    &scratch,
                    &mut effects,
                );
                self.frame_scratch = scratch;
                result
            };
            if let Err(err) = result {
                return self.protocol_error(err, effects);
            }
        }
        if self.pending_recv_window_credit >= WINDOW_CREDIT_FLUSH_THRESHOLD {
            let credit = self.pending_recv_window_credit;
            self.pending_recv_window_credit = 0;
            self.recv_window = self.recv_window.saturating_add(credit as i32);
            self.enqueue_frame(window_update_frame(0, credit));
        }
        // A WINDOW_UPDATE in this batch may have freed send-window credit
        // and drained a streaming request's outbound body; pull the next
        // chunk from any source that is now idle.
        self.pump_request_pulls(&mut effects);
        self.pump_io(&mut effects);
        batch(effects)
    }

    /// Dispatch an inline (non-DATA) frame from a borrowed payload. DATA is
    /// handled separately on an owned path because it is queued for the caller.
    fn handle_inline_frame(
        &mut self,
        ty: u8,
        flags: u8,
        stream_id: u32,
        payload: &[u8],
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        match ty {
            FRAME_SETTINGS => self.handle_settings(flags, stream_id, payload),
            FRAME_HEADERS => self.handle_headers(flags, stream_id, payload, effects),
            FRAME_WINDOW_UPDATE => self.handle_window_update(stream_id, payload),
            FRAME_RST_STREAM => self.handle_rst_stream(stream_id, payload, effects),
            FRAME_PING => self.handle_ping(flags, stream_id, payload),
            FRAME_GOAWAY => self.handle_goaway(stream_id, payload, effects),
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
        stream_id: u32,
        payload: &[u8],
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if stream_id != 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        // last_stream_id (4) + error code (4); additional debug data is
        // allowed and ignored.
        if payload.len() < 8 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        let last_stream_id =
            u32::from_be_bytes([payload[0] & 0x7f, payload[1], payload[2], payload[3]]);
        let error_code = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
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
                self.fail_stream(
                    idx,
                    refused_outcome.clone(),
                    Http2CloseReason::GoAway,
                    effects,
                );
                // fail_stream swap_removes, so do not advance idx.
            } else {
                idx += 1;
            }
        }
        Ok(())
    }

    fn handle_settings(
        &mut self,
        flags: u8,
        stream_id: u32,
        payload: &[u8],
    ) -> Result<(), Http2ProtocolError> {
        if stream_id != 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        if flags & FLAG_ACK != 0 {
            if !payload.is_empty() {
                return Err(Http2ProtocolError::BadFrameLength);
            }
            return Ok(());
        }
        if payload.len() % 6 != 0 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        for setting in payload.chunks_exact(6) {
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
        flags: u8,
        stream_id: u32,
        frame_payload: &[u8],
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if stream_id == 0 || stream_id % 2 == 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        if flags & FLAG_END_HEADERS == 0 {
            return Err(Http2ProtocolError::HpackUnsupported);
        }
        let Some(idx) = self.find_stream(stream_id) else {
            return Err(Http2ProtocolError::BadStreamId);
        };
        let payload = headers_payload_view(flags, frame_payload)?;
        // A gRPC-unary stream decodes its response compactly: gRPC facts only,
        // no public `HeaderMap`. Every generic stream keeps the public decode.
        let grpc_unary = self.streams[idx].grpc_unary;
        // Responses never carry `:path`, so the client needs no path interner.
        let header_block = if grpc_unary {
            decode_headers_block_compact_with(
                &mut self.hpack_decoder,
                payload,
                self.limits.max_header_bytes,
                None,
            )?
        } else {
            decode_headers_block_with(
                &mut self.hpack_decoder,
                payload,
                self.limits.max_header_bytes,
                None,
            )?
        };
        let end_stream = flags & FLAG_END_STREAM != 0;
        if !self.streams[idx].response_headers_seen {
            validate_response_headers(&header_block)?;
            if grpc_unary {
                apply_grpc_response_head(&mut self.streams[idx], header_block);
            } else {
                apply_response_headers(&mut self.streams[idx], header_block);
            }
            self.streams[idx].response_headers_seen = true;
            // For a streamed response, deliver the head to the OpenStream
            // waiter now; the body is pulled afterwards. gRPC-unary streams are
            // buffered, so this never fires for them.
            if self.streams[idx].response_streamed && !self.streams[idx].response_head_sent {
                self.send_response_head(idx, effects);
            }
        } else {
            // RFC 9113 §8.1: trailers must arrive with END_STREAM and
            // must not contain pseudo-headers, `content-length`, or
            // connection-control headers.
            if !end_stream {
                return Err(Http2ProtocolError::InvalidTrailerPseudoHeader);
            }
            validate_trailer_block(&header_block)?;
            if grpc_unary {
                apply_grpc_response_trailers(&mut self.streams[idx], header_block);
            } else {
                for (name, value) in header_block.headers.iter() {
                    self.streams[idx]
                        .response_trailers
                        .append(name.clone(), value.clone());
                }
            }
        }
        if end_stream {
            if self.streams[idx].response_streamed {
                self.streams[idx].response_eof = true;
                self.deliver_to_parked_pull(idx, effects);
            } else {
                self.complete_stream(idx, effects);
            }
        }
        Ok(())
    }

    /// Reply the `ResponseStreaming` head (status + headers) to the
    /// `OpenStream` waiter and mark it sent.
    fn send_response_head(&mut self, idx: usize, effects: &mut Vec<Effect<Self>>) {
        let stream_id = self.streams[idx].id;
        let status = self.streams[idx].response_status.unwrap_or(StatusCode::OK);
        let headers = self.streams[idx].response_headers.clone();
        self.streams[idx].response_head_sent = true;
        if let Some(waiter) = self.streams[idx].waiter.take() {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome {
                    stream_id,
                    outcome: Http2ClientOutcome::ResponseStreaming { status, headers },
                },
            ));
        }
        self.arm_response_idle_timeout(stream_id, effects);
    }

    fn handle_data(
        &mut self,
        frame: Frame,
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        if frame.stream_id == 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        // Move the unpadded payload out of the owned frame instead of cloning
        // it. `flow_len` is the full on-wire payload length (pad-length byte +
        // padding included); RFC 9113 §6.9.1 counts the *whole* DATA payload
        // against both the connection and stream windows. `payload_len` is the
        // unpadded application length, used for body caps and content-length.
        let stream_id = frame.stream_id;
        let end_stream = frame.flags & FLAG_END_STREAM != 0;
        let (payload, flow_len) = into_data_payload(frame)?;
        let payload_len = payload.len();
        let flow_i32 = i32::try_from(flow_len).map_err(|_| Http2ProtocolError::FlowControl)?;
        if self.recv_window < flow_i32 {
            self.report.flow_control_blocked += 1;
            return Err(Http2ProtocolError::FlowControl);
        }
        // Always count DATA on the connection window per RFC 9113 §6.9.1 by the
        // full wire length, even for closed streams; the same amount is credited
        // back (batched in handle_read).
        self.recv_window -= flow_i32;
        self.pending_recv_window_credit = self
            .pending_recv_window_credit
            .saturating_add(flow_len as u32);
        let Some(idx) = self.find_stream(stream_id) else {
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
        if self.streams[idx].recv_window < flow_i32 {
            self.report.flow_control_blocked += 1;
            return Err(Http2ProtocolError::FlowControl);
        }
        self.streams[idx].recv_window -= flow_i32;
        // Padding is consumed off the wire here and never handed to the caller,
        // so return its stream-window credit to the peer immediately. Only the
        // unpadded `payload_len` stays debited as the consumer-backpressure
        // lever (returned on consume for streamed responses, or batched below
        // for buffered ones). A no-op for the common unpadded frame.
        let padding = flow_len - payload_len;
        if padding > 0 {
            self.streams[idx].recv_window =
                self.streams[idx].recv_window.saturating_add(padding as i32);
            self.enqueue_frame(window_update_frame(stream_id, padding as u32));
        }
        // Streamed response: hold the per-stream credit (the backpressure
        // lever) and buffer the chunk for the caller to pull. There is no
        // total-body cap — the stream window bounds resident bytes, and
        // per-stream credit is only returned as the caller consumes (see
        // `deliver_to_parked_pull`). The connection window credited above
        // (batched at `WINDOW_CREDIT_FLUSH_THRESHOLD` in `handle_read`) is
        // deliberately not the backpressure lever: it is shared across
        // streams, so holding it would stall every other stream. A slow
        // consumer is bounded purely by this stream's window.
        if self.streams[idx].response_streamed {
            self.streams[idx].response_body_received += payload_len;
            if !payload.is_empty() {
                self.streams[idx].response_chunks.push_back(payload);
            }
            if end_stream {
                self.streams[idx].response_eof = true;
            }
            self.deliver_to_parked_pull(idx, effects);
            return Ok(());
        }
        if self.streams[idx].response_body.len() + payload_len > self.limits.max_response_body_bytes
        {
            let cap_bytes = self.limits.max_response_body_bytes;
            self.enqueue_frame(rst_stream_frame(stream_id, ERR_PROTOCOL_ERROR));
            // We sent the RST_STREAM, so this is a local close.
            self.fail_stream(
                idx,
                Http2ClientOutcome::ProtocolError(Http2ProtocolError::BodyTooLarge { cap_bytes }),
                Http2CloseReason::LocalCloseOnly,
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
        if end_stream {
            if let Some(declared) = self.streams[idx].response_content_length {
                if declared != self.streams[idx].response_body.len() {
                    return Err(Http2ProtocolError::ContentLengthMismatch);
                }
            }
            self.complete_stream(idx, effects);
        }
        Ok(())
    }

    fn handle_window_update(
        &mut self,
        stream_id: u32,
        payload: &[u8],
    ) -> Result<(), Http2ProtocolError> {
        if payload.len() != 4 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        let increment = u32::from_be_bytes([payload[0] & 0x7f, payload[1], payload[2], payload[3]]);
        if increment == 0 {
            return Err(Http2ProtocolError::WindowOverflow);
        }
        if stream_id == 0 {
            self.send_window = add_window(self.send_window, increment)?;
        } else if let Some(idx) = self.find_stream(stream_id) {
            self.streams[idx].send_window = add_window(self.streams[idx].send_window, increment)?;
        }
        // New credit may unblock parked outbound DATA on any stream.
        self.flush_outbound_data();
        Ok(())
    }

    fn handle_rst_stream(
        &mut self,
        stream_id: u32,
        payload: &[u8],
        effects: &mut Vec<Effect<Self>>,
    ) -> Result<(), Http2ProtocolError> {
        // RFC 9113 §6.4: RST_STREAM MUST be associated with a stream; a
        // RST_STREAM on stream 0x0 is a connection-level PROTOCOL_ERROR.
        if stream_id == 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        if payload.len() != 4 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        let code = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let Some(idx) = self.find_stream(stream_id) else {
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
        // Peer-initiated RST_STREAM: the remote closed this stream.
        self.fail_stream(
            idx,
            Http2ClientOutcome::Reset(reason),
            Http2CloseReason::RemoteCloseOnly,
            effects,
        );
        Ok(())
    }

    fn handle_ping(
        &mut self,
        flags: u8,
        stream_id: u32,
        payload: &[u8],
    ) -> Result<(), Http2ProtocolError> {
        // RFC 9113 §6.7: PING is sent on stream 0x0 (a non-zero stream id
        // is a connection-level PROTOCOL_ERROR) and carries exactly 8
        // octets of opaque data (any other length is FRAME_SIZE_ERROR).
        // Distinguish the two so replay/observability sees the right
        // cause rather than collapsing both into BadFrameLength.
        if stream_id != 0 {
            return Err(Http2ProtocolError::BadStreamId);
        }
        if payload.len() != 8 {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        if flags & FLAG_ACK == 0 {
            // Reflect the opaque data back; PING is rare, so this owns a copy.
            self.enqueue_frame(Frame::new(FRAME_PING, FLAG_ACK, 0, payload.to_vec()));
        }
        Ok(())
    }

    // A "trailers-only" gRPC response (HEADERS with END_STREAM and no
    // DATA — used to carry a non-OK status with no message frame)
    // arrives here with `response_headers_seen` set on the first HEADERS
    // and an empty body; its `grpc-status` lands in `response_headers`.
    // A normal gRPC response carries `grpc-status` in `response_trailers`.
    // We check both so the `GrpcFinalStatusReceived` protocol fact fires
    // either way — the receive-side mirror of the server's
    // `GrpcFinalStatusSent`.
    fn complete_stream(&mut self, idx: usize, effects: &mut Vec<Effect<Self>>) {
        let mut stream = self.swap_remove_stream_at(idx);
        let stream_id = stream.id;
        // The response is done. If this stream streamed its request body,
        // the source may still be alive (it returned `Eof`, or the peer
        // responded before we finished sending) — tell it to stop.
        self.cancel_request_source(&stream, effects);
        self.report.closed_streams += 1;
        effects.push(emit_fact(ProtocolFact::Http2StreamClosed {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            reason: Http2CloseReason::EndStream,
        }));
        // The final gRPC status fact comes from compact facts on a gRPC-unary
        // stream, or from the public trailer/header maps otherwise.
        let grpc_final = if stream.grpc_unary {
            stream.grpc_status.map(grpc_status_code_from_wire)
        } else {
            grpc_status_from_headers(&stream.response_trailers)
                .or_else(|| grpc_status_from_headers(&stream.response_headers))
        };
        if let Some(status) = grpc_final {
            effects.push(emit_fact(ProtocolFact::GrpcFinalStatusReceived {
                connection: self.connection_fact_id(),
                stream: tina_runtime::GrpcStreamId::new(stream_id as u64),
                status,
            }));
        }
        let outcome = match stream.response_status {
            Some(status) if stream.grpc_unary => Http2ClientOutcome::GrpcUnaryReplied {
                status,
                grpc_status: stream.grpc_status,
                grpc_message: stream.grpc_message,
                body: stream.response_body,
            },
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

    /// Fail one stream with a typed outcome and an explicit close
    /// reason. The close reason is supplied by the caller rather than
    /// derived from the outcome, because the same outcome can come from
    /// different causes (e.g. `Reset(reason)` is produced both by a peer
    /// RST_STREAM — a `RemoteCloseOnly` — and by an error-coded GOAWAY —
    /// a `GoAway`), and replay correlation needs the precise cause.
    fn fail_stream(
        &mut self,
        idx: usize,
        outcome: Http2ClientOutcome,
        close_reason: Http2CloseReason,
        effects: &mut Vec<Effect<Self>>,
    ) {
        let mut stream = self.swap_remove_stream_at(idx);
        let stream_id = stream.id;
        self.cancel_request_source(&stream, effects);
        self.report.closed_streams += 1;
        effects.push(emit_fact(ProtocolFact::Http2StreamClosed {
            connection: self.connection_fact_id(),
            stream: Http2StreamId::new(stream_id),
            reason: close_reason,
        }));
        self.settle_stream_terminal(&mut stream, outcome, effects);
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
        let queued_grpc_unary = std::mem::take(&mut self.queued_grpc_unary);
        for (_, waiter) in queued_grpc_unary {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome {
                    stream_id: 0,
                    outcome: outcome.clone(),
                },
            ));
        }
        let queued_streaming = std::mem::take(&mut self.queued_streaming);
        for (_, waiter) in queued_streaming {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome {
                    stream_id: 0,
                    outcome: outcome.clone(),
                },
            ));
        }
        let queued_open = std::mem::take(&mut self.queued_open);
        for (_, waiter) in queued_open {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome {
                    stream_id: 0,
                    outcome: outcome.clone(),
                },
            ));
        }
        let streams: Vec<_> = std::mem::take(&mut self.streams);
        self.stream_index.clear();
        for mut stream in streams {
            self.cancel_request_source(&stream, &mut effects);
            self.settle_stream_terminal(&mut stream, outcome.clone(), &mut effects);
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
        let streams: Vec<_> = std::mem::take(&mut self.streams);
        self.stream_index.clear();
        for mut stream in streams {
            self.cancel_request_source(&stream, &mut effects);
            self.settle_stream_terminal(
                &mut stream,
                Http2ClientOutcome::ProtocolError(err.clone()),
                &mut effects,
            );
        }
        self.closing_after_write = true;
        self.pump_io(&mut effects);
        batch(effects)
    }

    fn handle_wrote(&mut self, reply: TcpWriteOwnedReply) -> Effect<Self> {
        // Wrote completion: drain the bytes we know flushed, clear the
        // in-flight flag, then let `pump_io` schedule the next write — or,
        // on the half-duplex TLS rail, arm the response read now that the
        // write lane is free.
        self.write_in_flight = false;
        let TcpWriteOwnedReply { mut bytes, written } = reply;
        let drain = written.min(bytes.len());
        bytes.drain(..drain);
        self.pending_write = bytes;
        if self.pending_write.is_empty() {
            self.promote_queued_write();
        }
        let mut effects: Vec<Effect<Self>> = Vec::new();
        self.pump_io(&mut effects);
        batch(effects)
    }

    fn enqueue_frame(&mut self, frame: Frame) {
        self.enqueue_bytes(frame.encode());
    }

    /// Queue already-encoded frame bytes.
    ///
    /// When no write is in flight, append to the current pending write buffer
    /// instead of creating a second tiny TCP write. HTTP/2 frame boundaries are
    /// preserved inside the byte stream, but buffered request HEADERS + DATA can
    /// ride one kernel write. This matters on Linux: separate small writes can
    /// trip delayed-ACK/Nagle pacing and add tens of milliseconds to otherwise
    /// local requests.
    fn enqueue_bytes(&mut self, mut bytes: Vec<u8>) {
        if self.write_in_flight {
            self.write_queue.push_back(bytes);
        } else if self.pending_write.is_empty() {
            self.pending_write = bytes;
        } else if self.pending_write.len() + bytes.len() <= self.peer_max_frame_size {
            self.pending_write.append(&mut bytes);
        } else {
            self.write_queue.push_back(bytes);
        }
    }

    fn promote_queued_write(&mut self) {
        if !self.pending_write.is_empty() {
            return;
        }
        let Some(mut next) = self.write_queue.pop_front() else {
            return;
        };
        while matches!(
            self.write_queue.front(),
            Some(more) if next.len() + more.len() <= self.peer_max_frame_size
        ) {
            let mut more = self
                .write_queue
                .pop_front()
                .expect("front checked before pop");
            next.append(&mut more);
        }
        self.pending_write = next;
    }

    fn full_duplex(&self) -> bool {
        matches!(self.stream, Some(ClientStream::Tcp(_)))
    }

    /// Single entry point that advances connection IO. The TCP rail is
    /// full duplex (separate read/write lanes, poll-based driver): keep a
    /// read armed and write whenever there is outbound work. The TLS rail
    /// shares one lane per stream and is driven by one blocking worker, so
    /// it runs half-duplex — drain outbound writes first, and only arm a
    /// read once nothing is queued to write. This matches every caller's
    /// "I changed the write/read state, now make progress" need without
    /// double-arming a lane (which would be `ResourceBusy`).
    fn pump_io(&mut self, effects: &mut Vec<Effect<Self>>) {
        if self.stream.is_none() {
            return;
        }
        let full_duplex = self.full_duplex();
        let has_outbound = !self.pending_write.is_empty() || !self.write_queue.is_empty();

        if has_outbound && !self.write_in_flight && (full_duplex || !self.read_in_flight) {
            effects.push(self.write_more());
            // On TLS, a write and a read cannot coexist on the shared
            // lane; defer the read until the write drains.
            if !full_duplex {
                return;
            }
        } else if self.closing_after_write && !has_outbound && !self.write_in_flight {
            effects.push(self.close_now());
            return;
        }

        if !self.closing_after_write
            && !self.read_in_flight
            && (full_duplex || (!self.write_in_flight && !has_outbound))
        {
            effects.push(self.read_more());
        }
    }

    fn write_more(&mut self) -> Effect<Self> {
        if self.write_in_flight {
            // Another path already armed `tcp_write`; the eventual
            // `Wrote(...)` completion will re-enter this function.
            return noop();
        }
        if self.pending_write.is_empty() {
            self.promote_queued_write();
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
        let bytes = std::mem::take(&mut self.pending_write);
        match stream {
            ClientStream::Tcp(s) => tcp_write_owned(s, bytes)
                .then(|result| Http2ClientMsg::Wrote(result.map_err(|error| error.error))),
            ClientStream::Tls(s) => {
                tls_write_owned(s, bytes, self.limits.tls_io_timeout).then(|result| {
                    Http2ClientMsg::Wrote(
                        result
                            .map(tls_write_reply_to_tcp)
                            .map_err(|error| error.error),
                    )
                })
            }
        }
    }

    fn read_more(&mut self) -> Effect<Self> {
        let Some(stream) = self.stream else {
            return noop();
        };
        self.read_in_flight = true;
        let buffer = std::mem::take(&mut self.read_scratch);
        match stream {
            ClientStream::Tcp(s) => tcp_read_buf(s, buffer, READ_CHUNK)
                .then(|result| Http2ClientMsg::Read(result.map_err(|error| error.error))),
            ClientStream::Tls(s) => tls_read_buf(s, buffer, READ_CHUNK, self.limits.tls_io_timeout)
                .then(|result| {
                    Http2ClientMsg::Read(
                        result
                            .map(tls_read_reply_to_tcp)
                            .map_err(|error| error.error),
                    )
                }),
        }
    }

    fn close_with(&mut self, outcome: Http2ClientOutcome) -> Effect<Self> {
        let mut effects: Vec<Effect<Self>> = Vec::new();
        let streams: Vec<_> = std::mem::take(&mut self.streams);
        self.stream_index.clear();
        for mut stream in streams {
            self.cancel_request_source(&stream, &mut effects);
            self.settle_stream_terminal(&mut stream, outcome.clone(), &mut effects);
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
        let queued_grpc_unary = std::mem::take(&mut self.queued_grpc_unary);
        for (_, waiter) in queued_grpc_unary {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome {
                    stream_id: 0,
                    outcome: outcome.clone(),
                },
            ));
        }
        let queued_streaming = std::mem::take(&mut self.queued_streaming);
        for (_, waiter) in queued_streaming {
            effects.push(reply_to_request::<Self>(
                waiter,
                Http2ClientReply::Outcome {
                    stream_id: 0,
                    outcome: outcome.clone(),
                },
            ));
        }
        let queued_open = std::mem::take(&mut self.queued_open);
        for (_, waiter) in queued_open {
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
        match stream {
            ClientStream::Tcp(s) => tcp_close_stream(s).then(Http2ClientMsg::Closed),
            ClientStream::Tls(s) => {
                tls_close(s, self.limits.tls_io_timeout).then(Http2ClientMsg::Closed)
            }
        }
    }

    fn begin_goaway_shutdown(&mut self) -> Effect<Self> {
        if self.stream.is_none() {
            return self.fail_all(Http2ClientOutcome::Closed);
        }
        self.enqueue_frame(goaway_frame(self.next_stream_id, ERR_NO_ERROR));
        self.closing_after_write = true;
        let mut effects: Vec<Effect<Self>> = Vec::new();
        self.pump_io(&mut effects);
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

fn encode_request_headers(target: &Http2Target, req: &Http2ClientRequest) -> Vec<u8> {
    encode_request_header_block(target, &req.method, &req.path, &req.headers)
}

fn encode_grpc_unary_request_header_block(target: &Http2Target, path: &str) -> Vec<u8> {
    let mut block = Vec::new();
    encode_literal_header(":method", Method::POST.as_str(), &mut block);
    let scheme = if target.is_tls() { "https" } else { "http" };
    encode_literal_header(":scheme", scheme, &mut block);
    encode_literal_header(":path", path, &mut block);
    encode_literal_header(":authority", target.authority(), &mut block);
    encode_literal_header("content-type", "application/grpc+proto", &mut block);
    encode_literal_header("te", "trailers", &mut block);
    block
}

fn encode_request_header_block(
    target: &Http2Target,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
) -> Vec<u8> {
    let mut block = Vec::new();
    encode_literal_header(":method", method.as_str(), &mut block);
    let scheme = if target.is_tls() { "https" } else { "http" };
    encode_literal_header(":scheme", scheme, &mut block);
    encode_literal_header(":path", path, &mut block);
    encode_literal_header(":authority", target.authority(), &mut block);
    for (name, value) in headers.iter() {
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

/// Apply a gRPC-unary response head from compact facts: HTTP status, declared
/// length, and any `grpc-status`/`grpc-message` (a trailers-only error response
/// carries the status here). The owned message is moved out of the block, so no
/// public `HeaderMap` is built and the message is not cloned.
fn apply_grpc_response_head(stream: &mut ActiveClientStream, header_block: HeaderBlock) {
    stream.response_status = header_block.status;
    stream.response_content_length = header_block.content_length;
    if header_block.grpc_status.is_some() {
        stream.grpc_status = header_block.grpc_status;
    }
    if header_block.grpc_message.is_some() {
        stream.grpc_message = header_block.grpc_message;
    }
}

/// Capture `grpc-status`/`grpc-message` facts from a gRPC-unary trailer block.
fn apply_grpc_response_trailers(stream: &mut ActiveClientStream, header_block: HeaderBlock) {
    if header_block.grpc_status.is_some() {
        stream.grpc_status = header_block.grpc_status;
    }
    if header_block.grpc_message.is_some() {
        stream.grpc_message = header_block.grpc_message;
    }
}

fn emit_fact<S: Shard + 'static>(fact: ProtocolFact) -> Effect<Http2ClientConnection<S>> {
    tina::fact::<Http2ClientConnection<S>>(fact)
}

/// Map a teardown `Http2ClientOutcome` to the terminal `Http2ResponseChunk`
/// delivered to a parked `ResponseNext` pull. `Reset`/`ProtocolError`
/// carry their reason; everything else (a closed connection, local
/// cancel, …) collapses to `Closed` — the stream is gone either way.
fn response_chunk_from_outcome(outcome: Http2ClientOutcome) -> Http2ResponseChunk {
    match outcome {
        Http2ClientOutcome::Reset(reason) => Http2ResponseChunk::Reset(reason),
        Http2ClientOutcome::ProtocolError(err) => Http2ResponseChunk::ProtocolError(err),
        _ => Http2ResponseChunk::Closed,
    }
}

/// Read the trace-stable `grpc-status` code from a header/trailer map,
/// if present. Returns `None` when there is no `grpc-status` (i.e. this
/// is not a gRPC response), so the HTTP/2 layer only emits a gRPC fact
/// for actual gRPC traffic.
fn grpc_status_from_headers(headers: &HeaderMap) -> Option<tina_runtime::GrpcStatusCode> {
    let value = headers.get("grpc-status")?;
    let code: u16 = value.to_str().ok()?.trim().parse().ok()?;
    Some(grpc_status_code_from_wire(code))
}

/// Maps a numeric gRPC status code to the trace-stable runtime enum.
/// Unknown codes fold to `Unknown`, matching the gRPC spec's guidance
/// for unrecognized status values.
fn grpc_status_code_from_wire(code: u16) -> tina_runtime::GrpcStatusCode {
    use tina_runtime::GrpcStatusCode as C;
    match code {
        0 => C::Ok,
        1 => C::Cancelled,
        2 => C::Unknown,
        3 => C::InvalidArgument,
        4 => C::DeadlineExceeded,
        5 => C::NotFound,
        6 => C::AlreadyExists,
        7 => C::PermissionDenied,
        8 => C::ResourceExhausted,
        9 => C::FailedPrecondition,
        10 => C::Aborted,
        11 => C::OutOfRange,
        12 => C::Unimplemented,
        13 => C::Internal,
        14 => C::Unavailable,
        15 => C::DataLoss,
        16 => C::Unauthenticated,
        _ => C::Unknown,
    }
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
        // Without this property, connection pooling would share a
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
    fn idle_client_coalesces_ready_frames_into_one_pending_write() {
        let target = Http2Target::H2c {
            authority: "x".into(),
            addr: (Ipv4Addr::LOCALHOST, 80).into(),
        };
        let mut client =
            Http2ClientConnection::<tina::SingleShard>::new(target, Http2ClientLimits::default());

        client.enqueue_frame(headers_frame(1, false, b"headers".to_vec()));
        client.enqueue_frame(data_frame(1, true, b"body".to_vec()));

        assert!(
            client.pending_write.len() > FRAME_HEADER_LEN * 2,
            "coalesced write carries both frame headers and payloads"
        );
        assert!(
            client.write_queue.is_empty(),
            "idle HEADERS + DATA should not become two tiny TCP writes"
        );
        let (first, used) = try_decode_frame(&client.pending_write, client.limits.max_frame_size)
            .expect("decode first frame")
            .expect("first frame");
        assert_eq!(first.ty, FRAME_HEADERS);
        let (second, used2) =
            try_decode_frame(&client.pending_write[used..], client.limits.max_frame_size)
                .expect("decode second frame")
                .expect("second frame");
        assert_eq!(second.ty, FRAME_DATA);
        assert_eq!(used + used2, client.pending_write.len());
    }

    #[test]
    fn completed_write_promotes_queued_frames_as_one_buffer() {
        let target = Http2Target::H2c {
            authority: "x".into(),
            addr: (Ipv4Addr::LOCALHOST, 80).into(),
        };
        let mut client =
            Http2ClientConnection::<tina::SingleShard>::new(target, Http2ClientLimits::default());
        client.write_in_flight = true;
        client
            .write_queue
            .push_back(Frame::new(FRAME_SETTINGS, 0, 0, Vec::new()).encode());
        client
            .write_queue
            .push_back(data_frame(1, true, b"body".to_vec()).encode());

        let _ = client.handle_wrote(TcpWriteOwnedReply {
            bytes: b"prior".to_vec(),
            written: 5,
        });

        assert!(
            client.write_queue.is_empty(),
            "queued ready frames should be promoted together"
        );
        let (first, used) = try_decode_frame(&client.pending_write, client.limits.max_frame_size)
            .expect("decode first frame")
            .expect("first frame");
        assert_eq!(first.ty, FRAME_SETTINGS);
        let (second, used2) =
            try_decode_frame(&client.pending_write[used..], client.limits.max_frame_size)
                .expect("decode second frame")
                .expect("second frame");
        assert_eq!(second.ty, FRAME_DATA);
        assert_eq!(used + used2, client.pending_write.len());
    }

    #[test]
    fn coalescing_keeps_large_ready_write_queued() {
        let target = Http2Target::H2c {
            authority: "x".into(),
            addr: (Ipv4Addr::LOCALHOST, 80).into(),
        };
        let mut client =
            Http2ClientConnection::<tina::SingleShard>::new(target, Http2ClientLimits::default());

        let frame_cap = client.peer_max_frame_size;
        client.enqueue_bytes(vec![1; frame_cap]);
        client.enqueue_frame(data_frame(1, true, b"body".to_vec()));

        assert_eq!(client.pending_write.len(), frame_cap);
        assert_eq!(client.write_queue.len(), 1);
    }

    #[test]
    fn outbound_body_append_preserves_unsent_prefix_after_cursor() {
        let mut body = OutboundBody::owned(b"abcdef".to_vec());
        body.advance(2);
        body.append(b"gh");

        assert_eq!(body.remaining(), 6);
        assert_eq!(body.slice(6), b"cdefgh");
    }

    #[test]
    fn wrong_lane_message_is_counted_in_release_path() {
        let target = Http2Target::H2c {
            authority: "x".into(),
            addr: (Ipv4Addr::LOCALHOST, 80).into(),
        };
        let mut client =
            Http2ClientConnection::<tina::SingleShard>::new(target, Http2ClientLimits::default());

        let _ = client.wrong_lane_message();
        assert_eq!(client.report.wrong_lane_messages, 1);

        let _ = client.wrong_lane_message();
        assert_eq!(client.report.wrong_lane_messages, 2);
    }

    #[test]
    #[should_panic(expected = "max_concurrent_streams must be >= 1")]
    fn client_limits_reject_zero_concurrency_in_release_too() {
        let target = Http2Target::H2c {
            authority: "x".into(),
            addr: (Ipv4Addr::LOCALHOST, 80).into(),
        };
        let _ = Http2ClientConnection::<tina::SingleShard>::new(
            target,
            Http2ClientLimits {
                max_concurrent_streams: 0,
                ..Http2ClientLimits::default()
            },
        );
    }

    #[test]
    #[should_panic(expected = "pre_connect_submit_capacity must be >= 1")]
    fn client_limits_reject_zero_pre_connect_queue_in_release_too() {
        let target = Http2Target::H2c {
            authority: "x".into(),
            addr: (Ipv4Addr::LOCALHOST, 80).into(),
        };
        let _ = Http2ClientConnection::<tina::SingleShard>::new(
            target,
            Http2ClientLimits {
                pre_connect_submit_capacity: 0,
                ..Http2ClientLimits::default()
            },
        );
    }

    #[test]
    #[should_panic(expected = "max_frame_size must be in HTTP/2 range")]
    fn client_limits_reject_invalid_frame_size_in_release_too() {
        let target = Http2Target::H2c {
            authority: "x".into(),
            addr: (Ipv4Addr::LOCALHOST, 80).into(),
        };
        let _ = Http2ClientConnection::<tina::SingleShard>::new(
            target,
            Http2ClientLimits {
                max_frame_size: MIN_MAX_FRAME_SIZE as usize - 1,
                ..Http2ClientLimits::default()
            },
        );
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
                Http2ClientOutcome::GrpcUnaryReplied { .. } => "grpc-unary-replied",
                Http2ClientOutcome::ResponseStreaming { .. } => "response-streaming",
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
        assert_eq!(
            classify(&Http2ClientOutcome::GrpcUnaryReplied {
                status: StatusCode::OK,
                grpc_status: Some(0),
                grpc_message: None,
                body: Vec::new(),
            }),
            "grpc-unary-replied"
        );
    }
}
