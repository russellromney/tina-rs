//! Native gRPC first form over Tina HTTP/2.
//!
//! This is intentionally small: unary plus first server/client
//! streaming `prost` messages, typed status, bounded gRPC message
//! frames, h2c prior-knowledge, and real HTTP/2 trailers. It is not
//! tonic and does not include interceptors, compression, or a
//! production pooled client.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, HeaderValue, Method, StatusCode};
use prost::Message;
use tina::prelude::*;
use tina::reply_to;
use tina_runtime::{CallOutcome, call, call_cancelable_request, cancel_call};

use crate::{
    Http2ConnectionReply, Http2RequestParts, Http2RequestStream, Http2ServiceMessage, HttpRequest,
    HttpRequestBody, HttpResponse, HttpResponseBody, RequestChunkReply,
};
use crate::{IterBodySource, ResponseChunkMsg, ResponseChunkReply};

pub(crate) const GRPC_FRAME_HEADER_LEN: usize = 5;
const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_DATA: u8 = 0x0;
const FRAME_HEADERS: u8 = 0x1;
const FRAME_SETTINGS: u8 = 0x4;
const FLAG_ACK: u8 = 0x1;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;
const CLIENT_DATA_FRAME_PAYLOAD: usize = 16 * 1024;
const CLIENT_MAX_INBOUND_FRAME_PAYLOAD: usize = 64 * 1024;
const REQUEST_BODY_PULL_TIMEOUT: Duration = Duration::from_secs(10);

/// Configurable limits for the native gRPC first form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrpcLimits {
    /// Maximum decoded protobuf message bytes for one gRPC message.
    pub max_message_bytes: usize,
    /// Maximum number of messages in one client-streaming request body.
    /// Bounds the decoded `Vec<Req>` count, not just its byte size.
    pub max_messages: usize,
}

impl Default for GrpcLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: 512 * 1024,
            max_messages: 64,
        }
    }
}

/// Service-owned bounds for a finite buffered server-streaming response.
///
/// Use this only for small fixed streams. If the stream can grow with request
/// input, use source-backed server streaming instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrpcBufferedStreamLimits {
    /// Per-message protobuf limit.
    pub message: GrpcLimits,
    /// Maximum number of messages to buffer.
    pub max_messages: usize,
    /// Maximum bytes in the final gRPC-framed response body.
    pub max_body_bytes: usize,
}

impl GrpcBufferedStreamLimits {
    pub fn new(message: GrpcLimits, max_messages: usize, max_body_bytes: usize) -> Self {
        Self {
            message,
            max_messages,
            max_body_bytes,
        }
    }
}

impl Default for GrpcBufferedStreamLimits {
    fn default() -> Self {
        Self {
            message: GrpcLimits::default(),
            max_messages: 64,
            max_body_bytes: 512 * 1024,
        }
    }
}

/// Typed gRPC status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrpcStatusCode {
    Ok,
    Cancelled,
    Unknown,
    InvalidArgument,
    DeadlineExceeded,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    ResourceExhausted,
    FailedPrecondition,
    Aborted,
    OutOfRange,
    Unimplemented,
    Internal,
    Unavailable,
    DataLoss,
    Unauthenticated,
}

impl GrpcStatusCode {
    pub fn as_u16(self) -> u16 {
        match self {
            Self::Ok => 0,
            Self::Cancelled => 1,
            Self::Unknown => 2,
            Self::InvalidArgument => 3,
            Self::DeadlineExceeded => 4,
            Self::NotFound => 5,
            Self::AlreadyExists => 6,
            Self::PermissionDenied => 7,
            Self::ResourceExhausted => 8,
            Self::FailedPrecondition => 9,
            Self::Aborted => 10,
            Self::OutOfRange => 11,
            Self::Unimplemented => 12,
            Self::Internal => 13,
            Self::Unavailable => 14,
            Self::DataLoss => 15,
            Self::Unauthenticated => 16,
        }
    }

    pub fn from_u16(code: u16) -> Self {
        match code {
            0 => Self::Ok,
            1 => Self::Cancelled,
            3 => Self::InvalidArgument,
            4 => Self::DeadlineExceeded,
            5 => Self::NotFound,
            6 => Self::AlreadyExists,
            7 => Self::PermissionDenied,
            8 => Self::ResourceExhausted,
            9 => Self::FailedPrecondition,
            10 => Self::Aborted,
            11 => Self::OutOfRange,
            12 => Self::Unimplemented,
            13 => Self::Internal,
            14 => Self::Unavailable,
            15 => Self::DataLoss,
            16 => Self::Unauthenticated,
            _ => Self::Unknown,
        }
    }
}

/// Maps the local [`GrpcStatusCode`] to the trace-stable
/// [`tina_runtime::GrpcStatusCode`] used by replayable facts.
pub fn classify_grpc_status_code(status: &GrpcStatus) -> tina_runtime::GrpcStatusCode {
    match status.code {
        GrpcStatusCode::Ok => tina_runtime::GrpcStatusCode::Ok,
        GrpcStatusCode::Cancelled => tina_runtime::GrpcStatusCode::Cancelled,
        GrpcStatusCode::Unknown => tina_runtime::GrpcStatusCode::Unknown,
        GrpcStatusCode::InvalidArgument => tina_runtime::GrpcStatusCode::InvalidArgument,
        GrpcStatusCode::DeadlineExceeded => tina_runtime::GrpcStatusCode::DeadlineExceeded,
        GrpcStatusCode::NotFound => tina_runtime::GrpcStatusCode::NotFound,
        GrpcStatusCode::AlreadyExists => tina_runtime::GrpcStatusCode::AlreadyExists,
        GrpcStatusCode::PermissionDenied => tina_runtime::GrpcStatusCode::PermissionDenied,
        GrpcStatusCode::ResourceExhausted => tina_runtime::GrpcStatusCode::ResourceExhausted,
        GrpcStatusCode::FailedPrecondition => tina_runtime::GrpcStatusCode::FailedPrecondition,
        GrpcStatusCode::Aborted => tina_runtime::GrpcStatusCode::Aborted,
        GrpcStatusCode::OutOfRange => tina_runtime::GrpcStatusCode::OutOfRange,
        GrpcStatusCode::Unimplemented => tina_runtime::GrpcStatusCode::Unimplemented,
        GrpcStatusCode::Internal => tina_runtime::GrpcStatusCode::Internal,
        GrpcStatusCode::Unavailable => tina_runtime::GrpcStatusCode::Unavailable,
        GrpcStatusCode::DataLoss => tina_runtime::GrpcStatusCode::DataLoss,
        GrpcStatusCode::Unauthenticated => tina_runtime::GrpcStatusCode::Unauthenticated,
    }
}

/// A gRPC status plus optional message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpcStatus {
    pub code: GrpcStatusCode,
    pub message: Option<String>,
}

impl GrpcStatus {
    pub fn new(code: GrpcStatusCode) -> Self {
        Self {
            code,
            message: None,
        }
    }

    pub fn with_message(code: GrpcStatusCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: Some(message.into()),
        }
    }

    pub fn ok() -> Self {
        Self::new(GrpcStatusCode::Ok)
    }
}

/// Errors produced by gRPC wire handling or the tiny h2c client helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrpcError {
    Status(GrpcStatus),
    InvalidPath(String),
    BadContentType,
    BadFrame,
    CompressedUnsupported,
    MessageTooLarge { len: usize, max: usize },
    Decode,
    EncodeTooLarge { len: usize, max: usize },
    TooManyMessages { count: usize, max: usize },
    Io(String),
    MissingTrailers,
}

pub(crate) fn is_grpc_content_type(value: &str) -> bool {
    let content_type = value
        .split_once(';')
        .map_or(value, |(content_type, _)| content_type)
        .trim();
    content_type.eq_ignore_ascii_case("application/grpc")
        || content_type.eq_ignore_ascii_case("application/grpc+proto")
}

/// Typed unary request passed to user handlers.
#[derive(Debug, Clone)]
pub struct GrpcRequest<T> {
    path: Arc<str>,
    pub message: T,
}

impl<T> GrpcRequest<T> {
    /// The gRPC method path this request was routed to, e.g. `/pkg.Svc/Method`.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// Typed unary response returned by user handlers.
#[derive(Debug, Clone)]
pub struct GrpcResponse<T> {
    pub message: T,
}

impl<T> GrpcResponse<T> {
    pub fn new(message: T) -> Self {
        Self { message }
    }
}

trait ErasedUnary: Send + Sync {
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse;
    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse;
}

trait ErasedServerStreaming: Send + Sync {
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse;
    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse;
}

trait ErasedBufferedServerStreaming: Send + Sync {
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse;
    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse;
}

trait ErasedClientStreaming: Send + Sync {
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse;
    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse;
}

trait ErasedStreaming: Send + Sync {
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse;
    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse;
}

trait ErasedStreamingRaw: Send + Sync {
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse;
    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse;
}

trait ErasedActorCancel<S: Shard + 'static>: Send {
    fn cancel(self: Box<Self>, id: u64) -> Effect<GrpcRouter<S>>;
}

struct ActorCancel<R> {
    handle: tina::CallHandle<R>,
}

impl<S, R> ErasedActorCancel<S> for ActorCancel<R>
where
    S: Shard + 'static,
    R: Send + 'static,
{
    fn cancel(self: Box<Self>, id: u64) -> Effect<GrpcRouter<S>> {
        cancel_call(self.handle)
            .then(move |outcome| GrpcRouterMsg::ActorRouteCancelled { id, outcome })
    }
}

struct ActorRouteDispatch<S: Shard + 'static> {
    effect: Effect<GrpcRouter<S>>,
    cancel: Box<dyn ErasedActorCancel<S>>,
}

type ActorRouteDispatchResult<S> = Result<ActorRouteDispatch<S>, Box<HttpResponse>>;

trait ErasedActorUnary<S: Shard + 'static>: Send + Sync {
    fn call(
        &self,
        request: HttpRequest,
        limits: GrpcLimits,
        id: u64,
    ) -> ActorRouteDispatchResult<S>;

    fn call_http2(
        &self,
        request: GrpcHttp2Request,
        limits: GrpcLimits,
        id: u64,
    ) -> ActorRouteDispatchResult<S>;
}

trait ErasedActorStreaming<S: Shard + 'static>: Send + Sync {
    fn call(
        &self,
        request: HttpRequest,
        limits: GrpcLimits,
        id: u64,
    ) -> ActorRouteDispatchResult<S>;

    fn call_http2(
        &self,
        request: GrpcHttp2Request,
        limits: GrpcLimits,
        id: u64,
    ) -> ActorRouteDispatchResult<S>;
}

struct ActorUnaryHandler<Event, Req, Resp, S: Shard> {
    target: tina::ServiceRequestAddress<
        Event,
        GrpcRequest<Req>,
        Result<GrpcResponse<Resp>, GrpcStatus>,
    >,
    timeout: Duration,
    _shard: PhantomData<S>,
}

struct ActorStreamingHandler<Event, Req, Resp, S: Shard> {
    target: tina::ServiceRequestAddress<
        Event,
        GrpcStreamingCall<Req, Resp>,
        Result<GrpcStreamingResponse<Resp>, GrpcStatus>,
    >,
    timeout: Duration,
    _shard: PhantomData<S>,
}

struct UnaryHandler<Req, Resp, F> {
    f: F,
    _types: PhantomData<fn(Req) -> Resp>,
}

impl<Req, Resp, F> ErasedUnary for UnaryHandler<Req, Resp, F>
where
    Req: Message + Default + Send + Sync + 'static,
    Resp: Message + Default + Send + Sync + 'static,
    F: Fn(GrpcRequest<Req>) -> Result<GrpcResponse<Resp>, GrpcStatus> + Send + Sync + 'static,
{
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse {
        let HttpRequest {
            path,
            headers,
            body,
            ..
        } = request;
        // The public request shape owns a `String`; handlers take the interned
        // `Arc<str>` path, so the generic entry pays one copy here.
        let path: Arc<str> = Arc::from(path);
        match decode_unary_parts::<Req>(&headers, &body, limits) {
            Ok(message) => match (self.f)(GrpcRequest { path, message }) {
                Ok(response) => match encode_grpc_message(&response.message, limits) {
                    Ok(body) => grpc_http_response(body, GrpcStatus::ok()),
                    Err(GrpcError::EncodeTooLarge { len, max }) => grpc_http_response(
                        Vec::new(),
                        GrpcStatus::with_message(
                            GrpcStatusCode::ResourceExhausted,
                            format!("response message {len} exceeds cap {max}"),
                        ),
                    ),
                    Err(_) => {
                        grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal))
                    }
                },
                Err(status) => grpc_http_response(Vec::new(), status),
            },
            Err(error) => grpc_http_response(Vec::new(), status_for_error(error)),
        }
    }

    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse {
        let GrpcHttp2Request {
            path,
            body,
            content_type_ok,
            unsupported_encoding,
            ..
        } = request;
        let GrpcHttp2Body::Buffered(body) = body else {
            return grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            );
        };
        match decode_unary_body_with_flags::<Req>(
            &body,
            content_type_ok,
            unsupported_encoding,
            limits,
        ) {
            Ok(message) => match (self.f)(GrpcRequest { path, message }) {
                Ok(response) => match encode_grpc_message(&response.message, limits) {
                    Ok(body) => grpc_http_response(body, GrpcStatus::ok()),
                    Err(GrpcError::EncodeTooLarge { len, max }) => grpc_http_response(
                        Vec::new(),
                        GrpcStatus::with_message(
                            GrpcStatusCode::ResourceExhausted,
                            format!("response message {len} exceeds cap {max}"),
                        ),
                    ),
                    Err(_) => {
                        grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal))
                    }
                },
                Err(status) => grpc_http_response(Vec::new(), status),
            },
            Err(error) => grpc_http_response(Vec::new(), status_for_error(error)),
        }
    }
}

struct ServerStreamingHandler<Req, F> {
    f: F,
    _types: PhantomData<fn(Req)>,
}

struct BufferedServerStreamingHandler<Req, F> {
    f: F,
    _types: PhantomData<fn(Req)>,
}

struct ClientStreamingHandler<Req, Resp, F> {
    f: F,
    _types: PhantomData<fn(Req) -> Resp>,
}

struct StreamingHandler<Req, Resp, F> {
    f: F,
    _types: PhantomData<fn(Req) -> Resp>,
}

struct StreamingRawHandler<Req, F> {
    f: F,
    _types: PhantomData<fn(Req)>,
}

/// Response returned by a server-streaming gRPC handler.
///
/// The source must yield already gRPC-framed protobuf messages. Use
/// [`encode_grpc_message`] for each message. HTTP/2 owns flow control and
/// pulls one chunk at a time.
#[derive(Debug, Clone)]
pub struct GrpcServerStreamingResponse {
    pub source: tina::Address<ResponseChunkMsg, ResponseChunkReply>,
}

