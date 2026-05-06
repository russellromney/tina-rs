//! Public HTTP request/response types exchanged between connection isolates
//! and service isolates.
//!
//! Wraps the `http` crate's `Method`, `StatusCode`, `HeaderMap`, and
//! `Version` types. Body is a bounded `Vec<u8>`; streaming bodies are out
//! of scope here.

use http::{HeaderMap, Method, StatusCode, Version};

/// One parsed HTTP/1.x request, ready for service dispatch.
///
/// The connection isolate constructs one of these per request; the service
/// isolate receives it as its inbound message and replies with an
/// [`HttpResponse`].
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
    /// Bounded request body. The first form requires `Content-Length`;
    /// chunked transfer encoding is rejected as
    /// [`RequestParseError::UnsupportedTransferEncoding`].
    pub body: Vec<u8>,
}

/// One HTTP/1.x response produced by a service isolate.
///
/// The connection isolate serialises this onto the wire. The first form
/// does not stream response bodies; large responses must fit in memory.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Status code.
    pub status: StatusCode,
    /// Wire HTTP version. Defaults to whatever the request declared.
    pub version: Version,
    /// Response headers.
    pub headers: HeaderMap,
    /// Bounded response body.
    pub body: Vec<u8>,
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
            body: Vec::new(),
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
        response.body = body;
        response
    }

    /// Builds a response with the given status and bytes as body. Caller is
    /// responsible for setting `Content-Type` if relevant.
    pub fn with_body(status: StatusCode, body: Vec<u8>) -> Self {
        let mut response = Self::with_status(status);
        response.body = body;
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
    /// Borrows the body as a `&str`. Returns the underlying UTF-8 error if
    /// the bytes are not valid UTF-8.
    pub fn body_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.body)
    }

    /// Returns `true` if the request carries any body bytes.
    pub fn has_body(&self) -> bool {
        !self.body.is_empty()
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
    /// `Transfer-Encoding` was set to a value other than `identity`.
    /// First form only supports `Content-Length`. Maps to `501 Not
    /// Implemented`.
    UnsupportedTransferEncoding,
    /// `Content-Length` failed to parse, was negative, or had two
    /// conflicting values. Maps to `400 Bad Request` per RFC 7230 §3.3.2.
    InvalidContentLength,
    /// The declared body length exceeded the configured byte limit.
    /// Maps to `413 Payload Too Large`.
    BodyTooLarge,
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
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: 16 * 1024,
            max_body_bytes: 1024 * 1024,
            max_headers: 64,
            header_read_timeout: std::time::Duration::from_secs(10),
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

/// Reasons an outbound HTTP call failed before producing a parsed
/// [`HttpResponse`].
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
}
