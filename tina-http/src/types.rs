//! Public HTTP request/response types exchanged between connection isolates
//! and service isolates.
//!
//! Wraps the `http` crate's `Method`, `StatusCode`, `HeaderMap`, and
//! `Version` types. Bodies are either buffered (`Vec<u8>`) or streamed
//! through a chunk-source isolate.

use http::{HeaderMap, Method, StatusCode, Version};

/// One parsed HTTP/1.x request, ready for service dispatch.
///
/// The connection isolate constructs one of these per request; the
/// service isolate receives it as its inbound message and replies with
/// an [`HttpResponse`]. Body is either fully buffered or streamed
/// through a chunk-source isolate — see
/// [`HttpRequestBody`].
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP method (`GET`, `POST`, ...).
    pub method: Method,
    /// Request-target path (and query). The first form does not parse the
    /// authority/scheme; absolute-form request lines are rejected as
    /// [`RequestParseError::UnsupportedRequestTarget`].
    pub path: String,
    /// Wire HTTP version. The first form accepts `HTTP/1.0` and `HTTP/1.1`.
    pub version: Version,
    /// All headers that arrived on the wire. Parsing rejects requests
    /// whose total header bytes exceed a configured limit.
    pub headers: HeaderMap,
    /// Request body. Buffered up to a threshold; larger bodies are
    /// dispatched as a [`HttpRequestBody::Stream`] the service pulls
    /// from. Either way, [`HttpLimits::max_body_bytes`] caps the total
    /// declared `Content-Length`.
    pub body: HttpRequestBody,
}

/// Buffered request body or streaming source.
#[derive(Debug, Clone)]
pub enum HttpRequestBody {
    /// All body bytes already accumulated by the connection isolate.
    Buffered(Vec<u8>),
    /// Streaming source: the service pulls chunks via
    /// `call(stream.source, crate::HttpConnectionMsg::body_next(), t)
    /// .reply(...)` until `RequestChunkReply::Eof`.
    Stream(crate::streaming::RequestStream),
    /// HTTP/2 streaming source. Services pull chunks via
    /// `call(stream.source, crate::Http2ConnectionMsg::body_next(), t)`.
    Http2Stream(crate::streaming::Http2RequestStream),
}

impl HttpRequestBody {
    /// Returns the buffered bytes, or `None` for a streaming body.
    pub fn as_buffered(&self) -> Option<&[u8]> {
        match self {
            Self::Buffered(bytes) => Some(bytes),
            Self::Stream(_) | Self::Http2Stream(_) => None,
        }
    }

    /// Returns total declared body length. For `Buffered` this is the
    /// vec length; for `Stream` it is the declared `Content-Length`.
    pub fn declared_length(&self) -> Option<usize> {
        match self {
            Self::Buffered(bytes) => Some(bytes.len()),
            Self::Stream(s) => {
                if s.chunked {
                    None
                } else {
                    Some(s.content_length)
                }
            }
            Self::Http2Stream(s) => s.content_length,
        }
    }

    pub fn has_body(&self) -> bool {
        match self {
            Self::Buffered(bytes) => !bytes.is_empty(),
            Self::Stream(s) => s.chunked || s.content_length > 0,
            Self::Http2Stream(s) => s.content_length != Some(0),
        }
    }
}

impl Default for HttpRequestBody {
    fn default() -> Self {
        Self::Buffered(Vec::new())
    }
}

impl From<Vec<u8>> for HttpRequestBody {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Buffered(bytes)
    }
}

// Equality with byte slices preserves the existing `assert_eq!(req.body,
// b"...")` test ergonomics. Streaming bodies always compare unequal.
impl PartialEq<[u8]> for HttpRequestBody {
    fn eq(&self, other: &[u8]) -> bool {
        match self {
            Self::Buffered(bytes) => bytes == other,
            Self::Stream(_) | Self::Http2Stream(_) => false,
        }
    }
}

impl<const N: usize> PartialEq<[u8; N]> for HttpRequestBody {
    fn eq(&self, other: &[u8; N]) -> bool {
        self == &other[..]
    }
}