impl GrpcServerStreamingResponse {
    pub fn new(source: tina::Address<ResponseChunkMsg, ResponseChunkReply>) -> Self {
        Self { source }
    }

    pub fn from_messages<S, F, T, I>(
        runtime: &tina_runtime::ThreadedRuntime<S, F>,
        messages: I,
        limits: GrpcLimits,
        mailbox_capacity: usize,
    ) -> Result<Self, GrpcError>
    where
        S: Shard + Send + Sync + 'static,
        F: tina_runtime::MailboxFactory + Send + 'static,
        T: Message + Send + 'static,
        I: IntoIterator<Item = T>,
        I::IntoIter: Send + 'static,
    {
        let chunks = messages
            .into_iter()
            .map(|message| encode_grpc_message(&message, limits))
            .collect::<Result<Vec<_>, _>>()?;
        let source = IterBodySource::<S>::register(runtime, chunks.into_iter(), mailbox_capacity)
            .map_err(|error| GrpcError::Io(format!("{error:?}")))?;
        Ok(Self { source })
    }
}

/// Response returned by a finite buffered server-streaming gRPC handler.
///
/// Use this when the service already has a small fixed stream. It emits a
/// normal gRPC response body with multiple length-prefixed messages and final
/// OK trailers, without registering a response-source isolate for each call.
#[derive(Debug, Clone)]
pub struct GrpcBufferedServerStreamingResponse {
    body: Arc<[u8]>,
}

impl GrpcBufferedServerStreamingResponse {
    fn from_framed_body(body: Vec<u8>) -> Self {
        Self {
            body: Arc::from(body.into_boxed_slice()),
        }
    }

    pub fn from_messages<T, I>(
        messages: I,
        limits: GrpcBufferedStreamLimits,
    ) -> Result<Self, GrpcError>
    where
        T: Message,
        I: IntoIterator<Item = T>,
    {
        let mut body = Vec::new();
        for (idx, message) in messages.into_iter().enumerate() {
            let count = idx + 1;
            if count > limits.max_messages {
                return Err(GrpcError::TooManyMessages {
                    count,
                    max: limits.max_messages,
                });
            }
            let message_len = message.encoded_len();
            let framed_len = GRPC_FRAME_HEADER_LEN
                .checked_add(message_len)
                .ok_or(GrpcError::BadFrame)?;
            let next_len = body
                .len()
                .checked_add(framed_len)
                .ok_or(GrpcError::BadFrame)?;
            if next_len > limits.max_body_bytes {
                return Err(GrpcError::EncodeTooLarge {
                    len: next_len,
                    max: limits.max_body_bytes,
                });
            }
            encode_grpc_message_into(&mut body, &message, limits.message)?;
        }
        Ok(Self::from_framed_body(body))
    }
}

/// A typed gRPC bidirectional streaming call.
///
/// This is the primary Tina-shaped surface for gRPC's "bidirectional
/// streaming RPC" mode. It is not an async stream: user code still owns a
/// normal Tina state machine, pulls requests explicitly, and returns a bounded
/// response source.
#[derive(Debug)]
pub struct GrpcStreamingCall<Req, Resp> {
    path: Arc<str>,
    pub requests: GrpcRequestStream<Req>,
    _response: PhantomData<fn() -> Resp>,
}

impl<Req, Resp> GrpcStreamingCall<Req, Resp> {
    /// The gRPC method path this stream was routed to, e.g. `/pkg.Svc/Method`.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// Typed gRPC request stream helper for streaming RPCs.
///
/// The service/source owns this value and calls [`Self::pull_next_effect`] only
/// when it is ready for more input. Decoding state is held here so user code
/// does not hand-parse gRPC frame headers.
#[derive(Debug)]
pub struct GrpcRequestStream<T> {
    stream: Http2RequestStream,
    limits: GrpcLimits,
    buffer: Vec<u8>,
    eof: bool,
    _message: PhantomData<fn() -> T>,
}

/// Result of feeding one HTTP/2 request-body continuation into a typed gRPC
/// request stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrpcStreamReply<T> {
    Message(T),
    NeedMore,
    Eof,
    Status(GrpcStatus),
    Cancelled,
    DeadlineExceeded,
}

/// Outcome of one length-prefix reassembly step over a gRPC frame buffer.
///
/// This is the pure part of [`GrpcRequestStream::next_buffered_message`],
/// extracted so it can be unit-tested and fuzzed with no `Http2RequestStream`
/// coupling: it never touches the runtime, only the byte buffer and the
/// configured cap. See `fuzz/fuzz_targets/grpc_frame.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrpcFrameBoundary {
    /// Fewer than one full frame is buffered; the buffer is untouched.
    NeedMore,
    /// The buffered frame is malformed (unsupported compression flag or a
    /// declared length over the cap); the buffer has already been cleared.
    Malformed(GrpcFrameError),
    /// One full frame — the 5-byte header plus its declared payload — sits at
    /// `buffer[..end]`, not yet drained.
    Ready { end: usize },
}

/// Why [`GrpcFrameBoundary::Malformed`] rejected the buffered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrpcFrameError {
    /// The reserved compression-flag byte was non-zero; only identity
    /// (uncompressed) frames are supported.
    UnsupportedCompression,
    /// The declared payload length exceeds `max_message_bytes`.
    MessageTooLarge { len: usize, max: usize },
}

/// Advance the length-prefix reassembly state machine by exactly one step.
///
/// Preserves the original guards byte-for-byte: the 5-byte header check, the
/// `max_message_bytes` cap on the declared length (checked before the
/// buffered-bytes-available check, so an oversized declared length is
/// rejected without waiting for the body to arrive), and an `end` offset that
/// is always `<= buffer.len()` before it is used to slice or drain — no
/// allocation is ever sized directly from attacker-supplied bytes without
/// first passing the cap.
pub(crate) fn next_grpc_frame_boundary(
    buffer: &mut Vec<u8>,
    max_message_bytes: usize,
) -> GrpcFrameBoundary {
    if buffer.len() < GRPC_FRAME_HEADER_LEN {
        return GrpcFrameBoundary::NeedMore;
    }
    if buffer[0] != 0 {
        buffer.clear();
        return GrpcFrameBoundary::Malformed(GrpcFrameError::UnsupportedCompression);
    }
    let len = u32::from_be_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;
    if len > max_message_bytes {
        buffer.clear();
        return GrpcFrameBoundary::Malformed(GrpcFrameError::MessageTooLarge {
            len,
            max: max_message_bytes,
        });
    }
    let end = GRPC_FRAME_HEADER_LEN + len;
    if buffer.len() < end {
        return GrpcFrameBoundary::NeedMore;
    }
    GrpcFrameBoundary::Ready { end }
}

/// Pure entry points for the out-of-workspace fuzz harness. The reassembler
/// is `pub(crate)`; this is the only sanctioned way past that boundary and
/// exists only under the `fuzzing` feature.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing {
    /// Feed arbitrary bytes through the length-prefix reassembler under the
    /// default `max_message_bytes` cap, draining and re-running boundary
    /// lookup so concatenated frames in one input get exercised too. Only
    /// asserts panic-freedom and that every boundary stays within the
    /// buffer — never calls into `prost` decode.
    pub fn fuzz_grpc_frame_reassembly(bytes: &[u8]) {
        let max_message_bytes = super::GrpcLimits::default().max_message_bytes;
        let mut buffer = bytes.to_vec();
        loop {
            match super::next_grpc_frame_boundary(&mut buffer, max_message_bytes) {
                super::GrpcFrameBoundary::NeedMore => break,
                super::GrpcFrameBoundary::Malformed(_) => break,
                super::GrpcFrameBoundary::Ready { end } => {
                    assert!(end <= buffer.len(), "boundary must stay within the buffer");
                    let _frame: Vec<u8> = buffer.drain(..end).collect();
                }
            }
        }
    }
}

impl<T> GrpcRequestStream<T>
where
    T: Message + Default,
{
    pub fn new(stream: Http2RequestStream, limits: GrpcLimits) -> Self {
        Self {
            stream,
            limits,
            buffer: Vec::new(),
            eof: false,
            _message: PhantomData,
        }
    }

    pub fn pull_next_effect<I>(&self, timeout: Duration) -> Effect<I>
    where
        I: Isolate<Message = ResponseChunkMsg, Io = tina_runtime::RuntimeCall<ResponseChunkMsg>>,
    {
        call(
            self.stream.source,
            crate::Http2ConnectionMsg::body_next(self.stream.stream_id),
            timeout,
        )
        .then(ResponseChunkMsg::Http2RequestChunk)
    }

    pub fn accept_http2_outcome(
        &mut self,
        outcome: CallOutcome<Http2ConnectionReply>,
    ) -> GrpcStreamReply<T> {
        match outcome {
            CallOutcome::Replied(Http2ConnectionReply::RequestChunk(RequestChunkReply::Chunk(
                bytes,
            ))) => {
                self.buffer.extend_from_slice(&bytes);
                self.next_buffered_message()
            }
            CallOutcome::Replied(Http2ConnectionReply::RequestChunk(RequestChunkReply::Eof)) => {
                self.eof = true;
                if self.buffer.is_empty() {
                    GrpcStreamReply::Eof
                } else {
                    self.buffer.clear();
                    GrpcStreamReply::Status(GrpcStatus::new(GrpcStatusCode::InvalidArgument))
                }
            }
            CallOutcome::Replied(Http2ConnectionReply::RequestChunk(RequestChunkReply::Error(
                _,
            )))
            | CallOutcome::Replied(Http2ConnectionReply::Report(_))
            | CallOutcome::Replied(Http2ConnectionReply::RequestChunk(
                RequestChunkReply::WebSocketSend(_),
            ))
            | CallOutcome::Replied(Http2ConnectionReply::RequestChunk(
                RequestChunkReply::WebSocketReport(_),
            ))
            | CallOutcome::Closed
            | CallOutcome::Rejected(_) => GrpcStreamReply::Cancelled,
            CallOutcome::Full => {
                GrpcStreamReply::Status(GrpcStatus::new(GrpcStatusCode::ResourceExhausted))
            }
            CallOutcome::Timeout => GrpcStreamReply::DeadlineExceeded,
        }
    }

    pub fn next_buffered(&mut self) -> GrpcStreamReply<T> {
        if self.buffer.is_empty() && self.eof {
            GrpcStreamReply::Eof
        } else {
            self.next_buffered_message()
        }
    }

    fn next_buffered_message(&mut self) -> GrpcStreamReply<T> {
        match next_grpc_frame_boundary(&mut self.buffer, self.limits.max_message_bytes) {
            GrpcFrameBoundary::NeedMore => GrpcStreamReply::NeedMore,
            GrpcFrameBoundary::Malformed(GrpcFrameError::UnsupportedCompression) => {
                GrpcStreamReply::Status(GrpcStatus::with_message(
                    GrpcStatusCode::Unimplemented,
                    "compression unsupported",
                ))
            }
            GrpcFrameBoundary::Malformed(GrpcFrameError::MessageTooLarge { len, max }) => {
                GrpcStreamReply::Status(GrpcStatus::with_message(
                    GrpcStatusCode::ResourceExhausted,
                    format!("request message {len} exceeds cap {max}"),
                ))
            }
            GrpcFrameBoundary::Ready { end } => {
                let frame: Vec<u8> = self.buffer.drain(..end).collect();
                match T::decode(&frame[GRPC_FRAME_HEADER_LEN..]) {
                    Ok(message) => GrpcStreamReply::Message(message),
                    Err(_) => {
                        GrpcStreamReply::Status(GrpcStatus::new(GrpcStatusCode::InvalidArgument))
                    }
                }
            }
        }
    }
}

/// Response returned by a typed gRPC streaming handler.
#[derive(Debug, Clone)]
pub struct GrpcStreamingResponse<T> {
    pub source: tina::Address<ResponseChunkMsg, ResponseChunkReply>,
    pub status: GrpcStatus,
    _message: PhantomData<fn() -> T>,
}

impl<T> GrpcStreamingResponse<T> {
    pub fn new(source: tina::Address<ResponseChunkMsg, ResponseChunkReply>) -> Self {
        Self {
            source,
            status: GrpcStatus::ok(),
            _message: PhantomData,
        }
    }

    pub fn with_status(
        source: tina::Address<ResponseChunkMsg, ResponseChunkReply>,
        status: GrpcStatus,
    ) -> Self {
        Self {
            source,
            status,
            _message: PhantomData,
        }
    }
}

pub fn grpc_stream_message<T: Message>(
    message: &T,
    limits: GrpcLimits,
) -> Result<ResponseChunkReply, GrpcError> {
    encode_grpc_message(message, limits).map(ResponseChunkReply::Chunk)
}

pub fn grpc_stream_finish(status: GrpcStatus) -> ResponseChunkReply {
    ResponseChunkReply::GrpcStatus(status)
}

/// Advanced raw request passed to low-level streaming handlers.
///
/// Prefer [`GrpcRouter::streaming`] unless you are building a protocol adapter
/// or test fixture that must work directly with HTTP/2 request chunks.
#[derive(Debug, Clone)]
pub struct GrpcRawStreamingRequest<T> {
    path: Arc<str>,
    pub stream: Http2RequestStream,
    _message: PhantomData<fn() -> T>,
}

impl<T> GrpcRawStreamingRequest<T> {
    /// The gRPC method path this request was routed to, e.g. `/pkg.Svc/Method`.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn message_type(&self) -> PhantomData<fn() -> T> {
        self._message
    }
}

/// Response returned by an advanced raw gRPC streaming handler.
#[derive(Debug, Clone)]
pub struct GrpcRawStreamingResponse {
    pub source: tina::Address<ResponseChunkMsg, ResponseChunkReply>,
    pub status: GrpcStatus,
}

impl GrpcRawStreamingResponse {
    pub fn new(source: tina::Address<ResponseChunkMsg, ResponseChunkReply>) -> Self {
        Self {
            source,
            status: GrpcStatus::ok(),
        }
    }

    pub fn with_status(
        source: tina::Address<ResponseChunkMsg, ResponseChunkReply>,
        status: GrpcStatus,
    ) -> Self {
        Self { source, status }
    }
}

