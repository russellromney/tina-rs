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
use std::time::Duration;

use http::{HeaderMap, HeaderValue, Method, StatusCode};
use prost::Message;
use tina::prelude::*;
use tina::reply_to_request;
use tina_runtime::{CallOutcome, call};

use crate::{
    Http2ConnectionReply, Http2RequestParts, Http2RequestStream, Http2ServiceMessage, HttpRequest,
    HttpRequestBody, HttpResponse, RequestChunkReply,
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
}

impl Default for GrpcLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: 512 * 1024,
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
    pub path: String,
    pub message: T,
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

trait ErasedClientStreaming: Send + Sync {
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse;
    fn call_http2(&self, request: GrpcHttp2Request, limits: GrpcLimits) -> HttpResponse;
}

trait ErasedStreaming: Send + Sync {
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse;
}

trait ErasedStreamingRaw: Send + Sync {
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse;
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

/// A typed gRPC bidirectional streaming call.
///
/// This is the primary Tina-shaped surface for gRPC's "bidirectional
/// streaming RPC" mode. It is not an async stream: user code still owns a
/// normal Tina state machine, pulls requests explicitly, and returns a bounded
/// response source.
#[derive(Debug)]
pub struct GrpcStreamingCall<Req, Resp> {
    pub path: String,
    pub requests: GrpcRequestStream<Req>,
    _response: PhantomData<fn() -> Resp>,
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
        I: Isolate<Message = ResponseChunkMsg, Call = tina_runtime::RuntimeCall<ResponseChunkMsg>>,
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
        if self.buffer.len() < GRPC_FRAME_HEADER_LEN {
            return GrpcStreamReply::NeedMore;
        }
        if self.buffer[0] != 0 {
            self.buffer.clear();
            return GrpcStreamReply::Status(GrpcStatus::with_message(
                GrpcStatusCode::Unimplemented,
                "compression unsupported",
            ));
        }
        let len = u32::from_be_bytes([
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
            self.buffer[4],
        ]) as usize;
        if len > self.limits.max_message_bytes {
            self.buffer.clear();
            return GrpcStreamReply::Status(GrpcStatus::with_message(
                GrpcStatusCode::ResourceExhausted,
                format!(
                    "request message {len} exceeds cap {}",
                    self.limits.max_message_bytes
                ),
            ));
        }
        let end = GRPC_FRAME_HEADER_LEN + len;
        if self.buffer.len() < end {
            return GrpcStreamReply::NeedMore;
        }
        let frame: Vec<u8> = self.buffer.drain(..end).collect();
        match T::decode(&frame[GRPC_FRAME_HEADER_LEN..]) {
            Ok(message) => GrpcStreamReply::Message(message),
            Err(_) => GrpcStreamReply::Status(GrpcStatus::new(GrpcStatusCode::InvalidArgument)),
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
    pub path: String,
    pub stream: Http2RequestStream,
    _message: PhantomData<fn() -> T>,
}

impl<T> GrpcRawStreamingRequest<T> {
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

/// Typed client-streaming request passed to user handlers.
#[derive(Debug, Clone)]
pub struct GrpcClientStreamingRequest<T> {
    pub path: String,
    pub messages: Vec<T>,
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
}

/// Tina-shaped unary service/router template.
///
/// Register this isolate behind [`crate::Http2Listener`]. Each route
/// decodes one protobuf request message and returns one protobuf
/// response message plus real gRPC trailers.
pub struct GrpcRouter<S: Shard> {
    limits: GrpcLimits,
    unary: BTreeMap<String, Box<dyn ErasedUnary>>,
    server_streaming: BTreeMap<String, Box<dyn ErasedServerStreaming>>,
    client_streaming: BTreeMap<String, Box<dyn ErasedClientStreaming>>,
    streaming: BTreeMap<String, Box<dyn ErasedStreaming>>,
    streaming_raw: BTreeMap<String, Box<dyn ErasedStreamingRaw>>,
    pending: BTreeMap<u64, PendingGrpcRequest>,
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
}

#[derive(Debug)]
pub struct GrpcHttp2Request {
    method: Method,
    path: String,
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

    fn into_http_request(self) -> HttpRequest {
        let mut headers = HeaderMap::new();
        if self.content_type_ok {
            headers.insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/grpc"),
            );
        }
        if self.unsupported_encoding {
            headers.insert("grpc-encoding", HeaderValue::from_static("unsupported"));
        }
        let body = match self.body {
            GrpcHttp2Body::Buffered(bytes) => HttpRequestBody::Buffered(bytes),
            GrpcHttp2Body::Http2Stream(stream) => HttpRequestBody::Http2Stream(stream),
            GrpcHttp2Body::Unsupported => HttpRequestBody::Buffered(Vec::new()),
        };
        HttpRequest {
            method: self.method,
            path: self.path,
            version: http::Version::HTTP_2,
            headers,
            body,
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

struct PendingGrpcRequest {
    request: HttpRequest,
    body: Vec<u8>,
    call: tina::RequestContext<HttpResponse>,
}

impl<S: Shard + 'static> GrpcRouter<S> {
    pub fn new(limits: GrpcLimits) -> Self {
        Self {
            limits,
            unary: BTreeMap::new(),
            server_streaming: BTreeMap::new(),
            client_streaming: BTreeMap::new(),
            streaming: BTreeMap::new(),
            streaming_raw: BTreeMap::new(),
            pending: BTreeMap::new(),
            next_pending_id: 1,
            _shard: PhantomData,
        }
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
        if let Some(handler) = self.unary.get(&request.path) {
            return handler.call(request, self.limits);
        }
        if let Some(handler) = self.client_streaming.get(&request.path) {
            return handler.call(request, self.limits);
        }
        if let Some(handler) = self.streaming.get(&request.path) {
            return handler.call(request, self.limits);
        }
        if let Some(handler) = self.streaming_raw.get(&request.path) {
            return handler.call(request, self.limits);
        }
        let Some(handler) = self.server_streaming.get(&request.path) else {
            return grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented));
        };
        handler.call(request, self.limits)
    }