impl PartialEq<&[u8]> for HttpRequestBody {
    fn eq(&self, other: &&[u8]) -> bool {
        self == *other
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for HttpRequestBody {
    fn eq(&self, other: &&[u8; N]) -> bool {
        self == &other[..]
    }
}

impl PartialEq<Vec<u8>> for HttpRequestBody {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self == other.as_slice()
    }
}

/// One HTTP/1.x response produced by a service isolate.
///
/// The connection isolate serialises this onto the wire. Body is either
/// buffered or streamed — see [`HttpResponseBody`].
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Status code.
    pub status: StatusCode,
    /// Wire HTTP version. Defaults to whatever the request declared.
    pub version: Version,
    /// Response headers.
    pub headers: HeaderMap,
    /// Response body.
    pub body: HttpResponseBody,
}

/// Response body. Three variants because three things can be true
/// about the body length:
///
/// - We have all the bytes in memory — `Buffered`.
/// - We know the length but stream the bytes — `Stream`
///   (`Content-Length` framing).
/// - We do not know the length and stream the bytes —
///   `ChunkedStream` (`Transfer-Encoding: chunked` framing).
///
/// Callers usually do not construct these variants directly. The
/// loud-API constructors on [`HttpResponse`] — [`HttpResponse::with_body`]
/// for buffered, [`HttpResponse::stream_known_length`] for
/// `Content-Length`, and [`HttpResponse::stream_chunked`] for
/// chunked — pick the variant for you.
#[derive(Debug, Clone)]
pub enum HttpResponseBody {
    /// All response bytes built up-front by the service.
    Buffered(Vec<u8>),
    /// Streaming source with declared length. The connection isolate
    /// pulls chunks via `call(stream.source, ResponseChunkMsg::Next,
    /// t).reply(...)` until `ResponseChunkReply::Eof`.
    Stream(crate::streaming::ResponseStream),
    /// Streaming source with no declared length. Wire framing is
    /// `Transfer-Encoding: chunked`. Same `Next`/`Chunk`/`Eof`
    /// exchange as `Stream`; only the framing differs.
    ChunkedStream(crate::streaming::ChunkedResponseStream),
    /// HTTP/1.1 upgrade to a Tina-owned WebSocket session. The
    /// connection writes the `101 Switching Protocols` response and
    /// then stops HTTP request handling; the same isolate owns the
    /// upgraded stream as a WebSocket session.
    WebSocket(Box<crate::websocket::WebSocketAccept>),
}

impl HttpResponseBody {
    /// Returns the buffered bytes, or `None` for a streaming body.
    pub fn as_buffered(&self) -> Option<&[u8]> {
        match self {
            Self::Buffered(bytes) => Some(bytes),
            Self::Stream(_) | Self::ChunkedStream(_) | Self::WebSocket(_) => None,
        }
    }

    /// Returns the declared body length when one is known. `None`
    /// for `ChunkedStream` because chunked framing is precisely the
    /// "no declared length" case.
    ///
    /// The `Option` is intentional: encoding callers should branch
    /// on `Some(n)` → emit `Content-Length: n` vs `None` → emit
    /// `Transfer-Encoding: chunked`. Returning a sentinel `0` for
    /// chunked would confuse that branch with a genuine zero-length
    /// body. If you only need the variant tag, match on `body`
    /// directly.
    pub fn declared_length(&self) -> Option<usize> {
        match self {
            Self::Buffered(bytes) => Some(bytes.len()),
            Self::Stream(s) => Some(s.content_length),
            Self::ChunkedStream(_) | Self::WebSocket(_) => None,
        }
    }
}

impl Default for HttpResponseBody {
    fn default() -> Self {
        Self::Buffered(Vec::new())
    }
}

impl From<Vec<u8>> for HttpResponseBody {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Buffered(bytes)
    }
}