impl<Req, F> ErasedServerStreaming for ServerStreamingHandler<Req, F>
where
    Req: Message + Default + Send + Sync + 'static,
    F: Fn(GrpcRequest<Req>) -> Result<GrpcServerStreamingResponse, GrpcStatus>
        + Send
        + Sync
        + 'static,
{
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse {
        let HttpRequest {
            path,
            headers,
            body,
            ..
        } = request;
        // The public request shape owns a `String`; handlers take the interned
        // `Arc<str>` path, so the generic entry pays one copy here.
        let path: Arc<str> = Arc::from(path);
        match decode_unary_parts::<Req>(&headers, &body, limits) {
            Ok(message) => match (self.f)(GrpcRequest { path, message }) {
                Ok(response) => grpc_streaming_http_response(response.source, GrpcStatus::ok()),
                Err(status) => grpc_http_response(Vec::new(), status),
            },
            Err(error) => grpc_http_response(Vec::new(), status_for_error(error)),
        }
    }

    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse {
        let GrpcHttp2Request {
            path,
            body,
            content_type_ok,
            unsupported_encoding,
            ..
        } = request;
        let GrpcHttp2Body::Buffered(body) = body else {
            return grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            );
        };
        match decode_unary_body_with_flags::<Req>(
            &body,
            content_type_ok,
            unsupported_encoding,
            limits,
        ) {
            Ok(message) => match (self.f)(GrpcRequest { path, message }) {
                Ok(response) => grpc_streaming_http_response(response.source, GrpcStatus::ok()),
                Err(status) => grpc_http_response(Vec::new(), status),
            },
            Err(error) => grpc_http_response(Vec::new(), status_for_error(error)),
        }
    }
}

impl<Req, F> ErasedBufferedServerStreaming for BufferedServerStreamingHandler<Req, F>
where
    Req: Message + Default + Send + Sync + 'static,
    F: Fn(GrpcRequest<Req>) -> Result<GrpcBufferedServerStreamingResponse, GrpcStatus>
        + Send
        + Sync
        + 'static,
{
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse {
        let HttpRequest {
            path,
            headers,
            body,
            ..
        } = request;
        // The public request shape owns a `String`; handlers take the interned
        // `Arc<str>` path, so the generic entry pays one copy here.
        let path: Arc<str> = Arc::from(path);
        match decode_unary_parts::<Req>(&headers, &body, limits) {
            Ok(message) => match (self.f)(GrpcRequest { path, message }) {
                Ok(response) => grpc_http_response_shared(response.body, GrpcStatus::ok()),
                Err(status) => grpc_http_response(Vec::new(), status),
            },
            Err(error) => grpc_http_response(Vec::new(), status_for_error(error)),
        }
    }

    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse {
        let GrpcHttp2Request {
            path,
            body,
            content_type_ok,
            unsupported_encoding,
            ..
        } = request;
        let GrpcHttp2Body::Buffered(body) = body else {
            return grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            );
        };
        match decode_unary_body_with_flags::<Req>(
            &body,
            content_type_ok,
            unsupported_encoding,
            limits,
        ) {
            Ok(message) => match (self.f)(GrpcRequest { path, message }) {
                Ok(response) => grpc_http_response_shared(response.body, GrpcStatus::ok()),
                Err(status) => grpc_http_response(Vec::new(), status),
            },
            Err(error) => grpc_http_response(Vec::new(), status_for_error(error)),
        }
    }
}

/// Typed client-streaming request passed to user handlers.
#[derive(Debug, Clone)]
pub struct GrpcClientStreamingRequest<T> {
    path: Arc<str>,
    pub messages: Vec<T>,
}

impl<T> GrpcClientStreamingRequest<T> {
    /// The gRPC method path this request was routed to, e.g. `/pkg.Svc/Method`.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl<Req, Resp, F> ErasedClientStreaming for ClientStreamingHandler<Req, Resp, F>
where
    Req: Message + Default + Send + Sync + 'static,
    Resp: Message + Default + Send + Sync + 'static,
    F: Fn(GrpcClientStreamingRequest<Req>) -> Result<GrpcResponse<Resp>, GrpcStatus>
        + Send
        + Sync
        + 'static,
{
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse {
        let HttpRequest {
            path,
            headers,
            body,
            ..
        } = request;
        // The public request shape owns a `String`; handlers take the interned
        // `Arc<str>` path, so the generic entry pays one copy here.
        let path: Arc<str> = Arc::from(path);
        match decode_streaming_parts::<Req>(&headers, &body, limits) {
            Ok(messages) => match (self.f)(GrpcClientStreamingRequest { path, messages }) {
                Ok(response) => match encode_grpc_message(&response.message, limits) {
                    Ok(body) => grpc_http_response(body, GrpcStatus::ok()),
                    Err(GrpcError::EncodeTooLarge { len, max }) => grpc_http_response(
                        Vec::new(),
                        GrpcStatus::with_message(
                            GrpcStatusCode::ResourceExhausted,
                            format!("response message {len} exceeds cap {max}"),
                        ),
                    ),
                    Err(_) => {
                        grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal))
                    }
                },
                Err(status) => grpc_http_response(Vec::new(), status),
            },
            Err(error) => grpc_http_response(Vec::new(), status_for_error(error)),
        }
    }

    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse {
        let GrpcHttp2Request {
            path,
            body,
            content_type_ok,
            unsupported_encoding,
            ..
        } = request;
        let GrpcHttp2Body::Buffered(body) = body else {
            return grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            );
        };
        match decode_streaming_body_with_flags::<Req>(
            &body,
            content_type_ok,
            unsupported_encoding,
            limits,
        ) {
            Ok(messages) => match (self.f)(GrpcClientStreamingRequest { path, messages }) {
                Ok(response) => match encode_grpc_message(&response.message, limits) {
                    Ok(body) => grpc_http_response(body, GrpcStatus::ok()),
                    Err(GrpcError::EncodeTooLarge { len, max }) => grpc_http_response(
                        Vec::new(),
                        GrpcStatus::with_message(
                            GrpcStatusCode::ResourceExhausted,
                            format!("response message {len} exceeds cap {max}"),
                        ),
                    ),
                    Err(_) => {
                        grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal))
                    }
                },
                Err(status) => grpc_http_response(Vec::new(), status),
            },
            Err(error) => grpc_http_response(Vec::new(), status_for_error(error)),
        }
    }
}

impl<Req, Resp, F> ErasedStreaming for StreamingHandler<Req, Resp, F>
where
    Req: Message + Default + Send + Sync + 'static,
    Resp: Message + Default + Send + Sync + 'static,
    F: Fn(GrpcStreamingCall<Req, Resp>) -> Result<GrpcStreamingResponse<Resp>, GrpcStatus>
        + Send
        + Sync
        + 'static,
{
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse {
        let HttpRequest { path, body, .. } = request;
        let path: Arc<str> = Arc::from(path);
        let HttpRequestBody::Http2Stream(stream) = body else {
            return grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            );
        };
        match (self.f)(GrpcStreamingCall {
            path,
            requests: GrpcRequestStream::new(stream, limits),
            _response: PhantomData,
        }) {
            Ok(response) => grpc_streaming_http_response(response.source, response.status),
            Err(status) => grpc_http_response(Vec::new(), status),
        }
    }

    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse {
        // Take the HTTP/2 request body stream straight from the compact request
        // — no public `HttpRequest`/`HeaderMap` rebuild. The handler only needs
        // the method path and the request stream.
        let GrpcHttp2Request { path, body, .. } = request;
        let GrpcHttp2Body::Http2Stream(stream) = body else {
            return grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            );
        };
        match (self.f)(GrpcStreamingCall {
            path,
            requests: GrpcRequestStream::new(stream, limits),
            _response: PhantomData,
        }) {
            Ok(response) => grpc_streaming_http_response(response.source, response.status),
            Err(status) => grpc_http_response(Vec::new(), status),
        }
    }
}

impl<Req, F> ErasedStreamingRaw for StreamingRawHandler<Req, F>
where
    Req: Message + Default + Send + Sync + 'static,
    F: Fn(GrpcRawStreamingRequest<Req>) -> Result<GrpcRawStreamingResponse, GrpcStatus>
        + Send
        + Sync
        + 'static,
{
    fn call(&self, request: HttpRequest, _limits: GrpcLimits) -> HttpResponse {
        let HttpRequest { path, body, .. } = request;
        let path: Arc<str> = Arc::from(path);
        let HttpRequestBody::Http2Stream(stream) = body else {
            return grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            );
        };
        match (self.f)(GrpcRawStreamingRequest {
            path,
            stream,
            _message: PhantomData,
        }) {
            Ok(response) => grpc_streaming_http_response(response.source, response.status),
            Err(status) => grpc_http_response(Vec::new(), status),
        }
    }

    fn call_http2(&self, request: GrpcHttp2Request, _limits: GrpcLimits) -> HttpResponse {
        // Compact entry point: hand the raw HTTP/2 request stream to the handler
        // without building a public `HttpRequest`.
        let GrpcHttp2Request { path, body, .. } = request;
        let GrpcHttp2Body::Http2Stream(stream) = body else {
            return grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            );
        };
        match (self.f)(GrpcRawStreamingRequest {
            path,
            stream,
            _message: PhantomData,
        }) {
            Ok(response) => grpc_streaming_http_response(response.source, response.status),
            Err(status) => grpc_http_response(Vec::new(), status),
        }
    }
}

impl<Event, Req, Resp, S> ErasedActorUnary<S> for ActorUnaryHandler<Event, Req, Resp, S>
where
    Event: Send + 'static,
    Req: Message + Default + Send + Sync + 'static,
    Resp: Message + Default + Send + Sync + 'static,
    S: Shard + Send + Sync + 'static,
{
    fn call(
        &self,
        request: HttpRequest,
        limits: GrpcLimits,
        id: u64,
    ) -> ActorRouteDispatchResult<S> {
        let HttpRequest {
            path,
            headers,
            body,
            ..
        } = request;
        let message = decode_unary_parts::<Req>(&headers, &body, limits)
            .map_err(|error| Box::new(grpc_http_response(Vec::new(), status_for_error(error))))?;
        Ok(actor_unary_dispatch(
            self.target,
            GrpcRequest {
                path: Arc::from(path),
                message,
            },
            self.timeout,
            limits,
            id,
        ))
    }

    fn call_http2(
        &self,
        request: GrpcHttp2Request,
        limits: GrpcLimits,
        id: u64,
    ) -> ActorRouteDispatchResult<S> {
        let GrpcHttp2Request {
            path,
            body,
            content_type_ok,
            unsupported_encoding,
            ..
        } = request;
        let GrpcHttp2Body::Buffered(body) = body else {
            return Err(Box::new(grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            )));
        };
        let message = decode_unary_body_with_flags::<Req>(
            &body,
            content_type_ok,
            unsupported_encoding,
            limits,
        )
        .map_err(|error| Box::new(grpc_http_response(Vec::new(), status_for_error(error))))?;
        Ok(actor_unary_dispatch(
            self.target,
            GrpcRequest { path, message },
            self.timeout,
            limits,
            id,
        ))
    }
}

fn actor_unary_dispatch<Event, Req, Resp, S>(
    target: tina::ServiceRequestAddress<
        Event,
        GrpcRequest<Req>,
        Result<GrpcResponse<Resp>, GrpcStatus>,
    >,
    request: GrpcRequest<Req>,
    timeout: Duration,
    limits: GrpcLimits,
    id: u64,
) -> ActorRouteDispatch<S>
where
    Event: Send + 'static,
    Req: Message + Default + Send + Sync + 'static,
    Resp: Message + Default + Send + Sync + 'static,
    S: Shard + 'static,
{
    let (effect, handle) = call_cancelable_request(target, request, timeout).then(move |outcome| {
        GrpcRouterMsg::ActorRouteReturned {
            id,
            result: actor_unary_result(outcome, limits),
        }
    });
    ActorRouteDispatch {
        effect,
        cancel: Box::new(ActorCancel { handle }),
    }
}

fn actor_unary_result<Resp>(
    outcome: CallOutcome<Result<GrpcResponse<Resp>, GrpcStatus>>,
    limits: GrpcLimits,
) -> ActorRouteResult
where
    Resp: Message,
{
    match outcome {
        CallOutcome::Replied(Ok(response)) => {
            match encode_grpc_message(&response.message, limits) {
                Ok(body) => ActorRouteResult::Response(grpc_http_response(body, GrpcStatus::ok())),
                Err(error) => ActorRouteResult::Response(grpc_http_response(
                    Vec::new(),
                    status_for_error(error),
                )),
            }
        }
        CallOutcome::Replied(Err(status)) => {
            ActorRouteResult::Response(grpc_http_response(Vec::new(), status))
        }
        CallOutcome::Full => ActorRouteResult::Failure(GrpcActorRouteFailure::Full),
        CallOutcome::Closed => ActorRouteResult::Failure(GrpcActorRouteFailure::Closed),
        CallOutcome::Timeout => ActorRouteResult::Failure(GrpcActorRouteFailure::Timeout),
        CallOutcome::Rejected(reason) => {
            ActorRouteResult::Failure(GrpcActorRouteFailure::Rejected(reason))
        }
    }
}

impl<Event, Req, Resp, S> ErasedActorStreaming<S> for ActorStreamingHandler<Event, Req, Resp, S>
where
    Event: Send + 'static,
    Req: Message + Default + Send + Sync + 'static,
    Resp: Message + Default + Send + Sync + 'static,
    S: Shard + Send + Sync + 'static,
{
    fn call(
        &self,
        request: HttpRequest,
        limits: GrpcLimits,
        id: u64,
    ) -> ActorRouteDispatchResult<S> {
        let HttpRequest { path, body, .. } = request;
        let HttpRequestBody::Http2Stream(stream) = body else {
            return Err(Box::new(grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            )));
        };
        Ok(actor_streaming_dispatch(
            self.target,
            GrpcStreamingCall {
                path: Arc::from(path),
                requests: GrpcRequestStream::new(stream, limits),
                _response: PhantomData,
            },
            self.timeout,
            id,
        ))
    }

    fn call_http2(
        &self,
        request: GrpcHttp2Request,
        limits: GrpcLimits,
        id: u64,
    ) -> ActorRouteDispatchResult<S> {
        let GrpcHttp2Request { path, body, .. } = request;
        let GrpcHttp2Body::Http2Stream(stream) = body else {
            return Err(Box::new(grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            )));
        };
        Ok(actor_streaming_dispatch(
            self.target,
            GrpcStreamingCall {
                path,
                requests: GrpcRequestStream::new(stream, limits),
                _response: PhantomData,
            },
            self.timeout,
            id,
        ))
    }
}

