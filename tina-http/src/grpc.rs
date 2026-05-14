//! Native gRPC first form over Tina HTTP/2.
//!
//! This is intentionally small: unary plus first server/client
//! streaming `prost` messages, typed status, bounded gRPC message
//! frames, h2c prior-knowledge, and real HTTP/2 trailers. It is not
//! tonic and does not include interceptors, compression, bidirectional
//! streaming, or a production pooled client.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderValue, Method, StatusCode};
use prost::Message;
use tina::prelude::*;
use tina::reply_to_request;
use tina_runtime::{CallOutcome, call};

use crate::{Http2ConnectionReply, HttpRequest, HttpRequestBody, HttpResponse, RequestChunkReply};
use crate::{Http2RequestStream, IterBodySource, ResponseChunkMsg, ResponseChunkReply};

const GRPC_FRAME_HEADER_LEN: usize = 5;
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
}

trait ErasedServerStreaming: Send + Sync {
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse;
}

trait ErasedClientStreaming: Send + Sync {
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse;
}

trait ErasedClientStreamingLive: Send + Sync {
    fn start(
        &self,
        request: &HttpRequest,
        limits: GrpcLimits,
    ) -> Result<PendingGrpcBody, GrpcStatus>;
}

trait ErasedClientStreamingPull<S: Shard + 'static>: Send + Sync {
    fn call_service(
        &self,
        id: u64,
        child: tina::ChildRef<GrpcRequestStreamSourceMsg, GrpcRequestStreamSourceReply>,
        request: HttpRequest,
    ) -> Effect<GrpcRouter<S>>;
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
        let path = request.path.clone();
        match decode_unary_request::<Req>(&request, limits) {
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

struct ClientStreamingLiveHandler<Req, Resp, State, Init, Step, Finish> {
    init: Arc<Init>,
    step: Arc<Step>,
    finish: Arc<Finish>,
    _types: PhantomData<fn(Req, Resp, State)>,
}

struct ClientStreamingPullHandler<Req, Msg, F> {
    service: tina::Address<Msg, HttpResponse>,
    make: F,
    timeout: Duration,
    _types: PhantomData<fn(Req) -> Msg>,
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

impl<Req, F> ErasedServerStreaming for ServerStreamingHandler<Req, F>
where
    Req: Message + Default + Send + Sync + 'static,
    F: Fn(GrpcRequest<Req>) -> Result<GrpcServerStreamingResponse, GrpcStatus>
        + Send
        + Sync
        + 'static,
{
    fn call(&self, request: HttpRequest, limits: GrpcLimits) -> HttpResponse {
        let path = request.path.clone();
        match decode_unary_request::<Req>(&request, limits) {
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

/// Start metadata for an incremental client-streaming call.
#[derive(Debug, Clone)]
pub struct GrpcClientStreamingStart {
    pub path: String,
}

/// Control returned after each decoded request message.
///
/// `Continue` asks the router to pull the next HTTP/2 DATA chunk. `Respond`
/// finishes the RPC immediately, without waiting for request EOF.
#[derive(Debug, Clone)]
pub enum GrpcClientStreamingControl<T> {
    Continue,
    Respond(GrpcResponse<T>),
}

/// Pull-oriented client-streaming request passed to a Tina service isolate.
#[derive(Debug, Clone)]
pub struct GrpcClientStreamingPullRequest<T> {
    pub path: String,
    pub stream: GrpcRequestStream<T>,
}

/// One decoded item from a gRPC request stream.
#[derive(Debug, Clone)]
pub enum GrpcRequestStreamNext<T> {
    Message(T),
    Eof,
    Error(GrpcStatus),
}

/// Typed pull handle for gRPC request messages.
#[derive(Debug, Clone, Copy)]
pub struct GrpcRequestStream<T> {
    source: tina::Address<GrpcRequestStreamSourceMsg, GrpcRequestStreamSourceReply>,
    _type: PhantomData<T>,
}

impl<T> GrpcRequestStream<T>
where
    T: Message + Default + Send + 'static,
{
    pub fn next<I, M, F>(&self, timeout: Duration, map: F) -> Effect<I>
    where
        I: Isolate<Message = M, Call = tina_runtime::RuntimeCall<M>>,
        M: 'static,
        F: FnOnce(GrpcRequestStreamNext<T>) -> M + 'static,
    {
        call(self.source, GrpcRequestStreamSourceMsg::Next, timeout).reply(move |outcome| {
            let next = match outcome {
                CallOutcome::Replied(GrpcRequestStreamSourceReply::Message(bytes)) => {
                    match T::decode(&bytes[..]) {
                        Ok(message) => GrpcRequestStreamNext::Message(message),
                        Err(_) => GrpcRequestStreamNext::Error(GrpcStatus::new(
                            GrpcStatusCode::InvalidArgument,
                        )),
                    }
                }
                CallOutcome::Replied(GrpcRequestStreamSourceReply::Eof) => {
                    GrpcRequestStreamNext::Eof
                }
                CallOutcome::Replied(GrpcRequestStreamSourceReply::Error(status)) => {
                    GrpcRequestStreamNext::Error(status)
                }
                CallOutcome::Full => {
                    GrpcRequestStreamNext::Error(GrpcStatus::new(GrpcStatusCode::ResourceExhausted))
                }
                CallOutcome::Closed => {
                    GrpcRequestStreamNext::Error(GrpcStatus::new(GrpcStatusCode::Cancelled))
                }
                CallOutcome::Rejected(_) => {
                    GrpcRequestStreamNext::Error(GrpcStatus::new(GrpcStatusCode::Internal))
                }
                CallOutcome::Timeout => {
                    GrpcRequestStreamNext::Error(GrpcStatus::new(GrpcStatusCode::DeadlineExceeded))
                }
            };
            map(next)
        })
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
        let path = request.path.clone();
        match decode_streaming_request::<Req>(&request, limits) {
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

impl<Req, Resp, State, Init, Step, Finish> ErasedClientStreamingLive
    for ClientStreamingLiveHandler<Req, Resp, State, Init, Step, Finish>
where
    Req: Message + Default + Send + Sync + 'static,
    Resp: Message + Default + Send + Sync + 'static,
    State: Send + 'static,
    Init: Fn(GrpcClientStreamingStart) -> State + Send + Sync + 'static,
    Step: Fn(&mut State, GrpcRequest<Req>) -> Result<GrpcClientStreamingControl<Resp>, GrpcStatus>
        + Send
        + Sync
        + 'static,
    Finish: Fn(State) -> Result<GrpcResponse<Resp>, GrpcStatus> + Send + Sync + 'static,
{
    fn start(
        &self,
        request: &HttpRequest,
        _limits: GrpcLimits,
    ) -> Result<PendingGrpcBody, GrpcStatus> {
        validate_grpc_request(request).map_err(status_for_error)?;
        let path = request.path.clone();
        let state = (self.init)(GrpcClientStreamingStart { path: path.clone() });
        Ok(PendingGrpcBody::LiveClientStreaming(Box::new(
            LiveClientStreamingState::<Req, Resp, State, Step, Finish> {
                path,
                state: Some(state),
                decoder: GrpcStreamingDecoder::new(),
                step: Arc::clone(&self.step),
                finish: Arc::clone(&self.finish),
                _types: PhantomData,
            },
        )))
    }
}

trait ErasedLiveClientStreamingState: Send {
    fn push_chunk(
        &mut self,
        bytes: &[u8],
        limits: GrpcLimits,
    ) -> Result<Option<HttpResponse>, GrpcStatus>;
    fn finish(self: Box<Self>, limits: GrpcLimits) -> HttpResponse;
}

struct LiveClientStreamingState<Req, Resp, State, Step, Finish> {
    path: String,
    state: Option<State>,
    decoder: GrpcStreamingDecoder<Req>,
    step: Arc<Step>,
    finish: Arc<Finish>,
    _types: PhantomData<fn(Resp)>,
}

impl<Req, Resp, State, Step, Finish> ErasedLiveClientStreamingState
    for LiveClientStreamingState<Req, Resp, State, Step, Finish>
where
    Req: Message + Default + Send + Sync + 'static,
    Resp: Message + Default + Send + Sync + 'static,
    State: Send + 'static,
    Step: Fn(&mut State, GrpcRequest<Req>) -> Result<GrpcClientStreamingControl<Resp>, GrpcStatus>
        + Send
        + Sync
        + 'static,
    Finish: Fn(State) -> Result<GrpcResponse<Resp>, GrpcStatus> + Send + Sync + 'static,
{
    fn push_chunk(
        &mut self,
        bytes: &[u8],
        limits: GrpcLimits,
    ) -> Result<Option<HttpResponse>, GrpcStatus> {
        let step = Arc::clone(&self.step);
        let path = self.path.clone();
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| GrpcStatus::new(GrpcStatusCode::Internal))?;
        self.decoder
            .push(bytes, limits, move |message| {
                match step(
                    state,
                    GrpcRequest {
                        path: path.clone(),
                        message,
                    },
                )? {
                    GrpcClientStreamingControl::Continue => Ok(None),
                    GrpcClientStreamingControl::Respond(response) => {
                        Ok(Some(response_to_http(response, limits)))
                    }
                }
            })
            .map_err(status_for_error)
    }

    fn finish(mut self: Box<Self>, limits: GrpcLimits) -> HttpResponse {
        if !self.decoder.is_idle() {
            return grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            );
        }
        let Some(state) = self.state.take() else {
            return grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal));
        };
        match (self.finish)(state) {
            Ok(response) => response_to_http(response, limits),
            Err(status) => grpc_http_response(Vec::new(), status),
        }
    }
}

impl<S, Req, Msg, F> ErasedClientStreamingPull<S> for ClientStreamingPullHandler<Req, Msg, F>
where
    S: Shard + 'static,
    Req: Message + Default + Send + Sync + 'static,
    Msg: Send + 'static,
    F: Fn(GrpcClientStreamingPullRequest<Req>) -> Msg + Send + Sync + 'static,
{
    fn call_service(
        &self,
        id: u64,
        child: tina::ChildRef<GrpcRequestStreamSourceMsg, GrpcRequestStreamSourceReply>,
        request: HttpRequest,
    ) -> Effect<GrpcRouter<S>> {
        let msg = (self.make)(GrpcClientStreamingPullRequest {
            path: request.path,
            stream: GrpcRequestStream {
                source: child.address,
                _type: PhantomData,
            },
        });
        call(self.service, msg, self.timeout)
            .reply(move |outcome| GrpcRouterMsg::ClientStreamingPullReturned { id, outcome })
    }
}

#[derive(Debug)]
pub enum GrpcRequestStreamSourceMsg {
    Next,
    Stop,
    SourceChunk {
        outcome: CallOutcome<Http2ConnectionReply>,
    },
}

#[derive(Debug)]
pub enum GrpcRequestStreamSourceReply {
    Message(Vec<u8>),
    Eof,
    Error(GrpcStatus),
}

pub struct GrpcRequestStreamSource<S: Shard> {
    stream: Http2RequestStream,
    limits: GrpcLimits,
    decoder: GrpcFrameStreamDecoder,
    decoded: VecDeque<Vec<u8>>,
    pending_next: Option<tina::RequestContext<GrpcRequestStreamSourceReply>>,
    eof: bool,
    terminal_error: Option<GrpcStatus>,
    pull_in_flight: bool,
    _shard: PhantomData<S>,
}

impl<S: Shard> GrpcRequestStreamSource<S> {
    fn new(stream: Http2RequestStream, limits: GrpcLimits) -> Self {
        Self {
            stream,
            limits,
            decoder: GrpcFrameStreamDecoder::new(),
            decoded: VecDeque::new(),
            pending_next: None,
            eof: false,
            terminal_error: None,
            pull_in_flight: false,
            _shard: PhantomData,
        }
    }
}

impl<S: Shard + 'static> GrpcRequestStreamSource<S> {
    fn satisfy_or_pull(&mut self) -> Effect<Self> {
        let Some(call_ctx) = self.pending_next.take() else {
            return noop();
        };
        if let Some(message) = self.decoded.pop_front() {
            return reply_to_request(call_ctx, GrpcRequestStreamSourceReply::Message(message));
        }
        if let Some(status) = self.terminal_error.take() {
            return batch(vec![
                reply_to_request(call_ctx, GrpcRequestStreamSourceReply::Error(status)),
                stop(),
            ]);
        }
        if self.eof {
            return batch(vec![
                reply_to_request(call_ctx, GrpcRequestStreamSourceReply::Eof),
                stop(),
            ]);
        }
        self.pending_next = Some(call_ctx);
        if self.pull_in_flight {
            return noop();
        }
        self.pull_in_flight = true;
        let source = self.stream.source;
        let stream_id = self.stream.stream_id;
        call(
            source,
            crate::Http2ConnectionMsg::body_next(stream_id),
            REQUEST_BODY_PULL_TIMEOUT,
        )
        .reply(move |outcome| GrpcRequestStreamSourceMsg::SourceChunk { outcome })
    }
}

impl<S: Shard + 'static> Isolate for GrpcRequestStreamSource<S> {
    tina::isolate_types! {
        message: GrpcRequestStreamSourceMsg,
        reply: GrpcRequestStreamSourceReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: tina_runtime::RuntimeCall<GrpcRequestStreamSourceMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: GrpcRequestStreamSourceMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            GrpcRequestStreamSourceMsg::Next => noop(),
            GrpcRequestStreamSourceMsg::Stop => stop(),
            GrpcRequestStreamSourceMsg::SourceChunk { outcome } => {
                self.pull_in_flight = false;
                match outcome {
                    CallOutcome::Replied(Http2ConnectionReply::RequestChunk(
                        RequestChunkReply::Chunk(bytes),
                    )) => {
                        if let Err(error) =
                            self.decoder.push(&bytes, self.limits, &mut self.decoded)
                        {
                            self.terminal_error = Some(status_for_error(error));
                        }
                    }
                    CallOutcome::Replied(Http2ConnectionReply::RequestChunk(
                        RequestChunkReply::Eof,
                    )) => {
                        if self.decoder.is_idle() {
                            self.eof = true;
                        } else {
                            self.terminal_error =
                                Some(GrpcStatus::new(GrpcStatusCode::InvalidArgument));
                        }
                    }
                    CallOutcome::Replied(Http2ConnectionReply::RequestChunk(
                        RequestChunkReply::Error(error),
                    )) => {
                        self.terminal_error = Some(status_for_request_pull_error(error));
                    }
                    other => {
                        self.terminal_error = Some(status_for_request_pull_outcome(&other));
                    }
                }
                self.satisfy_or_pull()
            }
        }
    }

    fn handle_call(
        &mut self,
        msg: GrpcRequestStreamSourceMsg,
        call_ctx: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            GrpcRequestStreamSourceMsg::Next => {
                if self.pending_next.is_some() {
                    return call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage);
                }
                self.pending_next = Some(call_ctx.into_request_context());
                self.satisfy_or_pull()
            }
            GrpcRequestStreamSourceMsg::Stop => batch(vec![
                call_ctx.reply(GrpcRequestStreamSourceReply::Eof),
                stop(),
            ]),
            GrpcRequestStreamSourceMsg::SourceChunk { .. } => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
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
    live_client_streaming: BTreeMap<String, Box<dyn ErasedClientStreamingLive>>,
    pull_client_streaming: BTreeMap<String, Box<dyn ErasedClientStreamingPull<S>>>,
    pending: BTreeMap<u64, PendingGrpcRequest>,
    pending_pull: BTreeMap<u64, PendingGrpcPullRequest>,
    next_pending_id: u64,
    _shard: PhantomData<S>,
}

#[derive(Debug)]
pub enum GrpcRouterMsg {
    Request(HttpRequest),
    RequestBodyChunk {
        id: u64,
        outcome: CallOutcome<Http2ConnectionReply>,
    },
    RequestStreamSpawned {
        id: u64,
        result: tina::SpawnObservedResult<GrpcRequestStreamSourceMsg, GrpcRequestStreamSourceReply>,
    },
    ClientStreamingPullReturned {
        id: u64,
        outcome: CallOutcome<HttpResponse>,
    },
    RequestStreamStopped {
        outcome: CallOutcome<GrpcRequestStreamSourceReply>,
    },
}

impl From<HttpRequest> for GrpcRouterMsg {
    fn from(request: HttpRequest) -> Self {
        Self::Request(request)
    }
}

struct PendingGrpcRequest {
    request: HttpRequest,
    body: PendingGrpcBody,
    call: tina::RequestContext<HttpResponse>,
}

struct PendingGrpcPullRequest {
    request: HttpRequest,
    call: tina::RequestContext<HttpResponse>,
    child: Option<tina::Address<GrpcRequestStreamSourceMsg, GrpcRequestStreamSourceReply>>,
}

enum PendingGrpcBody {
    Buffered(Vec<u8>),
    LiveClientStreaming(Box<dyn ErasedLiveClientStreamingState>),
}

impl<S: Shard + 'static> GrpcRouter<S> {
    pub fn new(limits: GrpcLimits) -> Self {
        Self {
            limits,
            unary: BTreeMap::new(),
            server_streaming: BTreeMap::new(),
            client_streaming: BTreeMap::new(),
            live_client_streaming: BTreeMap::new(),
            pull_client_streaming: BTreeMap::new(),
            pending: BTreeMap::new(),
            pending_pull: BTreeMap::new(),
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

    pub fn client_streaming<Req, Resp, State, Init, Step, Finish>(
        mut self,
        path: impl Into<String>,
        init: Init,
        step: Step,
        finish: Finish,
    ) -> Self
    where
        Req: Message + Default + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
        State: Send + 'static,
        Init: Fn(GrpcClientStreamingStart) -> State + Send + Sync + 'static,
        Step: Fn(&mut State, GrpcRequest<Req>) -> Result<GrpcClientStreamingControl<Resp>, GrpcStatus>
            + Send
            + Sync
            + 'static,
        Finish: Fn(State) -> Result<GrpcResponse<Resp>, GrpcStatus> + Send + Sync + 'static,
    {
        self.live_client_streaming.insert(
            path.into(),
            Box::new(
                ClientStreamingLiveHandler::<Req, Resp, State, Init, Step, Finish> {
                    init: Arc::new(init),
                    step: Arc::new(step),
                    finish: Arc::new(finish),
                    _types: PhantomData,
                },
            ),
        );
        self
    }

    pub fn client_streaming_buffered<Req, Resp, F>(mut self, path: impl Into<String>, f: F) -> Self
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

    pub fn client_streaming_service<Req, Msg, F>(
        self,
        path: impl Into<String>,
        service: tina::Address<Msg, HttpResponse>,
        make: F,
    ) -> Self
    where
        Req: Message + Default + Send + Sync + 'static,
        Msg: Send + 'static,
        F: Fn(GrpcClientStreamingPullRequest<Req>) -> Msg + Send + Sync + 'static,
    {
        self.client_streaming_service_with_timeout(path, service, make, REQUEST_BODY_PULL_TIMEOUT)
    }

    pub fn client_streaming_service_with_timeout<Req, Msg, F>(
        mut self,
        path: impl Into<String>,
        service: tina::Address<Msg, HttpResponse>,
        make: F,
        timeout: Duration,
    ) -> Self
    where
        Req: Message + Default + Send + Sync + 'static,
        Msg: Send + 'static,
        F: Fn(GrpcClientStreamingPullRequest<Req>) -> Msg + Send + Sync + 'static,
    {
        self.pull_client_streaming.insert(
            path.into(),
            Box::new(ClientStreamingPullHandler::<Req, Msg, F> {
                service,
                make,
                timeout,
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
        if let Some(handler) = self.live_client_streaming.get(&request.path) {
            let body = request.body.as_buffered().map(|body| body.to_vec());
            let Some(body) = body else {
                return grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal));
            };
            let mut pending = match handler.start(&request, self.limits) {
                Ok(PendingGrpcBody::LiveClientStreaming(pending)) => pending,
                Ok(PendingGrpcBody::Buffered(_)) => {
                    return grpc_http_response(
                        Vec::new(),
                        GrpcStatus::new(GrpcStatusCode::Internal),
                    );
                }
                Err(status) => return grpc_http_response(Vec::new(), status),
            };
            match pending.push_chunk(&body, self.limits) {
                Ok(Some(response)) => return response,
                Ok(None) => return pending.finish(self.limits),
                Err(status) => return grpc_http_response(Vec::new(), status),
            }
        }
        if let Some(handler) = self.client_streaming.get(&request.path) {
            return handler.call(request, self.limits);
        }
        let Some(handler) = self.server_streaming.get(&request.path) else {
            return grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented));
        };
        handler.call(request, self.limits)
    }

    fn start_or_reply_request(
        &mut self,
        request: HttpRequest,
        call_ctx: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        if request.method != Method::POST {
            return call_ctx.reply(grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::Unimplemented),
            ));
        }
        match &request.body {
            HttpRequestBody::Buffered(_) => call_ctx.reply(self.response_for(request)),
            HttpRequestBody::Stream(_) => call_ctx.reply(grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::InvalidArgument),
            )),
            HttpRequestBody::Http2Stream(stream) => {
                if self.pull_client_streaming.contains_key(&request.path) {
                    if let Err(error) = validate_grpc_request(&request) {
                        return call_ctx
                            .reply(grpc_http_response(Vec::new(), status_for_error(error)));
                    }
                    let id = self.next_pending_id;
                    self.next_pending_id = self.next_pending_id.saturating_add(1);
                    let source = GrpcRequestStreamSource::<S>::new(stream.clone(), self.limits);
                    self.pending_pull.insert(
                        id,
                        PendingGrpcPullRequest {
                            request,
                            call: call_ctx.into_request_context(),
                            child: None,
                        },
                    );
                    return tina::spawn_observed(tina::ChildDefinition::new(source, 16))
                        .reply(move |result| GrpcRouterMsg::RequestStreamSpawned { id, result });
                }
                let body = if let Some(handler) = self.live_client_streaming.get(&request.path) {
                    match handler.start(&request, self.limits) {
                        Ok(body) => body,
                        Err(status) => {
                            return call_ctx.reply(grpc_http_response(Vec::new(), status));
                        }
                    }
                } else {
                    PendingGrpcBody::Buffered(Vec::new())
                };
                let id = self.next_pending_id;
                self.next_pending_id = self.next_pending_id.saturating_add(1);
                let source = stream.source;
                let stream_id = stream.stream_id;
                let request_context = call_ctx.into_request_context();
                self.pending.insert(
                    id,
                    PendingGrpcRequest {
                        request,
                        body,
                        call: request_context,
                    },
                );
                call(
                    source,
                    crate::Http2ConnectionMsg::body_next(stream_id),
                    REQUEST_BODY_PULL_TIMEOUT,
                )
                .reply(move |outcome| GrpcRouterMsg::RequestBodyChunk { id, outcome })
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
                match &mut pending.body {
                    PendingGrpcBody::Buffered(body) => body.extend_from_slice(&bytes),
                    PendingGrpcBody::LiveClientStreaming(state) => {
                        match state.push_chunk(&bytes, self.limits) {
                            Ok(Some(response)) => return reply_to_request(pending.call, response),
                            Ok(None) => {}
                            Err(status) => {
                                return reply_to_request(
                                    pending.call,
                                    grpc_http_response(Vec::new(), status),
                                );
                            }
                        }
                    }
                }
                let source = stream.source;
                let stream_id = stream.stream_id;
                self.pending.insert(id, pending);
                call(
                    source,
                    crate::Http2ConnectionMsg::body_next(stream_id),
                    REQUEST_BODY_PULL_TIMEOUT,
                )
                .reply(move |outcome| GrpcRouterMsg::RequestBodyChunk { id, outcome })
            }
            CallOutcome::Replied(Http2ConnectionReply::RequestChunk(RequestChunkReply::Eof)) => {
                let response = match pending.body {
                    PendingGrpcBody::Buffered(body) => {
                        pending.request.body = HttpRequestBody::Buffered(body);
                        self.response_for(pending.request)
                    }
                    PendingGrpcBody::LiveClientStreaming(state) => state.finish(self.limits),
                };
                reply_to_request(pending.call, response)
            }
            CallOutcome::Replied(Http2ConnectionReply::RequestChunk(RequestChunkReply::Error(
                error,
            ))) => reply_to_request(
                pending.call,
                grpc_http_response(Vec::new(), status_for_request_pull_error(error)),
            ),
            CallOutcome::Replied(Http2ConnectionReply::RequestChunk(
                RequestChunkReply::WebSocketSend(_),
            ))
            | CallOutcome::Replied(Http2ConnectionReply::Report(_)) => reply_to_request(
                pending.call,
                grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal)),
            ),
            other => reply_to_request(
                pending.call,
                grpc_http_response(Vec::new(), status_for_request_pull_outcome(&other)),
            ),
        }
    }

    fn handle_request_stream_spawned(
        &mut self,
        id: u64,
        result: tina::SpawnObservedResult<GrpcRequestStreamSourceMsg, GrpcRequestStreamSourceReply>,
    ) -> Effect<Self> {
        let Some(mut pending) = self.pending_pull.remove(&id) else {
            return noop();
        };
        let child = match result {
            Ok(child) => child,
            Err(_) => {
                return reply_to_request(
                    pending.call,
                    grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal)),
                );
            }
        };
        let Some(handler) = self.pull_client_streaming.get(&pending.request.path) else {
            return reply_to_request(
                pending.call,
                grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Unimplemented)),
            );
        };
        pending.child = Some(child.address);
        let request = pending.request.clone();
        self.pending_pull.insert(id, pending);
        handler.call_service(id, child, request)
    }

    fn handle_client_streaming_pull_returned(
        &mut self,
        id: u64,
        outcome: CallOutcome<HttpResponse>,
    ) -> Effect<Self> {
        let Some(pending) = self.pending_pull.remove(&id) else {
            return noop();
        };
        let response = match outcome {
            CallOutcome::Replied(response) => response,
            CallOutcome::Full => grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::ResourceExhausted),
            ),
            CallOutcome::Closed => {
                grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Cancelled))
            }
            CallOutcome::Rejected(_) => {
                grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal))
            }
            CallOutcome::Timeout => grpc_http_response(
                Vec::new(),
                GrpcStatus::new(GrpcStatusCode::DeadlineExceeded),
            ),
        };
        let reply_effect = reply_to_request(pending.call, response);
        if let Some(child) = pending.child {
            batch(vec![
                reply_effect,
                call(
                    child,
                    GrpcRequestStreamSourceMsg::Stop,
                    Duration::from_secs(1),
                )
                .reply(|outcome| GrpcRouterMsg::RequestStreamStopped { outcome }),
            ])
        } else {
            reply_effect
        }
    }
}