// Equality with byte slices keeps `assert_eq!(resp.body, b"...")`
// ergonomic. Streaming bodies always compare unequal.
impl PartialEq<[u8]> for HttpResponseBody {
    fn eq(&self, other: &[u8]) -> bool {
        match self {
            Self::Buffered(bytes) => bytes == other,
            Self::Stream(_) | Self::ChunkedStream(_) | Self::WebSocket(_) => false,
        }
    }
}

impl<const N: usize> PartialEq<[u8; N]> for HttpResponseBody {
    fn eq(&self, other: &[u8; N]) -> bool {
        self == &other[..]
    }
}

impl PartialEq<&[u8]> for HttpResponseBody {
    fn eq(&self, other: &&[u8]) -> bool {
        self == *other
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for HttpResponseBody {
    fn eq(&self, other: &&[u8; N]) -> bool {
        self == &other[..]
    }
}

impl PartialEq<Vec<u8>> for HttpResponseBody {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self == other.as_slice()
    }
}

impl HttpResponse {
    /// Builds a `200 OK` response with no body.
    pub fn ok() -> Self {
        Self::with_status(StatusCode::OK)
    }

    /// Builds a response with the given status and no body.
    pub fn with_status(status: StatusCode) -> Self {
        Self {
            status,
            version: Version::HTTP_11,
            headers: HeaderMap::new(),
            body: HttpResponseBody::default(),
        }
    }

    /// Builds a `200 OK` response with a `text/plain` body.
    pub fn text(body: impl Into<String>) -> Self {
        let body = body.into().into_bytes();
        let mut response = Self::with_status(StatusCode::OK);
        response.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/plain"),
        );
        response.body = HttpResponseBody::Buffered(body);
        response
    }

    /// Builds a response with the given status and bytes as body. Caller is
    /// responsible for setting `Content-Type` if relevant.
    pub fn with_body(status: StatusCode, body: Vec<u8>) -> Self {
        let mut response = Self::with_status(status);
        response.body = HttpResponseBody::Buffered(body);
        response
    }

    /// Builds a response with a streaming body. The service must
    /// register the chunk source isolate separately and supply its
    /// address. The body is framed by `Content-Length`. For an
    /// unknown-length body use [`Self::stream_chunked`].
    pub fn with_stream(status: StatusCode, stream: crate::streaming::ResponseStream) -> Self {
        let mut response = Self::with_status(status);
        response.body = HttpResponseBody::Stream(stream);
        response
    }

    /// Loud-API constructor for a known-length streamed response.
    /// Wire framing is `Content-Length: {content_length}`. Source
    /// must deliver exactly `content_length` bytes across `Chunk`
    /// replies before `Eof`. Equivalent to
    /// `Self::with_stream(status, ResponseStream { content_length, source })`.
    pub fn stream_known_length(
        status: StatusCode,
        content_length: usize,
        source: tina::Address<
            crate::streaming::ResponseChunkMsg,
            crate::streaming::ResponseChunkReply,
        >,
    ) -> Self {
        Self::with_stream(
            status,
            crate::streaming::ResponseStream {
                content_length,
                source,
            },
        )
    }

    /// Loud-API constructor for an unknown-length streamed response.
    /// Wire framing is `Transfer-Encoding: chunked`. Source produces
    /// chunks until it replies `Eof`; the connection writes the
    /// chunked terminator on its own. Use this when the body length
    /// is not known up front. If you do know the length, prefer
    /// [`Self::stream_known_length`] so the client can size its
    /// read buffer.
    pub fn stream_chunked(
        status: StatusCode,
        source: tina::Address<
            crate::streaming::ResponseChunkMsg,
            crate::streaming::ResponseChunkReply,
        >,
    ) -> Self {
        let mut response = Self::with_status(status);
        response.body =
            HttpResponseBody::ChunkedStream(crate::streaming::ChunkedResponseStream { source });
        response
    }