fn actor_streaming_dispatch<Event, Req, Resp, S>(
    target: tina::ServiceRequestAddress<
        Event,
        GrpcStreamingCall<Req, Resp>,
        Result<GrpcStreamingResponse<Resp>, GrpcStatus>,
    >,
    request: GrpcStreamingCall<Req, Resp>,
    timeout: Duration,
    id: u64,
) -> ActorRouteDispatch<S>
where
    Event: Send + 'static,
    Req: Message + Default + Send + Sync + 'static,
    Resp: Message + Default + Send + Sync + 'static,
    S: Shard + 'static,
{
    let (effect, handle) = call_cancelable_request(target, request, timeout).then(move |outcome| {
        let result = match outcome {
            CallOutcome::Replied(Ok(response)) => ActorRouteResult::Response(
                grpc_streaming_http_response(response.source, response.status),
            ),
            CallOutcome::Replied(Err(status)) => {
                ActorRouteResult::Response(grpc_http_response(Vec::new(), status))
            }
            CallOutcome::Full => ActorRouteResult::Failure(GrpcActorRouteFailure::Full),
            CallOutcome::Closed => ActorRouteResult::Failure(GrpcActorRouteFailure::Closed),
            CallOutcome::Timeout => ActorRouteResult::Failure(GrpcActorRouteFailure::Timeout),
            CallOutcome::Rejected(reason) => {
                ActorRouteResult::Failure(GrpcActorRouteFailure::Rejected(reason))
            }
        };
        GrpcRouterMsg::ActorRouteReturned { id, result }
    });
    ActorRouteDispatch {
        effect,
        cancel: Box::new(ActorCancel { handle }),
    }
}

/// Tina-shaped unary service/router template.
///
/// Register this isolate behind [`crate::Http2Listener`]. Each route
/// decodes one protobuf request message and returns one protobuf
/// response message plus real gRPC trailers.
pub struct GrpcRouter<S: Shard> {
    limits: GrpcLimits,
    actor_route_capacity: usize,
    actor_unary: BTreeMap<String, Box<dyn ErasedActorUnary<S>>>,
    actor_streaming: BTreeMap<String, Box<dyn ErasedActorStreaming<S>>>,
    unary: BTreeMap<String, Box<dyn ErasedUnary>>,
    server_streaming: BTreeMap<String, Box<dyn ErasedServerStreaming>>,
    buffered_server_streaming: BTreeMap<String, Box<dyn ErasedBufferedServerStreaming>>,
    client_streaming: BTreeMap<String, Box<dyn ErasedClientStreaming>>,
    streaming: BTreeMap<String, Box<dyn ErasedStreaming>>,
    streaming_raw: BTreeMap<String, Box<dyn ErasedStreamingRaw>>,
    pending: BTreeMap<u64, PendingGrpcRequest>,
    actor_pending: BTreeMap<u64, PendingActorRoute<S>>,
    next_pending_id: u64,
    _shard: PhantomData<S>,
}

#[derive(Debug)]
pub enum GrpcRouterMsg {
    Request(HttpRequest),
    Http2Request(GrpcHttp2Request),
    RequestBodyChunk {
        id: u64,
        outcome: CallOutcome<Http2ConnectionReply>,
    },
    #[doc(hidden)]
    ActorRouteReturned {
        id: u64,
        result: ActorRouteResult,
    },
    #[doc(hidden)]
    ActorRouteCancelled {
        id: u64,
        outcome: tina::CancelOutcome,
    },
}

/// Why an actor-backed gRPC route could not produce its typed reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrpcActorRouteFailure {
    /// The target actor's bounded mailbox refused admission.
    Full,
    /// The target actor was already stopped.
    Closed,
    /// The configured actor-route deadline elapsed.
    Timeout,
    /// The runtime rejected the typed actor call.
    Rejected(tina::CallRejectedReason),
}

impl GrpcActorRouteFailure {
    /// Stable wire status used for this route transport failure.
    pub fn status(self) -> GrpcStatus {
        match self {
            Self::Full => GrpcStatus::new(GrpcStatusCode::ResourceExhausted),
            Self::Closed => GrpcStatus::new(GrpcStatusCode::Unavailable),
            Self::Timeout => GrpcStatus::new(GrpcStatusCode::DeadlineExceeded),
            Self::Rejected(reason) => GrpcStatus::with_message(
                GrpcStatusCode::FailedPrecondition,
                format!("actor route rejected: {reason:?}"),
            ),
        }
    }
}

/// Configuration error or duplicate path in actor-backed route registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrpcActorRouteRegistrationError {
    ZeroPendingCapacity,
    DuplicatePath(String),
}

impl std::fmt::Display for GrpcActorRouteRegistrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroPendingCapacity => {
                formatter.write_str("actor route capacity must be non-zero")
            }
            Self::DuplicatePath(path) => write!(formatter, "duplicate gRPC route path {path}"),
        }
    }
}

impl std::error::Error for GrpcActorRouteRegistrationError {}

#[doc(hidden)]
#[derive(Debug)]
pub enum ActorRouteResult {
    Response(HttpResponse),
    Failure(GrpcActorRouteFailure),
}

struct PendingActorRoute<S: Shard> {
    call: tina::RequestContext<HttpResponse>,
    cancel: Box<dyn ErasedActorCancel<S>>,
}

#[derive(Debug)]
pub struct GrpcHttp2Request {
    method: Method,
    path: Arc<str>,
    body: GrpcHttp2Body,
    content_type_ok: bool,
    unsupported_encoding: bool,
}

#[derive(Debug)]
enum GrpcHttp2Body {
    Buffered(Vec<u8>),
    Http2Stream(Http2RequestStream),
    Unsupported,
}

impl GrpcHttp2Request {
    fn from_parts(parts: Http2RequestParts) -> Self {
        let body = match parts.body {
            HttpRequestBody::Buffered(bytes) => GrpcHttp2Body::Buffered(bytes),
            HttpRequestBody::Http2Stream(stream) => GrpcHttp2Body::Http2Stream(stream),
            HttpRequestBody::Stream(_) => GrpcHttp2Body::Unsupported,
        };
        Self {
            method: parts.method,
            path: parts.path,
            body,
            content_type_ok: parts.grpc_content_type,
            unsupported_encoding: parts.grpc_encoding_unsupported,
        }
    }
}

impl Http2ServiceMessage for GrpcRouterMsg {
    fn compact_http2_headers() -> bool {
        true
    }

    fn from_http_request(request: HttpRequest) -> Self {
        Self::Request(request)
    }

    fn from_http2_parts(parts: Http2RequestParts) -> Self {
        Self::Http2Request(GrpcHttp2Request::from_parts(parts))
    }
}

/// A streamed gRPC request whose body is being pulled chunk-by-chunk before
/// the route can run. The HTTP/2 (`Http2`) variant keeps compact gRPC facts —
/// not a public `HttpRequest`/`HeaderMap` — so the native streaming path never
/// rebuilds the public request shape.
enum PendingGrpcRequest {
    /// Generic `HttpRequest` entry path (kept for non-compact / direct API use).
    Public {
        request: HttpRequest,
        body: Vec<u8>,
        call: tina::RequestContext<HttpResponse>,
    },
    /// Native compact HTTP/2 entry path. Stores the method path, the two gRPC
    /// header facts, the request stream to pull from, and the accumulated body.
    Http2 {
        method: Method,
        path: Arc<str>,
        content_type_ok: bool,
        unsupported_encoding: bool,
        stream: Http2RequestStream,
        body: Vec<u8>,
        call: tina::RequestContext<HttpResponse>,
    },
}