    fn response_for_http2(&self, request: GrpcHttp2Request) -> HttpResponse {
        if request.method != Method::POST {
            return grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented));
        }
        if self.streaming.contains_key(&request.path)
            || self.streaming_raw.contains_key(&request.path)
        {
            return self.response_for(request.into_http_request());
        }
        if let Some(handler) = self.unary.get(&request.path) {
            return handler.call_http2(request, self.limits);
        }
        if let Some(handler) = self.client_streaming.get(&request.path) {
            return handler.call_http2(request, self.limits);
        }
        let Some(handler) = self.server_streaming.get(&request.path) else {
            return grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented));
        };
        handler.call_http2(request, self.limits)
    }

    fn start_or_reply_request(
        &mut self,
        request: HttpRequest,
        call_ctx: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match &request.body {
            HttpRequestBody::Buffered(_) => call_ctx.reply(self.response_for(request)),
            HttpRequestBody::Stream(_) => call_ctx.reply(grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            )),
            HttpRequestBody::Http2Stream(stream) => {
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
                    PendingGrpcRequest {
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
            GrpcHttp2Body::Buffered(_) => call_ctx.reply(self.response_for_http2(request)),
            GrpcHttp2Body::Unsupported => call_ctx.reply(grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            )),
            GrpcHttp2Body::Http2Stream(_) => {
                self.start_or_reply_request(request.into_http_request(), call_ctx)
            }
        }
    }

    fn handle_request_chunk(
        &mut self,
        id: u64,
        outcome: CallOutcome<Http2ConnectionReply>,
    ) -> Effect<Self> {
        let Some(mut pending) = self.pending.remove(&id) else {
            return noop();
        };
        let HttpRequestBody::Http2Stream(stream) = &pending.request.body else {
            return reply_to_request(
                pending.call,
                grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal)),
            );
        };
        match outcome {
            CallOutcome::Replied(Http2ConnectionReply::RequestChunk(RequestChunkReply::Chunk(
                bytes,
            ))) => {
                pending.body.extend_from_slice(&bytes);
                let source = stream.source;
                let stream_id = stream.stream_id;
                self.pending.insert(id, pending);
                call(
                    source,
                    crate::Http2ConnectionMsg::body_next(stream_id),
                    REQUEST_BODY_PULL_TIMEOUT,
                )
                .then(move |outcome| GrpcRouterMsg::RequestBodyChunk { id, outcome })
            }
            CallOutcome::Replied(Http2ConnectionReply::RequestChunk(RequestChunkReply::Eof)) => {
                pending.request.body = HttpRequestBody::Buffered(pending.body);
                let response = self.response_for(pending.request);
                reply_to_request(pending.call, response)
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
            )) => reply_to_request(
                pending.call,
                grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal)),
            ),
            CallOutcome::Full
            | CallOutcome::Closed
            | CallOutcome::Rejected(_)
            | CallOutcome::Timeout => reply_to_request(
                pending.call,
                grpc_http_response(
                    Vec::new(),
                    GrpcStatus::new(GrpcStatusCode::DeadlineExceeded),
                ),
            ),
        }
    }
}

impl<S: Shard + 'static> Isolate for GrpcRouter<S> {
    tina::isolate_types! {
        message: GrpcRouterMsg,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: tina_runtime::RuntimeCall<GrpcRouterMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: GrpcRouterMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            GrpcRouterMsg::Request(request) => reply(self.response_for(request)),
            GrpcRouterMsg::Http2Request(request) => reply(self.response_for_http2(request)),
            GrpcRouterMsg::RequestBodyChunk { id, outcome } => {
                self.handle_request_chunk(id, outcome)
            }
        }
    }

    fn handle_call(
        &mut self,
        msg: GrpcRouterMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            GrpcRouterMsg::Request(request) => self.start_or_reply_request(request, call),
            GrpcRouterMsg::Http2Request(request) => {
                self.start_or_reply_http2_request(request, call)
            }
            GrpcRouterMsg::RequestBodyChunk { .. } => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
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
        messages.push(decode_one_grpc_message::<T>(body, &mut cursor, limits)?);
    }
    Ok(messages)
}

pub fn encode_grpc_message<T: Message>(
    message: &T,
    limits: GrpcLimits,
) -> Result<Vec<u8>, GrpcError> {
    let len = message.encoded_len();
    if len > limits.max_message_bytes {
        return Err(GrpcError::EncodeTooLarge {
            len,
            max: limits.max_message_bytes,
        });
    }
    let mut out = Vec::with_capacity(GRPC_FRAME_HEADER_LEN + len);
    out.push(0);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    message.encode(&mut out).map_err(|_| GrpcError::BadFrame)?;
    Ok(out)
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
        GrpcError::EncodeTooLarge { .. } => GrpcStatus::new(GrpcStatusCode::ResourceExhausted),
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

    #[derive(Clone, PartialEq, Message)]
    struct Ping {
        #[prost(uint64, tag = "1")]
        value: u64,
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
}