    /// Builds a `101 Switching Protocols` response that hands the
    /// connection into the native WebSocket session loop after the
    /// upgrade head drains.
    pub fn websocket(accept: crate::websocket::WebSocketAccept) -> Self {
        let mut response = Self::with_status(StatusCode::SWITCHING_PROTOCOLS);
        response.version = Version::HTTP_11;
        response.headers.insert(
            http::header::UPGRADE,
            http::HeaderValue::from_static("websocket"),
        );
        response.headers.insert(
            http::header::CONNECTION,
            http::HeaderValue::from_static("Upgrade"),
        );
        response.headers.insert(
            http::HeaderName::from_static("sec-websocket-accept"),
            http::HeaderValue::from_str(accept.accept_key())
                .expect("computed websocket accept key is header-safe"),
        );
        response.body = HttpResponseBody::WebSocket(Box::new(accept));
        response
    }

    /// Builds a `text/plain` response with the given status and body.
    pub fn with_text(status: StatusCode, body: impl Into<String>) -> Self {
        let mut response = Self::text(body);
        response.status = status;
        response
    }

    /// Convenience: `400 Bad Request`.
    pub fn bad_request() -> Self {
        Self::with_status(StatusCode::BAD_REQUEST)
    }

    /// Convenience: `404 Not Found`.
    pub fn not_found() -> Self {
        Self::with_status(StatusCode::NOT_FOUND)
    }

    /// Convenience: `500 Internal Server Error`.
    pub fn internal_error() -> Self {
        Self::with_status(StatusCode::INTERNAL_SERVER_ERROR)
    }

    /// Convenience: `503 Service Unavailable`. Same shape the connection
    /// isolate emits on a `CallOutcome::Full` when the policy chooses to
    /// reply rather than close.
    pub fn service_unavailable() -> Self {
        Self::with_status(StatusCode::SERVICE_UNAVAILABLE)
    }

    /// Convenience: `502 Bad Gateway`. Useful for proxy services that
    /// receive a structured error from an upstream.
    pub fn bad_gateway() -> Self {
        Self::with_status(StatusCode::BAD_GATEWAY)
    }

    /// Convenience: `504 Gateway Timeout`. Mirrors the connection
    /// isolate's mapping for a service-call timeout.
    pub fn gateway_timeout() -> Self {
        Self::with_status(StatusCode::GATEWAY_TIMEOUT)
    }
}

impl HttpRequest {
    /// Borrows the buffered body as `&str`. Returns `None` for streaming
    /// bodies; returns `Err` for buffered bodies that aren't valid UTF-8.
    pub fn body_str(&self) -> Option<Result<&str, std::str::Utf8Error>> {
        self.body.as_buffered().map(std::str::from_utf8)
    }

    pub fn has_body(&self) -> bool {
        self.body.has_body()
    }
}

/// Reasons response parsing rejected an inbound response.
///
/// Mirror of the request-side errors. Each maps to an
/// [`crate::HttpClientError::Parse`] variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseParseError {
    /// Malformed status line.
    BadStatusLine,
    /// Header section exceeded the configured byte limit.
    HeadersTooLarge,
    /// `Transfer-Encoding` other than `identity`. Only `Content-Length`
    /// framing is supported.
    UnsupportedTransferEncoding,
    /// `Content-Length` failed to parse or had conflicting values.
    InvalidContentLength,
    /// `Content-Length` missing on a response that needs framing.
    MissingContentLength,
    /// Declared body length exceeded the configured byte limit.
    BodyTooLarge,
    /// Chunked transfer encoding produced malformed wire bytes.
    MalformedChunkedBody,
    /// The HTTP version on the wire was neither `HTTP/1.0` nor
    /// `HTTP/1.1`.
    UnsupportedHttpVersion,
}