impl<S: Shard + 'static> GrpcRouter<S> {
    pub fn new(limits: GrpcLimits) -> Self {
        Self {
            limits,
            actor_route_capacity: 0,
            actor_unary: BTreeMap::new(),
            actor_streaming: BTreeMap::new(),
            unary: BTreeMap::new(),
            server_streaming: BTreeMap::new(),
            buffered_server_streaming: BTreeMap::new(),
            client_streaming: BTreeMap::new(),
            streaming: BTreeMap::new(),
            streaming_raw: BTreeMap::new(),
            pending: BTreeMap::new(),
            actor_pending: BTreeMap::new(),
            next_pending_id: 1,
            _shard: PhantomData,
        }
    }

    /// Sets the total number of actor-backed gRPC calls this router may park.
    ///
    /// Admission beyond this bound returns gRPC `ResourceExhausted` before an
    /// actor call is constructed. The bound is shared by all actor routes so a
    /// route cannot hide work in an independent side queue.
    pub fn with_actor_route_capacity(
        mut self,
        capacity: usize,
    ) -> Result<Self, GrpcActorRouteRegistrationError> {
        if capacity == 0 {
            return Err(GrpcActorRouteRegistrationError::ZeroPendingCapacity);
        }
        self.actor_route_capacity = capacity;
        Ok(self)
    }

    /// Registers a typed unary route whose state and behavior live in a Tina actor.
    pub fn try_unary_actor<Event, Req, Resp>(
        mut self,
        path: impl Into<String>,
        target: tina::ServiceRequestAddress<
            Event,
            GrpcRequest<Req>,
            Result<GrpcResponse<Resp>, GrpcStatus>,
        >,
        timeout: Duration,
    ) -> Result<Self, GrpcActorRouteRegistrationError>
    where
        Event: Send + 'static,
        Req: Message + Default + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
        S: Send + Sync,
    {
        let path = path.into();
        self.ensure_actor_route_path_available(&path)?;
        self.actor_unary.insert(
            path,
            Box::new(ActorUnaryHandler::<Event, Req, Resp, S> {
                target,
                timeout,
                _shard: PhantomData,
            }),
        );
        Ok(self)
    }

    /// Registers a typed bidirectional stream route backed by a Tina actor.
    ///
    /// The request stream moves into the actor call. A successful reply moves a
    /// bounded response-source address back to the router; no shared stream slot
    /// or response pool is involved.
    pub fn try_streaming_actor<Event, Req, Resp>(
        mut self,
        path: impl Into<String>,
        target: tina::ServiceRequestAddress<
            Event,
            GrpcStreamingCall<Req, Resp>,
            Result<GrpcStreamingResponse<Resp>, GrpcStatus>,
        >,
        timeout: Duration,
    ) -> Result<Self, GrpcActorRouteRegistrationError>
    where
        Event: Send + 'static,
        Req: Message + Default + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
        S: Send + Sync,
    {
        let path = path.into();
        self.ensure_actor_route_path_available(&path)?;
        self.actor_streaming.insert(
            path,
            Box::new(ActorStreamingHandler::<Event, Req, Resp, S> {
                target,
                timeout,
                _shard: PhantomData,
            }),
        );
        Ok(self)
    }

    fn ensure_actor_route_path_available(
        &self,
        path: &str,
    ) -> Result<(), GrpcActorRouteRegistrationError> {
        if self.actor_route_capacity == 0 {
            return Err(GrpcActorRouteRegistrationError::ZeroPendingCapacity);
        }
        if self.actor_unary.contains_key(path)
            || self.actor_streaming.contains_key(path)
            || self.unary.contains_key(path)
            || self.server_streaming.contains_key(path)
            || self.buffered_server_streaming.contains_key(path)
            || self.client_streaming.contains_key(path)
            || self.streaming.contains_key(path)
            || self.streaming_raw.contains_key(path)
        {
            return Err(GrpcActorRouteRegistrationError::DuplicatePath(
                path.to_owned(),
            ));
        }
        Ok(())
    }

    pub fn unary<Req, Resp, F>(mut self, path: impl Into<String>, f: F) -> Self
    where
        Req: Message + Default + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
        F: Fn(GrpcRequest<Req>) -> Result<GrpcResponse<Resp>, GrpcStatus> + Send + Sync + 'static,
    {
        self.unary.insert(
            path.into(),
            Box::new(UnaryHandler::<Req, Resp, F> {
                f,
                _types: PhantomData,
            }),
        );
        self
    }

    pub fn server_streaming<Req, F>(mut self, path: impl Into<String>, f: F) -> Self
    where
        Req: Message + Default + Send + Sync + 'static,
        F: Fn(GrpcRequest<Req>) -> Result<GrpcServerStreamingResponse, GrpcStatus>
            + Send
            + Sync
            + 'static,
    {
        self.server_streaming.insert(
            path.into(),
            Box::new(ServerStreamingHandler::<Req, F> {
                f,
                _types: PhantomData,
            }),
        );
        self
    }

    pub fn server_streaming_buffered<Req, F>(mut self, path: impl Into<String>, f: F) -> Self
    where
        Req: Message + Default + Send + Sync + 'static,
        F: Fn(GrpcRequest<Req>) -> Result<GrpcBufferedServerStreamingResponse, GrpcStatus>
            + Send
            + Sync
            + 'static,
    {
        self.buffered_server_streaming.insert(
            path.into(),
            Box::new(BufferedServerStreamingHandler::<Req, F> {
                f,
                _types: PhantomData,
            }),
        );
        self
    }

    pub fn client_streaming<Req, Resp, F>(mut self, path: impl Into<String>, f: F) -> Self
    where
        Req: Message + Default + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
        F: Fn(GrpcClientStreamingRequest<Req>) -> Result<GrpcResponse<Resp>, GrpcStatus>
            + Send
            + Sync
            + 'static,
    {
        self.client_streaming.insert(
            path.into(),
            Box::new(ClientStreamingHandler::<Req, Resp, F> {
                f,
                _types: PhantomData,
            }),
        );
        self
    }

    pub fn streaming<Req, Resp, F>(mut self, path: impl Into<String>, f: F) -> Self
    where
        Req: Message + Default + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
        F: Fn(GrpcStreamingCall<Req, Resp>) -> Result<GrpcStreamingResponse<Resp>, GrpcStatus>
            + Send
            + Sync
            + 'static,
    {
        self.streaming.insert(
            path.into(),
            Box::new(StreamingHandler::<Req, Resp, F> {
                f,
                _types: PhantomData,
            }),
        );
        self
    }

    pub fn streaming_raw<Req, F>(mut self, path: impl Into<String>, f: F) -> Self
    where
        Req: Message + Default + Send + Sync + 'static,
        F: Fn(GrpcRawStreamingRequest<Req>) -> Result<GrpcRawStreamingResponse, GrpcStatus>
            + Send
            + Sync
            + 'static,
    {
        self.streaming_raw.insert(
            path.into(),
            Box::new(StreamingRawHandler::<Req, F> {
                f,
                _types: PhantomData,
            }),
        );
        self
    }

    fn response_for(&self, request: HttpRequest) -> HttpResponse {
        if request.method != Method::POST {
            return grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented));
        }
        if let Some(handler) = self.unary.get(&*request.path) {
            return handler.call(request, self.limits);
        }
        if let Some(handler) = self.client_streaming.get(&*request.path) {
            return handler.call(request, self.limits);
        }
        if let Some(handler) = self.streaming.get(&*request.path) {
            return handler.call(request, self.limits);
        }
        if let Some(handler) = self.streaming_raw.get(&*request.path) {
            return handler.call(request, self.limits);
        }
        if let Some(handler) = self.buffered_server_streaming.get(&*request.path) {
            return handler.call(request, self.limits);
        }
        let Some(handler) = self.server_streaming.get(&*request.path) else {
            return grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented));
        };
        handler.call(request, self.limits)
    }

    fn response_for_http2(&self, request: GrpcHttp2Request) -> HttpResponse {
        if request.method != Method::POST {
            return grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented));
        }
        if let Some(handler) = self.unary.get(&*request.path) {
            return handler.call_http2(request, self.limits);
        }
        if let Some(handler) = self.client_streaming.get(&*request.path) {
            return handler.call_http2(request, self.limits);
        }
        if let Some(handler) = self.streaming.get(&*request.path) {
            return handler.call_http2(request, self.limits);
        }
        if let Some(handler) = self.streaming_raw.get(&*request.path) {
            return handler.call_http2(request, self.limits);
        }
        if let Some(handler) = self.buffered_server_streaming.get(&*request.path) {
            return handler.call_http2(request, self.limits);
        }
        let Some(handler) = self.server_streaming.get(&*request.path) else {
            return grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented));
        };
        handler.call_http2(request, self.limits)
    }

    fn next_actor_route_id(&mut self) -> u64 {
        loop {
            let id = self.next_pending_id;
            self.next_pending_id = self.next_pending_id.wrapping_add(1).max(1);
            if !self.actor_pending.contains_key(&id) && !self.pending.contains_key(&id) {
                return id;
            }
        }
    }

    fn start_actor_public(
        &mut self,
        request: HttpRequest,
        call: tina::RequestContext<HttpResponse>,
    ) -> Effect<Self> {
        if request.method != Method::POST {
            return reply_to(
                call,
                grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented)),
            );
        }
        if self.actor_pending.len() >= self.actor_route_capacity {
            return reply_to(
                call,
                grpc_http_response(Vec::new(), GrpcActorRouteFailure::Full.status()),
            );
        }
        let id = self.next_actor_route_id();
        let dispatch = if let Some(handler) = self.actor_unary.get(&*request.path) {
            handler.call(request, self.limits, id)
        } else if let Some(handler) = self.actor_streaming.get(&*request.path) {
            handler.call(request, self.limits, id)
        } else {
            return reply_to(
                call,
                grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented)),
            );
        };
        self.admit_actor_dispatch(id, call, dispatch)
    }

    fn start_actor_http2(
        &mut self,
        request: GrpcHttp2Request,
        call: tina::RequestContext<HttpResponse>,
    ) -> Effect<Self> {
        if request.method != Method::POST {
            return reply_to(
                call,
                grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented)),
            );
        }
        if self.actor_pending.len() >= self.actor_route_capacity {
            return reply_to(
                call,
                grpc_http_response(Vec::new(), GrpcActorRouteFailure::Full.status()),
            );
        }
        let id = self.next_actor_route_id();
        let dispatch = if let Some(handler) = self.actor_unary.get(&*request.path) {
            handler.call_http2(request, self.limits, id)
        } else if let Some(handler) = self.actor_streaming.get(&*request.path) {
            handler.call_http2(request, self.limits, id)
        } else {
            return reply_to(
                call,
                grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented)),
            );
        };
        self.admit_actor_dispatch(id, call, dispatch)
    }

    fn admit_actor_dispatch(
        &mut self,
        id: u64,
        call: tina::RequestContext<HttpResponse>,
        dispatch: ActorRouteDispatchResult<S>,
    ) -> Effect<Self> {
        match dispatch {
            Ok(dispatch) => {
                self.actor_pending.insert(
                    id,
                    PendingActorRoute {
                        call,
                        cancel: dispatch.cancel,
                    },
                );
                dispatch.effect
            }
            Err(response) => reply_to(call, *response),
        }
    }

    fn complete_actor_route(&mut self, id: u64, result: ActorRouteResult) -> Effect<Self> {
        let Some(pending) = self.actor_pending.remove(&id) else {
            return cancel_actor_stream_response(result);
        };
        if !pending.call.is_open() {
            return cancel_actor_stream_response(result);
        }
        match result {
            ActorRouteResult::Response(response) => reply_to(pending.call, response),
            ActorRouteResult::Failure(failure) => reply_to(
                pending.call,
                grpc_http_response(Vec::new(), failure.status()),
            ),
        }
    }

    fn cancel_abandoned_actor_routes(&mut self) -> Effect<Self> {
        let abandoned: Vec<u64> = self
            .actor_pending
            .iter()
            .filter_map(|(id, pending)| (!pending.call.is_open()).then_some(*id))
            .collect();
        let effects = abandoned.into_iter().filter_map(|id| {
            self.actor_pending
                .remove(&id)
                .map(|pending| pending.cancel.cancel(id))
        });
        batch(effects)
    }

    fn start_or_reply_request(
        &mut self,
        request: HttpRequest,
        call_ctx: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match &request.body {
            HttpRequestBody::Buffered(_) => {
                if self.actor_unary.contains_key(&request.path) {
                    return self.start_actor_public(request, call_ctx.into_request_context());
                }
                call_ctx.reply(self.response_for(request))
            }
            HttpRequestBody::Stream(_) => call_ctx.reply(grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            )),
            HttpRequestBody::Http2Stream(stream) => {
                if self.actor_streaming.contains_key(&request.path) {
                    return self.start_actor_public(request, call_ctx.into_request_context());
                }
                if self.streaming.contains_key(&request.path)
                    || self.streaming_raw.contains_key(&request.path)
                {
                    return call_ctx.reply(self.response_for(request));
                }
                let id = self.next_pending_id;
                self.next_pending_id = self.next_pending_id.saturating_add(1);
                let source = stream.source;
                let stream_id = stream.stream_id;
                let request_context = call_ctx.into_request_context();
                self.pending.insert(
                    id,
                    PendingGrpcRequest::Public {
                        request,
                        body: Vec::new(),
                        call: request_context,
                    },
                );
                call(
                    source,
                    crate::Http2ConnectionMsg::body_next(stream_id),
                    REQUEST_BODY_PULL_TIMEOUT,
                )
                .then(move |outcome| GrpcRouterMsg::RequestBodyChunk { id, outcome })
            }
        }
    }

    fn start_or_reply_http2_request(
        &mut self,
        request: GrpcHttp2Request,
        call_ctx: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match &request.body {
            GrpcHttp2Body::Buffered(_) => {
                if self.actor_unary.contains_key(&*request.path) {
                    return self.start_actor_http2(request, call_ctx.into_request_context());
                }
                call_ctx.reply(self.response_for_http2(request))
            }
            GrpcHttp2Body::Unsupported => call_ctx.reply(grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            )),
            GrpcHttp2Body::Http2Stream(_) => {
                // streaming / raw-streaming routes consume the request stream
                // directly and reply with a streaming response source. They do
                // not accumulate the body, so dispatch them synchronously via
                // the compact entry point — no public `HttpRequest` rebuild.
                if self.actor_streaming.contains_key(&*request.path) {
                    return self.start_actor_http2(request, call_ctx.into_request_context());
                }
                if self.streaming.contains_key(&*request.path)
                    || self.streaming_raw.contains_key(&*request.path)
                {
                    return call_ctx.reply(self.response_for_http2(request));
                }
                // Other routes over a streamed body (e.g. client-streaming)
                // accumulate the framed body, then dispatch buffered. Keep the
                // compact gRPC facts instead of a public `HttpRequest`.
                let GrpcHttp2Request {
                    method,
                    path,
                    body,
                    content_type_ok,
                    unsupported_encoding,
                } = request;
                let GrpcHttp2Body::Http2Stream(stream) = body else {
                    // The outer match guarantees Http2Stream here.
                    return call_ctx.reply(grpc_http_response(
                        Vec::new(),
                        GrpcStatus::new(GrpcStatusCode::Internal),
                    ));
                };
                let id = self.next_pending_id;
                self.next_pending_id = self.next_pending_id.saturating_add(1);
                let source = stream.source;
                let stream_id = stream.stream_id;
                let request_context = call_ctx.into_request_context();
                self.pending.insert(
                    id,
                    PendingGrpcRequest::Http2 {
                        method,
                        path,
                        content_type_ok,
                        unsupported_encoding,
                        stream,
                        body: Vec::new(),
                        call: request_context,
                    },
                );
                call(
                    source,
                    crate::Http2ConnectionMsg::body_next(stream_id),
                    REQUEST_BODY_PULL_TIMEOUT,
                )
                .then(move |outcome| GrpcRouterMsg::RequestBodyChunk { id, outcome })
            }
        }
    }

    fn handle_request_chunk(
        &mut self,
        id: u64,
        outcome: CallOutcome<Http2ConnectionReply>,
    ) -> Effect<Self> {
        let Some(pending) = self.pending.remove(&id) else {
            return noop();
        };
        // Classify the chunk outcome once; both pending shapes share the same
        // body-pull contract (more bytes, clean EOF, or a failure mapped to a
        // gRPC status).
        match classify_request_chunk(outcome) {
            RequestChunkAction::More(bytes) => match pending {
                PendingGrpcRequest::Public {
                    request,
                    mut body,
                    call,
                } => {
                    let HttpRequestBody::Http2Stream(stream) = &request.body else {
                        return reply_to(
                            call,
                            grpc_http_response(
                                Vec::new(),
                                GrpcStatus::new(GrpcStatusCode::Internal),
                            ),
                        );
                    };
                    let source = stream.source;
                    let stream_id = stream.stream_id;
                    body.extend_from_slice(&bytes);
                    self.pending.insert(
                        id,
                        PendingGrpcRequest::Public {
                            request,
                            body,
                            call,
                        },
                    );
                    self.pull_next_chunk(id, source, stream_id)
                }
                PendingGrpcRequest::Http2 {
                    method,
                    path,
                    content_type_ok,
                    unsupported_encoding,
                    stream,
                    mut body,
                    call,
                } => {
                    let source = stream.source;
                    let stream_id = stream.stream_id;
                    body.extend_from_slice(&bytes);
                    self.pending.insert(
                        id,
                        PendingGrpcRequest::Http2 {
                            method,
                            path,
                            content_type_ok,
                            unsupported_encoding,
                            stream,
                            body,
                            call,
                        },
                    );
                    self.pull_next_chunk(id, source, stream_id)
                }
            },
            RequestChunkAction::Eof => match pending {
                PendingGrpcRequest::Public {
                    mut request,
                    body,
                    call,
                } => {
                    request.body = HttpRequestBody::Buffered(body);
                    if self.actor_unary.contains_key(&request.path) {
                        self.start_actor_public(request, call)
                    } else {
                        reply_to(call, self.response_for(request))
                    }
                }
                PendingGrpcRequest::Http2 {
                    method,
                    path,
                    content_type_ok,
                    unsupported_encoding,
                    body,
                    call,
                    ..
                } => {
                    let request = GrpcHttp2Request {
                        method,
                        path,
                        body: GrpcHttp2Body::Buffered(body),
                        content_type_ok,
                        unsupported_encoding,
                    };
                    if self.actor_unary.contains_key(&*request.path) {
                        self.start_actor_http2(request, call)
                    } else {
                        reply_to(call, self.response_for_http2(request))
                    }
                }
            },
            RequestChunkAction::Failed(status) => {
                reply_to(pending.into_call(), grpc_http_response(Vec::new(), status))
            }
        }
    }

    /// Pull the next request body chunk for a pending streamed request.
    fn pull_next_chunk(
        &self,
        id: u64,
        source: tina::Address<crate::Http2ConnectionMsg, crate::Http2ConnectionReply>,
        stream_id: u32,
    ) -> Effect<Self> {
        call(
            source,
            crate::Http2ConnectionMsg::body_next(stream_id),
            REQUEST_BODY_PULL_TIMEOUT,
        )
        .then(move |outcome| GrpcRouterMsg::RequestBodyChunk { id, outcome })
    }
}

fn cancel_actor_stream_response<S: Shard + 'static>(
    result: ActorRouteResult,
) -> Effect<GrpcRouter<S>> {
    let ActorRouteResult::Response(response) = result else {
        return noop();
    };
    let source = match response.body {
        HttpResponseBody::Stream(stream) => Some(stream.source),
        HttpResponseBody::ChunkedStream(stream) => Some(stream.source),
        HttpResponseBody::Buffered(_)
        | HttpResponseBody::Shared(_)
        | HttpResponseBody::WebSocket(_) => None,
    };
    source.map_or_else(noop, |source| send(source, ResponseChunkMsg::Cancel))
}

impl PendingGrpcRequest {
    /// The reply obligation, regardless of which entry path produced it.
    fn into_call(self) -> tina::RequestContext<HttpResponse> {
        match self {
            PendingGrpcRequest::Public { call, .. } => call,
            PendingGrpcRequest::Http2 { call, .. } => call,
        }
    }
}

/// What a request-body-pull outcome means for an accumulating streamed request.
enum RequestChunkAction {
    /// More body bytes arrived; keep pulling.
    More(Vec<u8>),
    /// Clean end of the request body.
    Eof,
    /// The pull failed; reply with this gRPC status.
    Failed(GrpcStatus),
}

fn classify_request_chunk(outcome: CallOutcome<Http2ConnectionReply>) -> RequestChunkAction {
    match outcome {
        CallOutcome::Replied(Http2ConnectionReply::RequestChunk(RequestChunkReply::Chunk(
            bytes,
        ))) => RequestChunkAction::More(bytes),
        CallOutcome::Replied(Http2ConnectionReply::RequestChunk(RequestChunkReply::Eof)) => {
            RequestChunkAction::Eof
        }
        CallOutcome::Replied(Http2ConnectionReply::RequestChunk(RequestChunkReply::Error(_)))
        | CallOutcome::Replied(Http2ConnectionReply::Report(_))
        | CallOutcome::Replied(Http2ConnectionReply::RequestChunk(
            RequestChunkReply::WebSocketSend(_),
        ))
        | CallOutcome::Replied(Http2ConnectionReply::RequestChunk(
            RequestChunkReply::WebSocketReport(_),
        )) => RequestChunkAction::Failed(GrpcStatus::new(GrpcStatusCode::Internal)),
        CallOutcome::Full => {
            RequestChunkAction::Failed(GrpcStatus::new(GrpcStatusCode::ResourceExhausted))
        }
        CallOutcome::Closed | CallOutcome::Rejected(_) => {
            RequestChunkAction::Failed(GrpcStatus::new(GrpcStatusCode::Cancelled))
        }
        CallOutcome::Timeout => {
            RequestChunkAction::Failed(GrpcStatus::new(GrpcStatusCode::DeadlineExceeded))
        }
    }
}