fn status_for_request_pull_error(error: tina_runtime::CallError) -> GrpcStatus {
    match error {
        tina_runtime::CallError::TargetClosed => GrpcStatus::new(GrpcStatusCode::Cancelled),
        tina_runtime::CallError::TargetFull
        | tina_runtime::CallError::ResourceBusy
        | tina_runtime::CallError::StorageFull
        | tina_runtime::CallError::DnsFull
        | tina_runtime::CallError::TlsFull
        | tina_runtime::CallError::SignalFull
        | tina_runtime::CallError::ProcessFull => {
            GrpcStatus::new(GrpcStatusCode::ResourceExhausted)
        }
        tina_runtime::CallError::Timeout => GrpcStatus::new(GrpcStatusCode::DeadlineExceeded),
        tina_runtime::CallError::Io => GrpcStatus::new(GrpcStatusCode::Unavailable),
        _ => GrpcStatus::new(GrpcStatusCode::Internal),
    }
}

fn status_for_request_pull_outcome(outcome: &CallOutcome<Http2ConnectionReply>) -> GrpcStatus {
    match outcome {
        CallOutcome::Full => GrpcStatus::new(GrpcStatusCode::ResourceExhausted),
        CallOutcome::Closed => GrpcStatus::new(GrpcStatusCode::Cancelled),
        CallOutcome::Rejected(_) => GrpcStatus::new(GrpcStatusCode::Internal),
        CallOutcome::Timeout => GrpcStatus::new(GrpcStatusCode::DeadlineExceeded),
        CallOutcome::Replied(_) => GrpcStatus::new(GrpcStatusCode::Internal),
    }
}