/// Reasons request parsing rejected an inbound request before it reached
/// any service isolate.
///
/// Each variant maps to a typed connection-trace event and a typed HTTP
/// response status (or a connection close, where no clean response is
/// possible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestParseError {
    /// The request line was malformed — bad method/target/version syntax.
    /// Maps to `400 Bad Request`.
    BadRequestLine,
    /// The header section exceeded the configured byte limit.
    /// Maps to `431 Request Header Fields Too Large`.
    HeadersTooLarge,
    /// `Transfer-Encoding` was unsupported, or chunked request
    /// decoding was disabled by [`HttpLimits::inbound_stream_chunk_size`].
    /// Maps to `501 Not Implemented`.
    UnsupportedTransferEncoding,
    /// `Content-Length` failed to parse, was negative, or had two
    /// conflicting values. Maps to `400 Bad Request` per RFC 7230 §3.3.2.
    InvalidContentLength,
    /// The declared body length exceeded the configured byte limit.
    /// Maps to `413 Payload Too Large`.
    BodyTooLarge,
    /// Chunked transfer encoding produced malformed wire bytes.
    /// Maps to `400 Bad Request`.
    MalformedChunkedBody,
    /// The request line used an unsupported request-target form (e.g.
    /// absolute-form, authority-form, asterisk-form). Maps to `400 Bad
    /// Request`.
    UnsupportedRequestTarget,
    /// The HTTP version on the wire was neither `HTTP/1.0` nor
    /// `HTTP/1.1`. Maps to `505 HTTP Version Not Supported`.
    UnsupportedHttpVersion,
    /// The client did not send a complete request head before
    /// [`HttpLimits::header_read_timeout`] elapsed. The slow-loris case.
    /// Maps to `408 Request Timeout` when a response can be sent; the
    /// current connection isolate closes without writing the response while
    /// a read is pending.
    HeaderReadTimeout,
}

impl RequestParseError {
    /// Returns the HTTP status code this parser failure maps to. The
    /// connection isolate uses this to build a typed error response
    /// before closing the connection.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequestLine => StatusCode::BAD_REQUEST,
            Self::HeadersTooLarge => StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            Self::UnsupportedTransferEncoding => StatusCode::NOT_IMPLEMENTED,
            // RFC 7230 §3.3.2: conflicting/invalid Content-Length is a
            // 400 Bad Request, not a 411 Length Required.
            Self::InvalidContentLength => StatusCode::BAD_REQUEST,
            Self::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::MalformedChunkedBody => StatusCode::BAD_REQUEST,
            Self::UnsupportedRequestTarget => StatusCode::BAD_REQUEST,
            Self::UnsupportedHttpVersion => StatusCode::HTTP_VERSION_NOT_SUPPORTED,
            Self::HeaderReadTimeout => StatusCode::REQUEST_TIMEOUT,
        }
    }
}