impl<S: Shard + 'static> Isolate for GrpcRouter<S> {
    tina::isolate_types! {
        message: GrpcRouterMsg,
        reply: HttpResponse,
        send: tina::Outbound<ResponseChunkMsg>,
        spawn: Infallible,
        io: tina_runtime::RuntimeCall<GrpcRouterMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: GrpcRouterMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        let cleanup = self.cancel_abandoned_actor_routes();
        let current = match msg {
            GrpcRouterMsg::Request(request) => reply(self.response_for(request)),
            GrpcRouterMsg::Http2Request(request) => reply(self.response_for_http2(request)),
            GrpcRouterMsg::RequestBodyChunk { id, outcome } => {
                self.handle_request_chunk(id, outcome)
            }
            GrpcRouterMsg::ActorRouteReturned { id, result } => {
                self.complete_actor_route(id, result)
            }
            GrpcRouterMsg::ActorRouteCancelled { id: _, outcome: _ } => noop(),
        };
        batch([cleanup, current])
    }

    fn handle_call(
        &mut self,
        msg: GrpcRouterMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        let cleanup = self.cancel_abandoned_actor_routes();
        let current = match msg {
            GrpcRouterMsg::Request(request) => self.start_or_reply_request(request, call),
            GrpcRouterMsg::Http2Request(request) => {
                self.start_or_reply_http2_request(request, call)
            }
            GrpcRouterMsg::RequestBodyChunk { .. }
            | GrpcRouterMsg::ActorRouteReturned { .. }
            | GrpcRouterMsg::ActorRouteCancelled { .. } => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        };
        batch([cleanup, current])
    }
}

pub fn decode_unary_request<T: Message + Default>(
    request: &HttpRequest,
    limits: GrpcLimits,
) -> Result<T, GrpcError> {
    decode_unary_parts(&request.headers, &request.body, limits)
}

fn validate_grpc_request_headers(headers: &HeaderMap) -> Result<(), GrpcError> {
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !is_grpc_content_type(content_type) {
        return Err(GrpcError::BadContentType);
    }
    let unsupported_encoding = headers
        .get("grpc-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.eq_ignore_ascii_case("identity"));
    if unsupported_encoding {
        return Err(GrpcError::CompressedUnsupported);
    }
    Ok(())
}

fn validate_grpc_header_flags(
    content_type_ok: bool,
    unsupported_encoding: bool,
) -> Result<(), GrpcError> {
    if !content_type_ok {
        return Err(GrpcError::BadContentType);
    }
    if unsupported_encoding {
        return Err(GrpcError::CompressedUnsupported);
    }
    Ok(())
}

fn decode_unary_parts<T: Message + Default>(
    headers: &HeaderMap,
    body: &HttpRequestBody,
    limits: GrpcLimits,
) -> Result<T, GrpcError> {
    validate_grpc_request_headers(headers)?;
    let body = body.as_buffered().ok_or(GrpcError::BadFrame)?;
    decode_unary_body(body, limits)
}

fn decode_unary_body_with_flags<T: Message + Default>(
    body: &[u8],
    content_type_ok: bool,
    unsupported_encoding: bool,
    limits: GrpcLimits,
) -> Result<T, GrpcError> {
    validate_grpc_header_flags(content_type_ok, unsupported_encoding)?;
    decode_unary_body(body, limits)
}

fn decode_unary_body<T: Message + Default>(
    body: &[u8],
    limits: GrpcLimits,
) -> Result<T, GrpcError> {
    let mut cursor = 0;
    let message = decode_one_grpc_message::<T>(body, &mut cursor, limits)?;
    if cursor != body.len() {
        return Err(GrpcError::BadFrame);
    }
    Ok(message)
}

pub fn decode_streaming_request<T: Message + Default>(
    request: &HttpRequest,
    limits: GrpcLimits,
) -> Result<Vec<T>, GrpcError> {
    decode_streaming_parts(&request.headers, &request.body, limits)
}

fn decode_streaming_parts<T: Message + Default>(
    headers: &HeaderMap,
    body: &HttpRequestBody,
    limits: GrpcLimits,
) -> Result<Vec<T>, GrpcError> {
    validate_grpc_request_headers(headers)?;
    let body = body.as_buffered().ok_or(GrpcError::BadFrame)?;
    decode_streaming_body(body, limits)
}

fn decode_streaming_body_with_flags<T: Message + Default>(
    body: &[u8],
    content_type_ok: bool,
    unsupported_encoding: bool,
    limits: GrpcLimits,
) -> Result<Vec<T>, GrpcError> {
    validate_grpc_header_flags(content_type_ok, unsupported_encoding)?;
    decode_streaming_body(body, limits)
}

fn decode_streaming_body<T: Message + Default>(
    body: &[u8],
    limits: GrpcLimits,
) -> Result<Vec<T>, GrpcError> {
    let mut cursor = 0;
    let mut messages = Vec::new();
    while cursor < body.len() {
        // Bound the count, mirroring the encode side; a body of tiny frames
        // would otherwise materialize an unbounded `Vec<Req>`.
        if messages.len() >= limits.max_messages {
            return Err(GrpcError::TooManyMessages {
                count: messages.len() + 1,
                max: limits.max_messages,
            });
        }
        messages.push(decode_one_grpc_message::<T>(body, &mut cursor, limits)?);
    }
    Ok(messages)
}

/// Frame one protobuf message as a length-prefixed gRPC message in a fresh
/// `Vec`. A convenience wrapper over [`encode_grpc_message_into`]; callers that
/// frame several messages, or that reuse a scratch buffer across calls, should
/// use `encode_grpc_message_into` to append into storage they own.
pub fn encode_grpc_message<T: Message>(
    message: &T,
    limits: GrpcLimits,
) -> Result<Vec<u8>, GrpcError> {
    // Start empty so an over-cap message is rejected before any allocation;
    // `encode_grpc_message_into` reserves the exact size after its cap check.
    let mut out = Vec::new();
    encode_grpc_message_into(&mut out, message, limits)?;
    Ok(out)
}

/// Append one length-prefixed gRPC message onto `out`, the reusable framing
/// primitive. The caller owns `out` and may reuse its capacity across calls or
/// pack several messages into one buffer (e.g. a buffered server-streaming
/// body, or a client-streaming source that batches messages).
///
/// The message-size cap is enforced **before** any framing bytes are written,
/// so an over-cap message fails with [`GrpcError::EncodeTooLarge`] and leaves
/// `out` untouched.
///
/// The reuse helps for multi-message framing and caller-held scratch buffers. A
/// single body that is moved into a message crossing an isolate boundary travels
/// with the message and cannot be pool-reused.
pub fn encode_grpc_message_into<T: Message>(
    out: &mut Vec<u8>,
    message: &T,
    limits: GrpcLimits,
) -> Result<(), GrpcError> {
    let len = message.encoded_len();
    // The gRPC length prefix is a 4-byte unsigned int, so the wire cap is
    // u32::MAX no matter how large the configured limit is — checking only the
    // configured cap would let an oversized message truncate its length on the
    // wire. Enforce the smaller of the two.
    let max = limits.max_message_bytes.min(u32::MAX as usize);
    if len > max {
        return Err(GrpcError::EncodeTooLarge { len, max });
    }
    // Cap is satisfied (len <= u32::MAX), so the length cast below is exact and
    // the framed size cannot overflow. Reserve only now, so no large buffer is
    // allocated for an over-cap message.
    let framed = GRPC_FRAME_HEADER_LEN + len;
    out.reserve(framed);
    out.push(0);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    message.encode(out).map_err(|_| GrpcError::BadFrame)
}

pub(crate) fn decode_one_grpc_message<T: Message + Default>(
    body: &[u8],
    cursor: &mut usize,
    limits: GrpcLimits,
) -> Result<T, GrpcError> {
    if body.len().saturating_sub(*cursor) < GRPC_FRAME_HEADER_LEN {
        return Err(GrpcError::BadFrame);
    }
    let compressed = body[*cursor];
    *cursor += 1;
    if compressed != 0 {
        return Err(GrpcError::CompressedUnsupported);
    }
    let len = u32::from_be_bytes([
        body[*cursor],
        body[*cursor + 1],
        body[*cursor + 2],
        body[*cursor + 3],
    ]) as usize;
    *cursor += 4;
    if len > limits.max_message_bytes {
        return Err(GrpcError::MessageTooLarge {
            len,
            max: limits.max_message_bytes,
        });
    }
    let end = cursor.checked_add(len).ok_or(GrpcError::BadFrame)?;
    if end > body.len() {
        return Err(GrpcError::BadFrame);
    }
    let decoded = T::decode(&body[*cursor..end]).map_err(|_| GrpcError::Decode)?;
    *cursor = end;
    Ok(decoded)
}

pub(crate) fn grpc_status_http_response(status: GrpcStatus) -> HttpResponse {
    grpc_http_response(Vec::new(), status)
}

pub fn grpc_status_trailers(status: GrpcStatus) -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    insert_grpc_status_headers(&mut headers, status);
    headers
}

pub(crate) fn grpc_status_trailers_block(status: GrpcStatus) -> Vec<u8> {
    let mut block = Vec::with_capacity(if status.message.is_some() { 64 } else { 16 });
    literal(
        "grpc-status",
        grpc_status_header_str(status.code),
        &mut block,
    );
    if let Some(message) = status.message {
        literal(
            "grpc-message",
            &percent_encode_grpc_message(&message),
            &mut block,
        );
    }
    block
}

fn grpc_http_response(body: Vec<u8>, status: GrpcStatus) -> HttpResponse {
    let mut response = HttpResponse::with_body(StatusCode::OK, body);
    response.headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc+proto"),
    );
    insert_grpc_status_headers(&mut response.headers, status);
    response
}

fn grpc_http_response_shared(body: Arc<[u8]>, status: GrpcStatus) -> HttpResponse {
    let mut response = HttpResponse::with_shared_body(StatusCode::OK, body);
    response.headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc+proto"),
    );
    insert_grpc_status_headers(&mut response.headers, status);
    response
}

fn grpc_streaming_http_response(
    source: tina::Address<ResponseChunkMsg, ResponseChunkReply>,
    status: GrpcStatus,
) -> HttpResponse {
    let mut response = HttpResponse::stream_chunked(StatusCode::OK, source);
    response.headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc+proto"),
    );
    insert_grpc_status_headers(&mut response.headers, status);
    response
}

fn insert_grpc_status_headers(headers: &mut http::HeaderMap, status: GrpcStatus) {
    headers.insert(
        http::HeaderName::from_static("grpc-status"),
        grpc_status_header_value(status.code),
    );
    if let Some(message) = status.message {
        headers.insert(
            http::HeaderName::from_static("grpc-message"),
            HeaderValue::from_str(&percent_encode_grpc_message(&message))
                .unwrap_or_else(|_| HeaderValue::from_static("invalid")),
        );
    }
}

fn grpc_status_header_value(code: GrpcStatusCode) -> HeaderValue {
    HeaderValue::from_static(grpc_status_header_str(code))
}

fn grpc_status_header_str(code: GrpcStatusCode) -> &'static str {
    match code {
        GrpcStatusCode::Ok => "0",
        GrpcStatusCode::Cancelled => "1",
        GrpcStatusCode::Unknown => "2",
        GrpcStatusCode::InvalidArgument => "3",
        GrpcStatusCode::DeadlineExceeded => "4",
        GrpcStatusCode::NotFound => "5",
        GrpcStatusCode::AlreadyExists => "6",
        GrpcStatusCode::PermissionDenied => "7",
        GrpcStatusCode::ResourceExhausted => "8",
        GrpcStatusCode::FailedPrecondition => "9",
        GrpcStatusCode::Aborted => "10",
        GrpcStatusCode::OutOfRange => "11",
        GrpcStatusCode::Unimplemented => "12",
        GrpcStatusCode::Internal => "13",
        GrpcStatusCode::Unavailable => "14",
        GrpcStatusCode::DataLoss => "15",
        GrpcStatusCode::Unauthenticated => "16",
    }
}

fn status_for_error(error: GrpcError) -> GrpcStatus {
    match error {
        GrpcError::InvalidPath(_) | GrpcError::BadContentType => {
            GrpcStatus::new(GrpcStatusCode::InvalidArgument)
        }
        GrpcError::CompressedUnsupported => {
            GrpcStatus::with_message(GrpcStatusCode::Unimplemented, "compression unsupported")
        }
        GrpcError::MessageTooLarge { len, max } => GrpcStatus::with_message(
            GrpcStatusCode::ResourceExhausted,
            format!("request message {len} exceeds cap {max}"),
        ),
        GrpcError::BadFrame | GrpcError::Decode => GrpcStatus::new(GrpcStatusCode::InvalidArgument),
        GrpcError::EncodeTooLarge { .. } | GrpcError::TooManyMessages { .. } => {
            GrpcStatus::new(GrpcStatusCode::ResourceExhausted)
        }
        GrpcError::Status(status) => status,
        GrpcError::Io(_) | GrpcError::MissingTrailers => GrpcStatus::new(GrpcStatusCode::Unknown),
    }
}

