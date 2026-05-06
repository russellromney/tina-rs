//! Public HTTP request/response types exchanged between connection isolates
//! and service isolates.
//!
//! Wraps the `http` crate's `Method`, `StatusCode`, `HeaderMap`, and
//! `Version` types. Body is a bounded `Vec<u8>`; streaming bodies live in
//! 048c.

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
    /// `Content-Length` was missing on a method that requires a body
    /// length, or was present but invalid. Maps to `411 Length Required`
    /// or `400 Bad Request` per the underlying cause.
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
            Self::InvalidContentLength => StatusCode::LENGTH_REQUIRED,
            Self::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsupportedRequestTarget => StatusCode::BAD_REQUEST,
            Self::UnsupportedHttpVersion => StatusCode::HTTP_VERSION_NOT_SUPPORTED,
        }
    }
}

/// Configurable byte limits for the connection isolate. These bound the
/// memory a single connection may force the runtime to hold before the
/// service isolate is even reached.
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
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: 16 * 1024,
            max_body_bytes: 1024 * 1024,
            max_headers: 64,
        }
    }
}