/// Configurable byte and time limits for the connection isolate. These
/// bound the memory and wall-clock duration a single connection may
/// force the runtime to hold before the service isolate is reached.
#[derive(Debug, Clone, Copy)]
pub struct HttpLimits {
    /// Maximum bytes of header section (request line + headers + final
    /// CRLF). Rejected with [`RequestParseError::HeadersTooLarge`].
    pub max_header_bytes: usize,
    /// Maximum body length permitted via `Content-Length`. Rejected with
    /// [`RequestParseError::BodyTooLarge`].
    pub max_body_bytes: usize,
    /// Number of headers `httparse` may parse. Headers beyond this limit
    /// fail as a parse error.
    pub max_headers: usize,
    /// Maximum wall-clock time the connection isolate will wait for the client
    /// to finish sending the request head (status line + headers + terminating
    /// CRLF CRLF). Slow-loris-style clients that drip-feed bytes hit this and
    /// the connection closes without reaching the service.
    pub header_read_timeout: std::time::Duration,
    /// When `Some(chunk_size)`, the connection isolate dispatches a
    /// non-empty request body as a [`crate::HttpRequestBody::Stream`]
    /// source as soon as the head parses, then pulls body bytes from
    /// the socket on demand. The service drives the pull with
    /// `call(stream.source, crate::HttpConnectionMsg::body_next(), t)
    /// .reply(...)`; each call returns a chunk of up to `chunk_size`
    /// bytes (or `Eof`). The connection only issues `tcp_read` when
    /// the buffer is empty and more body is owed, so a slow service
    /// applies real backpressure to TCP reads.
    ///
    /// **Resident-bytes caveat.** Subsequent socket reads are
    /// capped by `chunk_size`, but the initial post-head residual
    /// is bounded only by the per-syscall ceiling (`READ_CHUNK = 4096`
    /// bytes). The first `tcp_read` is issued before we know the
    /// `Content-Length` and pulls up to `READ_CHUNK` bytes; whatever
    /// arrived past the head ends up resident in the chunk buffer
    /// even if `chunk_size` is smaller. After that initial slice
    /// drains, subsequent reads honor `chunk_size`. To bound the
    /// peak honestly under `chunk_size`, send the head and body in
    /// separate writes so the head-read returns no body bytes.
    ///
    /// When `None` (the default), the connection accumulates the full
    /// body up to `max_body_bytes` and dispatches a
    /// [`crate::HttpRequestBody::Buffered`].
    pub inbound_stream_chunk_size: Option<usize>,
    /// When `Some(d)`, the connection serves multiple sequential
    /// requests on one TCP/TLS stream (HTTP/1.1 keep-alive) and waits
    /// up to `d` for the next request's head to start (and complete)
    /// after the previous response was written. On idle expiry the
    /// connection closes cleanly, the same way it would on peer EOF.
    /// Per-request close intent is still honored: HTTP/1.0, an
    /// explicit `Connection: close` header, or any parse / service
    /// error closes immediately regardless of this knob.
    ///
    /// When `None` (the default), the connection serves one request
    /// per accept and closes — the legacy first-form behaviour.
    pub keepalive_idle_timeout: Option<std::time::Duration>,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: 16 * 1024,
            max_body_bytes: 1024 * 1024,
            max_headers: 64,
            header_read_timeout: std::time::Duration::from_secs(10),
            inbound_stream_chunk_size: None,
            keepalive_idle_timeout: None,
        }
    }
}

/// Server-side knobs: limits, timeout, and mailbox capacities.
///
/// `Copy` so callers can read it once to build the listener and again
/// to pass `listener_mailbox_capacity` into `register_with_capacity`.
#[derive(Debug, Clone, Copy)]
pub struct HttpServerConfig {
    /// Per-request byte and slow-loris limits.
    pub limits: HttpLimits,
    /// Per-call timeout into the service isolate. Exceeding it maps to
    /// `504 Gateway Timeout`.
    pub service_call_timeout: std::time::Duration,
    /// Mailbox size for each [`crate::HttpConnection`] child.
    pub connection_mailbox_capacity: usize,
    /// Mailbox size for the listener isolate itself.
    pub listener_mailbox_capacity: usize,
}

impl HttpServerConfig {
    /// Roomy preset: generous limits, 10s service timeout.
    pub fn dev() -> Self {
        Self {
            limits: HttpLimits::default(),
            service_call_timeout: std::time::Duration::from_secs(10),
            connection_mailbox_capacity: 16,
            listener_mailbox_capacity: 8,
        }
    }

    /// Tight preset for pressure tests: small limits, 1s service
    /// timeout.
    pub fn pressure() -> Self {
        Self {
            limits: HttpLimits {
                max_header_bytes: 4 * 1024,
                max_body_bytes: 64 * 1024,
                max_headers: 32,
                header_read_timeout: std::time::Duration::from_millis(500),
                inbound_stream_chunk_size: None,
                keepalive_idle_timeout: None,
            },
            service_call_timeout: std::time::Duration::from_secs(1),
            connection_mailbox_capacity: 8,
            listener_mailbox_capacity: 4,
        }
    }
}

impl Default for HttpServerConfig {
    fn default() -> Self {
        Self::dev()
    }
}

/// Client-side knobs: limits and request deadline.
#[derive(Debug, Clone, Copy)]
pub struct HttpClientConfig {
    /// Byte/time limits for parsing the response head.
    pub limits: HttpLimits,
    /// Wall-clock deadline for the whole request — connect, write,
    /// response head, response body, close. Exceeding it surfaces as
    /// [`HttpClientError::Timeout`].
    pub request_timeout: std::time::Duration,
}