fn percent_encode_grpc_message(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for byte in message.bytes() {
        if (0x20..=0x7e).contains(&byte) && byte != b'%' {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// Tiny blocking prior-knowledge h2c unary client helper.
///
/// This is a blocking specimen/test helper, not a pooled Tina client
/// service. It exists to prove the native bytes/status path without
/// introducing hyper, tonic, or Tokio.
///
/// It does not run inside a Tina isolate, does not emit runtime trace facts,
/// and is not part of deterministic replay. A future native Tina gRPC client
/// service should own received-status facts.
pub fn grpc_unary_call_h2c_blocking<Req, Resp>(
    target: SocketAddr,
    path: &str,
    request: &Req,
    timeout: Duration,
    limits: GrpcLimits,
) -> Result<Resp, GrpcError>
where
    Req: Message,
    Resp: Message + Default,
{
    let body = encode_grpc_message(request, limits)?;
    let mut stream = TcpStream::connect_timeout(&target, timeout)
        .map_err(|error| GrpcError::Io(error.to_string()))?;
    stream
        .set_nodelay(true)
        .map_err(|error| GrpcError::Io(error.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| GrpcError::Io(error.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| GrpcError::Io(error.to_string()))?;
    stream
        .write_all(CLIENT_PREFACE)
        .map_err(|error| GrpcError::Io(error.to_string()))?;
    write_frame(&mut stream, FRAME_SETTINGS, 0, 0, &[])?;
    finish_settings(&mut stream)?;

    let headers = grpc_request_headers(path);
    write_frame(&mut stream, FRAME_HEADERS, FLAG_END_HEADERS, 1, &headers)?;
    write_data_frames(&mut stream, 1, &body)?;

    let mut response_body = Vec::new();
    let mut trailers = None;
    for _ in 0..16 {
        let frame = read_frame(&mut stream)?;
        if frame.stream_id != 1 {
            continue;
        }
        match frame.ty {
            FRAME_HEADERS if frame.flags & FLAG_END_STREAM != 0 => {
                trailers = Some(frame.payload);
                break;
            }
            FRAME_HEADERS => {}
            FRAME_DATA => response_body.extend_from_slice(&frame.payload),
            _ => {}
        }
    }
    let trailers = trailers.ok_or(GrpcError::MissingTrailers)?;
    let status = decode_grpc_status_trailers(&trailers)?;
    if status.code != GrpcStatusCode::Ok {
        return Err(GrpcError::Status(status));
    }
    let mut cursor = 0;
    let response = decode_one_grpc_message::<Resp>(&response_body, &mut cursor, limits)?;
    if cursor != response_body.len() {
        return Err(GrpcError::BadFrame);
    }
    Ok(response)
}

#[derive(Debug)]
struct ClientFrame {
    ty: u8,
    flags: u8,
    stream_id: u32,
    payload: Vec<u8>,
}

fn finish_settings(stream: &mut TcpStream) -> Result<(), GrpcError> {
    let mut saw_settings = false;
    let mut saw_ack = false;
    for _ in 0..4 {
        let frame = read_frame(stream)?;
        if frame.ty == FRAME_SETTINGS && frame.flags & FLAG_ACK == 0 {
            saw_settings = true;
            write_frame(stream, FRAME_SETTINGS, FLAG_ACK, 0, &[])?;
        } else if frame.ty == FRAME_SETTINGS && frame.flags & FLAG_ACK != 0 {
            saw_ack = true;
        }
        if saw_settings && saw_ack {
            return Ok(());
        }
    }
    Err(GrpcError::Io(
        "HTTP/2 settings handshake did not finish".to_owned(),
    ))
}

fn write_frame(
    stream: &mut TcpStream,
    ty: u8,
    flags: u8,
    stream_id: u32,
    payload: &[u8],
) -> Result<(), GrpcError> {
    let len = payload.len();
    let mut out = Vec::with_capacity(9 + len);
    out.push(((len >> 16) & 0xff) as u8);
    out.push(((len >> 8) & 0xff) as u8);
    out.push((len & 0xff) as u8);
    out.push(ty);
    out.push(flags);
    out.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
    stream
        .write_all(&out)
        .map_err(|error| GrpcError::Io(error.to_string()))?;
    stream
        .flush()
        .map_err(|error| GrpcError::Io(error.to_string()))
}

fn write_data_frames(stream: &mut TcpStream, stream_id: u32, body: &[u8]) -> Result<(), GrpcError> {
    if body.is_empty() {
        return write_frame(stream, FRAME_DATA, FLAG_END_STREAM, stream_id, &[]);
    }
    let chunks = body.len().div_ceil(CLIENT_DATA_FRAME_PAYLOAD);
    for (idx, chunk) in body.chunks(CLIENT_DATA_FRAME_PAYLOAD).enumerate() {
        write_frame(
            stream,
            FRAME_DATA,
            if idx + 1 == chunks {
                FLAG_END_STREAM
            } else {
                0
            },
            stream_id,
            chunk,
        )?;
    }
    Ok(())
}

fn read_frame(stream: &mut TcpStream) -> Result<ClientFrame, GrpcError> {
    let mut head = [0_u8; 9];
    stream
        .read_exact(&mut head)
        .map_err(|error| GrpcError::Io(error.to_string()))?;
    let len = ((head[0] as usize) << 16) | ((head[1] as usize) << 8) | head[2] as usize;
    if len > CLIENT_MAX_INBOUND_FRAME_PAYLOAD {
        return Err(GrpcError::MessageTooLarge {
            len,
            max: CLIENT_MAX_INBOUND_FRAME_PAYLOAD,
        });
    }
    let mut payload = vec![0_u8; len];
    stream
        .read_exact(&mut payload)
        .map_err(|error| GrpcError::Io(error.to_string()))?;
    let mut sid = [0_u8; 4];
    sid.copy_from_slice(&head[5..9]);
    Ok(ClientFrame {
        ty: head[3],
        flags: head[4],
        stream_id: u32::from_be_bytes(sid) & 0x7fff_ffff,
        payload,
    })
}

fn grpc_request_headers(path: &str) -> Vec<u8> {
    let mut block = Vec::new();
    literal(":method", "POST", &mut block);
    literal(":scheme", "http", &mut block);
    literal(":path", path, &mut block);
    literal(":authority", "localhost", &mut block);
    literal("content-type", "application/grpc+proto", &mut block);
    literal("te", "trailers", &mut block);
    block
}

/// Read a [`GrpcStatus`] from an already-decoded header/trailer map (the
/// shape the native HTTP/2 client hands back), checking `grpc-status` and
/// optional `grpc-message`. Returns `None` when there is no `grpc-status`
/// at all, so the caller can tell "not a gRPC response" apart from a
/// malformed one.
pub(crate) fn grpc_status_from_header_map(headers: &http::HeaderMap) -> Option<GrpcStatus> {
    let raw = headers.get("grpc-status")?.to_str().ok()?;
    let code = GrpcStatusCode::from_u16(raw.trim().parse::<u16>().ok()?);
    let message = headers
        .get("grpc-message")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| percent_decode_grpc_message(v).ok());
    Some(GrpcStatus { code, message })
}

/// Build a [`GrpcStatus`] from the compact wire facts captured by the HTTP/2
/// client's gRPC-unary response path: a raw status code and the still
/// percent-encoded message. The same percent-decode as the header-map path, so
/// the compact and public client paths report identical status truth.
pub(crate) fn grpc_status_from_compact(code: u16, raw_message: Option<&str>) -> GrpcStatus {
    GrpcStatus {
        code: GrpcStatusCode::from_u16(code),
        message: raw_message.and_then(|m| percent_decode_grpc_message(m).ok()),
    }
}

fn decode_grpc_status_trailers(block: &[u8]) -> Result<GrpcStatus, GrpcError> {
    let mut cursor = 0;
    let mut status = None;
    let mut message = None;
    while cursor < block.len() {
        if block[cursor] != 0 {
            return Err(GrpcError::BadFrame);
        }
        cursor += 1;
        let (name, used) = hpack_string(&block[cursor..])?;
        cursor += used;
        let (value, used) = hpack_string(&block[cursor..])?;
        cursor += used;
        match name.as_str() {
            "grpc-status" => {
                status = value.parse::<u16>().ok().map(GrpcStatusCode::from_u16);
            }
            "grpc-message" => message = Some(percent_decode_grpc_message(&value)?),
            _ => {}
        }
    }
    Ok(GrpcStatus {
        code: status.ok_or(GrpcError::MissingTrailers)?,
        message,
    })
}

fn percent_decode_grpc_message(message: &str) -> Result<String, GrpcError> {
    let bytes = message.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(GrpcError::BadFrame);
            }
            let hi = from_hex(bytes[i + 1]).ok_or(GrpcError::BadFrame)?;
            let lo = from_hex(bytes[i + 2]).ok_or(GrpcError::BadFrame)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| GrpcError::BadFrame)
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn literal(name: &str, value: &str, out: &mut Vec<u8>) {
    out.push(0);
    encode_hpack_string(name, out);
    encode_hpack_string(value, out);
}

fn encode_hpack_string(value: &str, out: &mut Vec<u8>) {
    encode_hpack_integer(value.len(), out);
    out.extend_from_slice(value.as_bytes());
}

fn encode_hpack_integer(mut value: usize, out: &mut Vec<u8>) {
    if value < 127 {
        out.push(value as u8);
        return;
    }
    out.push(127);
    value -= 127;
    while value >= 128 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn hpack_string(input: &[u8]) -> Result<(String, usize), GrpcError> {
    if input.is_empty() || input[0] & 0x80 != 0 {
        return Err(GrpcError::BadFrame);
    }
    let (len, used) = hpack_integer(input)?;
    let end = used.checked_add(len).ok_or(GrpcError::BadFrame)?;
    if input.len() < end {
        return Err(GrpcError::BadFrame);
    }
    let value = std::str::from_utf8(&input[used..end]).map_err(|_| GrpcError::BadFrame)?;
    Ok((value.to_owned(), end))
}

fn hpack_integer(input: &[u8]) -> Result<(usize, usize), GrpcError> {
    if input.is_empty() || input[0] & 0x80 != 0 {
        return Err(GrpcError::BadFrame);
    }
    let mut value = (input[0] & 0x7f) as usize;
    if value < 127 {
        return Ok((value, 1));
    }
    let mut shift = 0usize;
    let mut used = 1usize;
    loop {
        let Some(byte) = input.get(used).copied() else {
            return Err(GrpcError::BadFrame);
        };
        used += 1;
        value = value
            .checked_add(((byte & 0x7f) as usize) << shift)
            .ok_or(GrpcError::BadFrame)?;
        if byte & 0x80 == 0 {
            return Ok((value, used));
        }
        shift = shift.checked_add(7).ok_or(GrpcError::BadFrame)?;
        if shift >= usize::BITS as usize {
            return Err(GrpcError::BadFrame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::convert::Infallible;

    use tina_runtime::{DefaultMailboxFactory, Runtime};

    #[derive(Clone, PartialEq, Message)]
    struct Ping {
        #[prost(uint64, tag = "1")]
        value: u64,
    }

    struct PingActor;

    #[tina_runtime::isolate(
        request = GrpcRequest<Ping>,
        reply = Result<GrpcResponse<Ping>, GrpcStatus>
    )]
    impl PingActor {
        fn handle_request(
            &mut self,
            request: GrpcRequest<Ping>,
            call: RequestCall<'_, Self>,
        ) -> RequestEffect<Self> {
            call.reply(Ok(GrpcResponse::new(request.message)))
        }
    }

    struct TestSource;

    #[tina_runtime::isolate(message = ResponseChunkMsg, reply = ResponseChunkReply)]
    impl TestSource {
        fn handle(
            &mut self,
            message: ResponseChunkMsg,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            match message {
                ResponseChunkMsg::Cancel => stop(),
                ResponseChunkMsg::Next | ResponseChunkMsg::Http2RequestChunk(_) => {
                    reply(ResponseChunkReply::Eof)
                }
            }
        }

        fn handle_call(
            &mut self,
            message: ResponseChunkMsg,
            call: tina::CallContext<'_, Self>,
        ) -> Effect<Self> {
            match message {
                ResponseChunkMsg::Cancel => stop(),
                ResponseChunkMsg::Next | ResponseChunkMsg::Http2RequestChunk(_) => {
                    call.reply(ResponseChunkReply::Eof)
                }
            }
        }
    }

    fn closed_request_context<R: 'static>(slot_id: u64) -> tina::RequestContext<R> {
        use std::any::TypeId;
        use std::sync::Arc;

        let shared = Arc::new(tina::DeferredSlotShared::new(slot_id, TypeId::of::<R>()));
        shared.set_state(tina::DeferredSlotState::Closed);
        let deferred = tina::runtime_internal::deferred_from_handle(
            tina::runtime_internal::handle_from_shared(shared),
        );
        tina::runtime_internal::request_context_from_deferred(deferred)
    }

    fn open_request_context<R: 'static>(slot_id: u64) -> tina::RequestContext<R> {
        use std::any::TypeId;
        use std::sync::Arc;

        let shared = Arc::new(tina::DeferredSlotShared::new(slot_id, TypeId::of::<R>()));
        let deferred = tina::runtime_internal::deferred_from_handle(
            tina::runtime_internal::handle_from_shared(shared),
        );
        tina::runtime_internal::request_context_from_deferred(deferred)
    }

    fn pending_handle<R: 'static>() -> tina::CallHandle<R> {
        use std::any::TypeId;
        use std::sync::Arc;

        tina::runtime_internal::call_handle_from_shared(Arc::new(tina::CallHandleShared::new(
            TypeId::of::<R>(),
        )))
    }

    #[test]
    fn actor_route_failures_keep_distinct_wire_statuses() {
        assert_eq!(
            GrpcActorRouteFailure::Full.status().code,
            GrpcStatusCode::ResourceExhausted
        );
        assert_eq!(
            GrpcActorRouteFailure::Closed.status().code,
            GrpcStatusCode::Unavailable
        );
        assert_eq!(
            GrpcActorRouteFailure::Timeout.status().code,
            GrpcStatusCode::DeadlineExceeded
        );
        assert_eq!(
            GrpcActorRouteFailure::Rejected(tina::CallRejectedReason::UnsupportedMessage)
                .status()
                .code,
            GrpcStatusCode::FailedPrecondition
        );
    }

    #[test]
    fn actor_route_registration_rejects_zero_capacity_and_duplicate_paths() {
        let zero =
            GrpcRouter::<SingleShard>::new(GrpcLimits::default()).with_actor_route_capacity(0);
        assert!(matches!(
            zero,
            Err(GrpcActorRouteRegistrationError::ZeroPendingCapacity)
        ));

        let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
        let target = runtime
            .register_request_service::<PingActor, GrpcRequest<Ping>, Infallible>(PingActor, 4);
        let router = GrpcRouter::<SingleShard>::new(GrpcLimits::default())
            .with_actor_route_capacity(1)
            .unwrap()
            .try_unary_actor("/pkg.Ping/Get", target, Duration::from_secs(1))
            .unwrap();
        let duplicate = router.try_unary_actor("/pkg.Ping/Get", target, Duration::from_secs(1));
        assert!(matches!(
            duplicate,
            Err(GrpcActorRouteRegistrationError::DuplicatePath(path))
                if path == "/pkg.Ping/Get"
        ));
    }

    #[test]
    fn actor_unary_transport_outcomes_are_not_collapsed() {
        let cases = [
            (CallOutcome::Full, GrpcActorRouteFailure::Full),
            (CallOutcome::Closed, GrpcActorRouteFailure::Closed),
            (CallOutcome::Timeout, GrpcActorRouteFailure::Timeout),
            (
                CallOutcome::Rejected(tina::CallRejectedReason::UnsupportedMessage),
                GrpcActorRouteFailure::Rejected(tina::CallRejectedReason::UnsupportedMessage),
            ),
        ];
        for (outcome, expected) in cases {
            assert!(matches!(
                actor_unary_result::<Ping>(outcome, GrpcLimits::default()),
                ActorRouteResult::Failure(actual) if actual == expected
            ));
        }
    }

    #[test]
    fn caller_gone_reclaims_pending_capacity_and_cancels_actor_call() {
        let mut router = GrpcRouter::<SingleShard>::new(GrpcLimits::default())
            .with_actor_route_capacity(1)
            .unwrap();
        router.actor_pending.insert(
            9,
            PendingActorRoute {
                call: closed_request_context(9),
                cancel: Box::new(ActorCancel {
                    handle: pending_handle::<()>(),
                }),
            },
        );

        let effect = router.cancel_abandoned_actor_routes();
        assert!(router.actor_pending.is_empty());
        assert!(matches!(effect, Effect::Batch(effects) if effects.len() == 1));
    }

    #[test]
    fn actor_route_capacity_returns_resource_exhausted_before_dispatch() {
        let mut router = GrpcRouter::<SingleShard>::new(GrpcLimits::default())
            .with_actor_route_capacity(1)
            .unwrap();
        router.actor_pending.insert(
            1,
            PendingActorRoute {
                call: open_request_context(1),
                cancel: Box::new(ActorCancel {
                    handle: pending_handle::<()>(),
                }),
            },
        );
        let request = HttpRequest {
            method: Method::POST,
            path: "/pkg.Ping/Get".to_owned(),
            version: http::Version::HTTP_2,
            headers: HeaderMap::new(),
            body: HttpRequestBody::Buffered(Vec::new()),
        };
        let effect = router.start_actor_public(request, open_request_context(2));
        let Effect::ReplyTo(_, response) = effect else {
            panic!("full actor route must reply immediately");
        };
        assert_eq!(
            response
                .headers
                .get("grpc-status")
                .unwrap()
                .to_str()
                .unwrap(),
            GrpcStatusCode::ResourceExhausted.as_u16().to_string()
        );
        assert_eq!(router.actor_pending.len(), 1);
    }

    #[test]
    fn stale_stream_completion_cancels_the_returned_source() {
        let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
        let source = runtime.register_with_capacity::<TestSource, Infallible>(TestSource, 4);
        let mut router = GrpcRouter::<SingleShard>::new(GrpcLimits::default());
        let response = grpc_streaming_http_response(source, GrpcStatus::ok());

        let effect = router.complete_actor_route(404, ActorRouteResult::Response(response));
        let Effect::Send(outbound) = effect else {
            panic!("stale stream response must be cancelled");
        };
        let (destination, message) = outbound.into_parts();
        assert_eq!(destination.system(), source.system());
        assert_eq!(destination.shard(), source.shard());
        assert_eq!(destination.isolate(), source.isolate());
        assert_eq!(destination.generation(), source.generation());
        assert!(matches!(message, ResponseChunkMsg::Cancel));
    }

    #[test]
    fn grpc_frame_round_trips_one_message() {
        let encoded = encode_grpc_message(&Ping { value: 7 }, GrpcLimits::default()).unwrap();
        let request = HttpRequest {
            method: Method::POST,
            path: "/pkg.Ping/Get".to_owned(),
            version: http::Version::HTTP_2,
            headers: {
                let mut headers = http::HeaderMap::new();
                headers.insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/grpc"),
                );
                headers
            },
            body: crate::HttpRequestBody::Buffered(encoded),
        };
        let decoded = decode_unary_request::<Ping>(&request, GrpcLimits::default()).unwrap();
        assert_eq!(decoded.value, 7);
    }

    #[test]
    fn decode_streaming_body_rejects_too_many_messages() {
        // Client-streaming decode must bound the message COUNT, not just
        // total bytes. A body packed with tiny empty frames overshoots the
        // count cap even though each frame is well under the byte limit.
        let limits = GrpcLimits::default();
        let frames = limits.max_messages + 1;
        let mut body = Vec::with_capacity(frames * GRPC_FRAME_HEADER_LEN);
        for _ in 0..frames {
            body.extend_from_slice(&[0u8, 0, 0, 0, 0]); // 5-byte empty gRPC frame
        }
        let request = HttpRequest {
            method: Method::POST,
            path: "/pkg.Ping/Stream".to_owned(),
            version: http::Version::HTTP_2,
            headers: {
                let mut headers = http::HeaderMap::new();
                headers.insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/grpc"),
                );
                headers
            },
            body: crate::HttpRequestBody::Buffered(body),
        };
        let err = decode_streaming_request::<Ping>(&request, limits)
            .expect_err("count over cap must be rejected");
        assert!(
            matches!(err, GrpcError::TooManyMessages { max, .. } if max == limits.max_messages),
            "expected TooManyMessages, got {err:?}"
        );
    }

    #[test]
    fn grpc_status_trailers_block_handles_long_message() {
        let message = "x".repeat(200);
        let block = grpc_status_trailers_block(GrpcStatus::with_message(
            GrpcStatusCode::ResourceExhausted,
            message.clone(),
        ));
        let status = decode_grpc_status_trailers(&block).unwrap();
        assert_eq!(status.code, GrpcStatusCode::ResourceExhausted);
        assert_eq!(status.message.as_deref(), Some(message.as_str()));
    }

    fn classified_request_chunk_status(
        outcome: CallOutcome<Http2ConnectionReply>,
    ) -> GrpcStatusCode {
        match classify_request_chunk(outcome) {
            RequestChunkAction::Failed(status) => status.code,
            RequestChunkAction::More(_) | RequestChunkAction::Eof => {
                panic!("expected failed request chunk action")
            }
        }
    }

    #[test]
    fn request_chunk_outcome_taxonomy_keeps_runtime_causes_distinct() {
        assert_eq!(
            classified_request_chunk_status(CallOutcome::Full),
            GrpcStatusCode::ResourceExhausted
        );
        assert_eq!(
            classified_request_chunk_status(CallOutcome::Closed),
            GrpcStatusCode::Cancelled
        );
        assert_eq!(
            classified_request_chunk_status(CallOutcome::Rejected(
                tina::CallRejectedReason::UnsupportedMessage
            )),
            GrpcStatusCode::Cancelled
        );
        assert_eq!(
            classified_request_chunk_status(CallOutcome::Timeout),
            GrpcStatusCode::DeadlineExceeded
        );
    }

    #[test]
    fn buffered_server_streaming_from_messages_rejects_oversized_message() {
        let limits = GrpcBufferedStreamLimits::new(
            GrpcLimits {
                max_message_bytes: 1,
                ..Default::default()
            },
            4,
            1024,
        );
        let err =
            GrpcBufferedServerStreamingResponse::from_messages([Ping { value: u64::MAX }], limits)
                .expect_err("message exceeds cap");

        assert!(matches!(err, GrpcError::EncodeTooLarge { max: 1, .. }));
    }

    #[test]
    fn encode_grpc_message_into_rejects_over_cap_before_allocating() {
        let limits = GrpcLimits {
            max_message_bytes: 1,
            ..Default::default()
        };
        let mut out = Vec::new();
        let err = encode_grpc_message_into(&mut out, &Ping { value: u64::MAX }, limits)
            .expect_err("message exceeds cap");
        assert!(matches!(err, GrpcError::EncodeTooLarge { max: 1, .. }));
        // The cap must bound real work: nothing framed, and — the bug this
        // guards — no buffer reserved before the check.
        assert!(out.is_empty());
        assert_eq!(out.capacity(), 0, "must not allocate before the cap check");
    }

    #[test]
    fn encode_grpc_message_rejects_over_cap() {
        let limits = GrpcLimits {
            max_message_bytes: 1,
            ..Default::default()
        };
        let err = encode_grpc_message(&Ping { value: u64::MAX }, limits)
            .expect_err("message exceeds cap");
        assert!(matches!(err, GrpcError::EncodeTooLarge { max: 1, .. }));
    }

    // The gRPC length prefix is u32, so a message over u32::MAX cannot be framed
    // even with a larger configured cap; it must be rejected, not truncated. The
    // bug only exists where usize > u32, i.e. 64-bit targets.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn encode_grpc_message_rejects_message_over_u32_wire_cap() {
        #[derive(Debug, Default)]
        struct HugeMessage;
        impl Message for HugeMessage {
            fn encode_raw(&self, _buf: &mut impl prost::bytes::BufMut) {
                unreachable!("must be rejected before encoding");
            }
            fn merge_field(
                &mut self,
                _tag: u32,
                _wire_type: prost::encoding::WireType,
                _buf: &mut impl prost::bytes::Buf,
                _ctx: prost::encoding::DecodeContext,
            ) -> Result<(), prost::DecodeError> {
                Ok(())
            }
            fn encoded_len(&self) -> usize {
                (u32::MAX as usize) + 1
            }
            fn clear(&mut self) {}
        }

        // Configured cap is above the wire cap; the wire cap must still win.
        let limits = GrpcLimits {
            max_message_bytes: usize::MAX,
            ..Default::default()
        };
        let mut out = Vec::new();
        let err = encode_grpc_message_into(&mut out, &HugeMessage, limits)
            .expect_err("message exceeds the u32 wire cap");
        assert!(
            matches!(err, GrpcError::EncodeTooLarge { max, .. } if max == u32::MAX as usize),
            "must report the wire cap, got {err:?}"
        );
        assert!(out.is_empty());
        assert_eq!(out.capacity(), 0, "must not allocate before rejecting");
    }

    #[test]
    fn buffered_server_streaming_from_messages_rejects_too_many_messages() {
        let limits = GrpcBufferedStreamLimits::new(GrpcLimits::default(), 1, 1024);
        let err = GrpcBufferedServerStreamingResponse::from_messages(
            [Ping { value: 1 }, Ping { value: 2 }],
            limits,
        )
        .expect_err("too many messages");

        assert!(matches!(
            err,
            GrpcError::TooManyMessages { count: 2, max: 1 }
        ));
    }

    #[test]
    fn buffered_server_streaming_from_messages_rejects_total_body_cap() {
        let limits = GrpcBufferedStreamLimits::new(GrpcLimits::default(), 4, 4);
        let err = GrpcBufferedServerStreamingResponse::from_messages([Ping { value: 1 }], limits)
            .expect_err("framed body exceeds cap");

        assert!(matches!(err, GrpcError::EncodeTooLarge { max: 4, .. }));
    }

    // -- next_grpc_frame_boundary: the extracted pure framing state machine --
    // (fuzzed directly in fuzz/fuzz_targets/grpc_frame.rs; these cover the
    // seed-corpus shapes plus the boundary math as deterministic unit tests.)

    #[test]
    fn frame_boundary_need_more_on_truncated_header() {
        // Fewer than 5 header bytes: must wait, must not touch the buffer.
        let mut buf = vec![0u8; GRPC_FRAME_HEADER_LEN - 1];
        let boundary = next_grpc_frame_boundary(&mut buf, GrpcLimits::default().max_message_bytes);
        assert_eq!(boundary, GrpcFrameBoundary::NeedMore);
        assert_eq!(
            buf.len(),
            GRPC_FRAME_HEADER_LEN - 1,
            "must not mutate the buffer while awaiting more header bytes"
        );
    }

    #[test]
    fn frame_boundary_rejects_length_over_cap() {
        // Header declares len=10 against a cap of 5: reject immediately, do
        // not wait for the (attacker-controlled) body to show up.
        let mut buf = vec![0u8, 0, 0, 0, 10];
        let boundary = next_grpc_frame_boundary(&mut buf, 5);
        assert_eq!(
            boundary,
            GrpcFrameBoundary::Malformed(GrpcFrameError::MessageTooLarge { len: 10, max: 5 })
        );
        assert!(
            buf.is_empty(),
            "oversized declared length must clear the buffer"
        );
    }

    #[test]
    fn frame_boundary_rejects_length_over_cap_disabled_guard_would_fail_here() {
        // Same shape as above, phrased as the guard-disabled regression: if
        // the `len > max_message_bytes` check were ever deleted, this frame
        // would fall through to `Ready` with an `end` computed from a fully
        // attacker-controlled `len`, which is exactly the unbounded
        // allocation this cap exists to prevent.
        let mut buf = vec![0u8, 0, 0, 1, 0]; // len = 256
        let boundary = next_grpc_frame_boundary(&mut buf, 4);
        assert!(
            matches!(
                boundary,
                GrpcFrameBoundary::Malformed(GrpcFrameError::MessageTooLarge { len: 256, max: 4 })
            ),
            "expected the cap to reject len=256 against max=4, got {boundary:?}"
        );
    }

    #[test]
    fn frame_boundary_need_more_on_huge_length_short_body() {
        // Header declares the maximum possible u32 length but only the
        // 5-byte header itself has arrived. With no cap in effect this must
        // still be NeedMore (not a crash, not an allocation of 4GiB) — the
        // boundary check is `buffer.len() < end`, never an eager allocation
        // sized from the declared length.
        let mut buf = vec![0u8, 0xFF, 0xFF, 0xFF, 0xFF];
        let boundary = next_grpc_frame_boundary(&mut buf, usize::MAX);
        assert_eq!(boundary, GrpcFrameBoundary::NeedMore);
        assert_eq!(
            buf.len(),
            GRPC_FRAME_HEADER_LEN,
            "must not mutate the buffer while awaiting body bytes"
        );
    }

    #[test]
    fn frame_boundary_finds_first_of_several_concatenated_messages() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0, 0, 0, 0, 2]); // message 1: len=2
        buf.extend_from_slice(&[0xAA, 0xBB]);
        buf.extend_from_slice(&[0, 0, 0, 0, 3]); // message 2: len=3
        buf.extend_from_slice(&[1, 2, 3]);
        let max = GrpcLimits::default().max_message_bytes;

        let first = next_grpc_frame_boundary(&mut buf, max);
        assert_eq!(
            first,
            GrpcFrameBoundary::Ready {
                end: GRPC_FRAME_HEADER_LEN + 2
            }
        );
        let frame: Vec<u8> = buf.drain(..GRPC_FRAME_HEADER_LEN + 2).collect();
        assert_eq!(&frame[GRPC_FRAME_HEADER_LEN..], &[0xAA, 0xBB]);

        let second = next_grpc_frame_boundary(&mut buf, max);
        assert_eq!(
            second,
            GrpcFrameBoundary::Ready {
                end: GRPC_FRAME_HEADER_LEN + 3
            }
        );
        let frame: Vec<u8> = buf.drain(..GRPC_FRAME_HEADER_LEN + 3).collect();
        assert_eq!(&frame[GRPC_FRAME_HEADER_LEN..], &[1, 2, 3]);
        assert!(buf.is_empty());
    }

    #[test]
    fn frame_boundary_accepts_zero_length_message() {
        let mut buf = vec![0u8, 0, 0, 0, 0];
        let boundary = next_grpc_frame_boundary(&mut buf, GrpcLimits::default().max_message_bytes);
        assert_eq!(
            boundary,
            GrpcFrameBoundary::Ready {
                end: GRPC_FRAME_HEADER_LEN
            }
        );
        assert_eq!(
            buf.len(),
            GRPC_FRAME_HEADER_LEN,
            "boundary lookup must not drain; the caller drains"
        );
    }

    #[test]
    fn frame_boundary_rejects_nonzero_compression_flag_and_clears_buffer() {
        let mut buf = vec![1u8, 0, 0, 0, 0];
        let boundary = next_grpc_frame_boundary(&mut buf, GrpcLimits::default().max_message_bytes);
        assert_eq!(
            boundary,
            GrpcFrameBoundary::Malformed(GrpcFrameError::UnsupportedCompression)
        );
        assert!(buf.is_empty());
    }
}