impl<S: Shard + 'static> Isolate for GrpcRouter<S> {
    tina::isolate_types! {
        message: GrpcRouterMsg,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        spawn_observed: tina::SpawnObserved<
            tina::ChildDefinition<GrpcRequestStreamSource<S>>,
            GrpcRouterMsg,
            GrpcRequestStreamSourceMsg,
            GrpcRequestStreamSourceReply
        >,
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
            GrpcRouterMsg::RequestBodyChunk { id, outcome } => {
                self.handle_request_chunk(id, outcome)
            }
            GrpcRouterMsg::RequestStreamSpawned { id, result } => {
                self.handle_request_stream_spawned(id, result)
            }
            GrpcRouterMsg::ClientStreamingPullReturned { id, outcome } => {
                self.handle_client_streaming_pull_returned(id, outcome)
            }
            GrpcRouterMsg::RequestStreamStopped { outcome } => {
                let _ = outcome;
                noop()
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
            GrpcRouterMsg::RequestBodyChunk { .. }
            | GrpcRouterMsg::RequestStreamSpawned { .. }
            | GrpcRouterMsg::ClientStreamingPullReturned { .. }
            | GrpcRouterMsg::RequestStreamStopped { .. } => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

pub fn decode_unary_request<T: Message + Default>(
    request: &HttpRequest,
    limits: GrpcLimits,
) -> Result<T, GrpcError> {
    validate_grpc_request(request)?;
    let body = request.body.as_buffered().ok_or(GrpcError::BadFrame)?;
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
    validate_grpc_request(request)?;
    let body = request.body.as_buffered().ok_or(GrpcError::BadFrame)?;
    let mut cursor = 0;
    let mut messages = Vec::new();
    while cursor < body.len() {
        messages.push(decode_one_grpc_message::<T>(body, &mut cursor, limits)?);
    }
    Ok(messages)
}

fn validate_grpc_request(request: &HttpRequest) -> Result<(), GrpcError> {
    let content_type = request
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !is_grpc_content_type(content_type) {
        return Err(GrpcError::BadContentType);
    }
    let unsupported_encoding = request
        .headers
        .get("grpc-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.eq_ignore_ascii_case("identity"));
    if unsupported_encoding {
        return Err(GrpcError::CompressedUnsupported);
    }
    Ok(())
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

fn decode_one_grpc_message<T: Message + Default>(
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

struct GrpcStreamingDecoder<T> {
    header: [u8; GRPC_FRAME_HEADER_LEN],
    header_len: usize,
    message: Vec<u8>,
    expected_len: Option<usize>,
    _type: PhantomData<T>,
}

struct GrpcFrameStreamDecoder {
    header: [u8; GRPC_FRAME_HEADER_LEN],
    header_len: usize,
    message: Vec<u8>,
    expected_len: Option<usize>,
}

impl GrpcFrameStreamDecoder {
    fn new() -> Self {
        Self {
            header: [0; GRPC_FRAME_HEADER_LEN],
            header_len: 0,
            message: Vec::new(),
            expected_len: None,
        }
    }

    fn is_idle(&self) -> bool {
        self.header_len == 0 && self.expected_len.is_none() && self.message.is_empty()
    }

    fn push(
        &mut self,
        mut bytes: &[u8],
        limits: GrpcLimits,
        out: &mut VecDeque<Vec<u8>>,
    ) -> Result<(), GrpcError> {
        while !bytes.is_empty() || self.expected_len == Some(0) {
            if self.expected_len.is_none() {
                let take = (GRPC_FRAME_HEADER_LEN - self.header_len).min(bytes.len());
                self.header[self.header_len..self.header_len + take]
                    .copy_from_slice(&bytes[..take]);
                self.header_len += take;
                bytes = &bytes[take..];
                if self.header_len < GRPC_FRAME_HEADER_LEN {
                    continue;
                }
                if self.header[0] != 0 {
                    return Err(GrpcError::CompressedUnsupported);
                }
                let len = u32::from_be_bytes([
                    self.header[1],
                    self.header[2],
                    self.header[3],
                    self.header[4],
                ]) as usize;
                if len > limits.max_message_bytes {
                    return Err(GrpcError::MessageTooLarge {
                        len,
                        max: limits.max_message_bytes,
                    });
                }
                self.message.clear();
                self.message.reserve(len);
                self.expected_len = Some(len);
            }

            let expected_len = self.expected_len.expect("expected length is set");
            let remaining = expected_len.saturating_sub(self.message.len());
            let take = remaining.min(bytes.len());
            self.message.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];

            if self.message.len() == expected_len {
                out.push_back(std::mem::take(&mut self.message));
                self.header_len = 0;
                self.expected_len = None;
            }
        }
        Ok(())
    }
}

impl<T> GrpcStreamingDecoder<T>
where
    T: Message + Default,
{
    fn new() -> Self {
        Self {
            header: [0; GRPC_FRAME_HEADER_LEN],
            header_len: 0,
            message: Vec::new(),
            expected_len: None,
            _type: PhantomData,
        }
    }

    fn is_idle(&self) -> bool {
        self.header_len == 0 && self.expected_len.is_none() && self.message.is_empty()
    }

    fn push<F>(
        &mut self,
        mut bytes: &[u8],
        limits: GrpcLimits,
        mut on_message: F,
    ) -> Result<Option<HttpResponse>, GrpcError>
    where
        F: FnMut(T) -> Result<Option<HttpResponse>, GrpcStatus>,
    {
        while !bytes.is_empty() || self.expected_len == Some(0) {
            if self.expected_len.is_none() {
                let take = (GRPC_FRAME_HEADER_LEN - self.header_len).min(bytes.len());
                self.header[self.header_len..self.header_len + take]
                    .copy_from_slice(&bytes[..take]);
                self.header_len += take;
                bytes = &bytes[take..];
                if self.header_len < GRPC_FRAME_HEADER_LEN {
                    continue;
                }
                if self.header[0] != 0 {
                    return Err(GrpcError::CompressedUnsupported);
                }
                let len = u32::from_be_bytes([
                    self.header[1],
                    self.header[2],
                    self.header[3],
                    self.header[4],
                ]) as usize;
                if len > limits.max_message_bytes {
                    return Err(GrpcError::MessageTooLarge {
                        len,
                        max: limits.max_message_bytes,
                    });
                }
                self.message.clear();
                self.message.reserve(len);
                self.expected_len = Some(len);
            }

            let expected_len = self.expected_len.expect("expected length is set");
            let remaining = expected_len.saturating_sub(self.message.len());
            let take = remaining.min(bytes.len());
            self.message.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];

            if self.message.len() == expected_len {
                let decoded = T::decode(&self.message[..]).map_err(|_| GrpcError::Decode)?;
                self.header_len = 0;
                self.expected_len = None;
                self.message.clear();
                if let Some(response) = on_message(decoded).map_err(GrpcError::Status)? {
                    return Ok(Some(response));
                }
            }
        }
        Ok(None)
    }
}

pub(crate) fn grpc_status_http_response(status: GrpcStatus) -> HttpResponse {
    grpc_http_response(Vec::new(), status)
}

pub fn grpc_response<T: Message>(
    response: Result<GrpcResponse<T>, GrpcStatus>,
    limits: GrpcLimits,
) -> HttpResponse {
    match response {
        Ok(response) => response_to_http(response, limits),
        Err(status) => grpc_http_response(Vec::new(), status),
    }
}

fn response_to_http<T: Message>(response: GrpcResponse<T>, limits: GrpcLimits) -> HttpResponse {
    match encode_grpc_message(&response.message, limits) {
        Ok(body) => grpc_http_response(body, GrpcStatus::ok()),
        Err(GrpcError::EncodeTooLarge { len, max }) => grpc_http_response(
            Vec::new(),
            GrpcStatus::with_message(
                GrpcStatusCode::ResourceExhausted,
                format!("response message {len} exceeds cap {max}"),
            ),
        ),
        Err(_) => grpc_http_response(Vec::new(), GrpcStatus::new(GrpcStatusCode::Internal)),
    }
}

fn grpc_http_response(body: Vec<u8>, status: GrpcStatus) -> HttpResponse {
    let mut response = HttpResponse::with_body(StatusCode::OK, body);
    response.headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc+proto"),
    );
    response.headers.insert(
        http::HeaderName::from_static("grpc-status"),
        HeaderValue::from_str(&status.code.as_u16().to_string())
            .expect("numeric grpc status is header-safe"),
    );
    if let Some(message) = status.message {
        response.headers.insert(
            http::HeaderName::from_static("grpc-message"),
            HeaderValue::from_str(&percent_encode_grpc_message(&message))
                .unwrap_or_else(|_| HeaderValue::from_static("invalid")),
        );
    }
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
    response.headers.insert(
        http::HeaderName::from_static("grpc-status"),
        HeaderValue::from_str(&status.code.as_u16().to_string())
            .expect("numeric grpc status is header-safe"),
    );
    if let Some(message) = status.message {
        response.headers.insert(
            http::HeaderName::from_static("grpc-message"),
            HeaderValue::from_str(&percent_encode_grpc_message(&message))
                .unwrap_or_else(|_| HeaderValue::from_static("invalid")),
        );
    }
    response
}

fn status_for_error(error: GrpcError) -> GrpcStatus {
    match error {
        GrpcError::BadContentType => GrpcStatus::new(GrpcStatusCode::InvalidArgument),
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

/// Tiny prior-knowledge h2c unary client helper.
///
/// This is a blocking specimen/test helper, not a pooled Tina client
/// service. It exists to prove the native bytes/status path without
/// introducing hyper, tonic, or Tokio.
pub fn grpc_unary_call_h2c<Req, Resp>(
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
    assert!(value.len() < 127);
    out.push(value.len() as u8);
    out.extend_from_slice(value.as_bytes());
}

fn hpack_string(input: &[u8]) -> Result<(String, usize), GrpcError> {
    if input.is_empty() || input[0] & 0x80 != 0 {
        return Err(GrpcError::BadFrame);
    }
    let len = input[0] as usize;
    let end = 1 + len;
    if input.len() < end {
        return Err(GrpcError::BadFrame);
    }
    let value = std::str::from_utf8(&input[1..end]).map_err(|_| GrpcError::BadFrame)?;
    Ok((value.to_owned(), end))
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
}