impl HttpClientConfig {
    /// Roomy preset for development and example code: generous limits,
    /// 10s request deadline, mailbox capacity 16.
    pub fn dev() -> Self {
        Self {
            limits: HttpLimits::default(),
            request_timeout: std::time::Duration::from_secs(10),
        }
    }

    /// Tight preset for pressure tests: 1s request deadline.
    pub fn pressure() -> Self {
        Self {
            limits: HttpLimits {
                max_header_bytes: 4 * 1024,
                max_body_bytes: 64 * 1024,
                max_headers: 32,
                header_read_timeout: std::time::Duration::from_millis(500),
                inbound_stream_chunk_size: None,
                keepalive_idle_timeout: None,
            },
            request_timeout: std::time::Duration::from_secs(1),
        }
    }
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self::dev()
    }
}

/// Pool-side knobs.
///
/// `capacity` must be 1. Multi-slot pools are a separate slice.
#[derive(Debug, Clone, Copy)]
pub struct PoolConfig {
    /// Maximum number of in-flight HTTP calls. Must be `1`.
    pub capacity: usize,
    /// Per-call timeout the pool passes to the underlying
    /// [`crate::HttpClient`].
    pub client_call_timeout: std::time::Duration,
    /// Mailbox size for the pool isolate.
    pub mailbox_capacity: usize,
}

impl PoolConfig {
    /// Roomy preset: 10s client call timeout.
    pub fn dev() -> Self {
        Self {
            capacity: 1,
            client_call_timeout: std::time::Duration::from_secs(10),
            mailbox_capacity: 16,
        }
    }

    /// Tight preset for pressure tests: 1s client call timeout.
    pub fn pressure() -> Self {
        Self {
            capacity: 1,
            client_call_timeout: std::time::Duration::from_secs(1),
            mailbox_capacity: 8,
        }
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self::dev()
    }
}

/// Phase tag inside [`HttpClientError::Transport`]. `Bind` and
/// `Accept` exist for symmetry; the client only ever emits
/// `Connect`, `Read`, `Write`, `Close`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpTransportPhase {
    Connect,
    Bind,
    Accept,
    Read,
    Write,
    Close,
}

/// Error during chunked transfer decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkedError {
    BadChunkSize,
    MissingCrlf,
    BodyTooLarge,
    TrailersNotSupported,
}

/// Outbound call failed before producing a parsed [`HttpResponse`].
/// Plain HTTP keeps flat variants for source compat; HTTPS produces
/// [`HttpClientError::Transport`] carrying the typed
/// [`tina_runtime::CallError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpClientError {
    /// `tcp_connect` failed before the request could be written.
    Connect,
    /// `tcp_write` failed mid-request.
    Write,
    /// `tcp_read` failed mid-response.
    Read,
    /// The response parser rejected the bytes from the server.
    Parse(ResponseParseError),
    /// Peer closed before the response head/body completed.
    Closed,
    /// `request_timeout` elapsed before delivery.
    Timeout,
    /// The client isolate already had a call in flight when this one
    /// arrived. Use a pool for explicit admission.
    Busy,
    /// The pool's slot was busy when this Submit arrived. Direct (non-
    /// pooled) calls never produce this variant.
    PoolFull,
    /// TLS lane call failed. `source` is the typed `CallError`
    /// (`TlsName`, `TlsCertificate`, `TlsHandshake`, `TlsFull`,
    /// `TlsClosed`, `Timeout`, `Io`, ...).
    Transport {
        phase: HttpTransportPhase,
        source: tina_runtime::CallError,
    },
    /// Caller already set `Host:` on an HTTPS target — conflicts
    /// with the [`crate::HttpHostPolicy`].
    DuplicateHostHeader,
    /// [`crate::HttpHostPolicy`] resolved to a string with bytes
    /// that are not a valid `HeaderValue`.
    InvalidHostHeaderValue,
}
