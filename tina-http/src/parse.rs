//! HTTP/1.1 request head parsing.
//!
//! Wraps `httparse` so the connection isolate stays a state machine over
//! [`ParseProgress`] outcomes:
//!
//! - [`ParseProgress::NeedMore`] means the buffer holds a partial request
//!   head; the connection should issue another `tcp_read`.
//! - [`ParseProgress::Complete`] returns the parsed [`HttpRequestHead`]
//!   plus the byte offset where the body begins inside the buffer.
//! - [`ParseProgress::Failed`] returns a typed [`RequestParseError`] that
//!   maps to a status code and a connection-trace event.
//!
//! Body bytes are not consumed by the parser. The connection isolate is
//! responsible for accumulating up to `Content-Length` bytes (the parser
//! reports the declared length) and constructing the full
//! [`HttpRequest`].

use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version};

use crate::types::{HttpLimits, HttpRequest, RequestParseError, ResponseParseError};

/// Result of attempting to parse a request head out of a buffer.
#[derive(Debug)]
pub enum ParseProgress {
    /// Buffer does not yet contain a complete request head. The connection
    /// must read more bytes and try again.
    NeedMore,
    /// Buffer contains a complete request head.
    Complete {
        /// The parsed head — method, path, version, headers — plus any
        /// body length declared via `Content-Length`.
        head: HttpRequestHead,
        /// Byte index where the body begins inside the input buffer. Body
        /// bytes that already arrived are at `buffer[head_len..]`.
        head_len: usize,
    },
    /// Parsing rejected the request before completion.
    Failed(RequestParseError),
}

/// Parsed HTTP request head with no body.
///
/// The body is filled in by the connection isolate after the head parses,
/// then folded into a final [`HttpRequest`] via
/// [`HttpRequestHead::into_request`].
#[derive(Debug)]
pub struct HttpRequestHead {
    /// Parsed method.
    pub method: Method,
    /// Request-target path (origin-form: `/path?query`).
    pub path: String,
    /// HTTP wire version (1.0 or 1.1).
    pub version: Version,
    /// Parsed headers.
    pub headers: HeaderMap,
    /// Declared body length from `Content-Length`, or zero if absent.
    pub content_length: usize,
    /// `true` when `Transfer-Encoding: chunked` was present and accepted.
    pub chunked: bool,
    /// Whether the connection should be closed after this response (per
    /// `Connection: close` header or HTTP/1.0 default).
    pub connection_close: bool,
}

impl HttpRequestHead {
    /// Folds this head plus a body buffer into a complete request.
    pub fn into_request(self, body: Vec<u8>) -> HttpRequest {
        HttpRequest {
            method: self.method,
            path: self.path,
            version: self.version,
            headers: self.headers,
            body: crate::types::HttpRequestBody::Buffered(body),
        }
    }

    /// Builds a request whose body is a streaming source. Used by the
    /// connection isolate when a body exceeds the buffered threshold.
    pub fn into_streaming_request(self, stream: crate::streaming::RequestStream) -> HttpRequest {
        HttpRequest {
            method: self.method,
            path: self.path,
            version: self.version,
            headers: self.headers,
            body: crate::types::HttpRequestBody::Stream(stream),
        }
    }
}

/// Number of headers we can parse without heap allocation. Anything
/// configured beyond this in [`HttpLimits::max_headers`] falls back to a
/// per-call `Vec`. The default limit (64) fits inline.
const MAX_INLINE_HEADERS: usize = 64;

/// Attempts to parse an HTTP/1.x request head out of `buffer`.
///
/// This function is pure and does not consume `buffer`. The connection
/// isolate calls it after each `tcp_read`; on `NeedMore`, it reads more
/// bytes; on `Complete`, it slices the body off `buffer[head_len..]`.
///
/// Allocation: the common case (`max_headers <= 64`) uses a stack array
/// for the header slot. Larger limits fall back to a heap `Vec`.
pub fn parse_request_head(buffer: &[u8], limits: &HttpLimits) -> ParseProgress {
    let mut inline_headers = [httparse::EMPTY_HEADER; MAX_INLINE_HEADERS];
    let mut heap_headers: Vec<httparse::Header<'_>>;
    let header_slots: &mut [httparse::Header<'_>] = if limits.max_headers <= MAX_INLINE_HEADERS {
        &mut inline_headers[..limits.max_headers]
    } else {
        heap_headers = vec![httparse::EMPTY_HEADER; limits.max_headers];
        &mut heap_headers
    };
    let mut req = httparse::Request::new(header_slots);

    match req.parse(buffer) {
        Ok(httparse::Status::Partial) => {
            if buffer.len() >= limits.max_header_bytes {
                ParseProgress::Failed(RequestParseError::HeadersTooLarge)
            } else {
                ParseProgress::NeedMore
            }
        }
        Ok(httparse::Status::Complete(head_len)) => {
            if head_len > limits.max_header_bytes {
                return ParseProgress::Failed(RequestParseError::HeadersTooLarge);
            }
            let parsed = match build_head(&req, limits) {
                Ok(head) => head,
                Err(error) => return ParseProgress::Failed(error),
            };
            ParseProgress::Complete {
                head: parsed,
                head_len,
            }
        }
        Err(httparse::Error::TooManyHeaders) => {
            ParseProgress::Failed(RequestParseError::HeadersTooLarge)
        }
        Err(_) => ParseProgress::Failed(RequestParseError::BadRequestLine),
    }
}

fn build_head(
    parsed: &httparse::Request<'_, '_>,
    limits: &HttpLimits,
) -> Result<HttpRequestHead, RequestParseError> {
    let method_str = parsed.method.ok_or(RequestParseError::BadRequestLine)?;
    let method =
        Method::from_bytes(method_str.as_bytes()).map_err(|_| RequestParseError::BadRequestLine)?;

    let raw_path = parsed.path.ok_or(RequestParseError::BadRequestLine)?;
    if !is_origin_form(raw_path) {
        return Err(RequestParseError::UnsupportedRequestTarget);
    }
    let path = raw_path.to_owned();

    let version = match parsed.version {
        Some(0) => Version::HTTP_10,
        Some(1) => Version::HTTP_11,
        Some(_) => return Err(RequestParseError::UnsupportedHttpVersion),
        None => return Err(RequestParseError::BadRequestLine),
    };

    let mut headers = HeaderMap::with_capacity(parsed.headers.len());
    let mut content_length: Option<usize> = None;
    let mut transfer_encoding_chunked = false;
    let mut transfer_encoding_unsupported = false;
    let mut connection_close = matches!(version, Version::HTTP_10);

    for header in parsed.headers.iter() {
        if header.name.is_empty() {
            continue;
        }
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| RequestParseError::BadRequestLine)?;
        let value =
            HeaderValue::from_bytes(header.value).map_err(|_| RequestParseError::BadRequestLine)?;

        if name == http::header::CONTENT_LENGTH {
            let value_str = std::str::from_utf8(header.value)
                .map_err(|_| RequestParseError::InvalidContentLength)?;
            let trimmed = value_str.trim();
            if trimmed.is_empty() || !trimmed.as_bytes().iter().all(u8::is_ascii_digit) {
                return Err(RequestParseError::InvalidContentLength);
            }
            let parsed_len: usize = trimmed
                .parse()
                .map_err(|_| RequestParseError::InvalidContentLength)?;
            if parsed_len > limits.max_body_bytes {
                return Err(RequestParseError::BodyTooLarge);
            }
            if let Some(prev) = content_length {
                if prev != parsed_len {
                    return Err(RequestParseError::InvalidContentLength);
                }
            }
            content_length = Some(parsed_len);
        } else if name == http::header::TRANSFER_ENCODING {
            let value_str = std::str::from_utf8(header.value).unwrap_or("");
            let parsed = parse_transfer_encoding(value_str);
            transfer_encoding_chunked |= parsed.chunked;
            transfer_encoding_unsupported |= parsed.unsupported;
        } else if name == http::header::CONNECTION {
            let value_str = std::str::from_utf8(header.value).unwrap_or("");
            let has_close = value_str
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("close"));
            if has_close {
                connection_close = true;
            } else if !connection_close {
                let has_keep_alive = value_str
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("keep-alive"));
                if has_keep_alive {
                    connection_close = false;
                }
            }
        }

        headers.append(name, value);
    }

    if transfer_encoding_unsupported {
        return Err(RequestParseError::UnsupportedTransferEncoding);
    }

    if transfer_encoding_chunked && content_length.is_some() {
        return Err(RequestParseError::InvalidContentLength);
    }

    if transfer_encoding_chunked && limits.inbound_stream_chunk_size.is_none() {
        return Err(RequestParseError::UnsupportedTransferEncoding);
    }

    Ok(HttpRequestHead {
        method,
        path,
        version,
        headers,
        content_length: content_length.unwrap_or(0),
        chunked: transfer_encoding_chunked,
        connection_close,
    })
}

fn is_origin_form(target: &str) -> bool {
    // Origin-form is `/...` or `*` (asterisk-form for OPTIONS — we reject
    // it for first form). Authority-form (`example.com:80`) and
    // absolute-form (`http://example.com/`) are both rejected.
    target.starts_with('/')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransferEncodingFlags {
    chunked: bool,
    unsupported: bool,
}

fn parse_transfer_encoding(value: &str) -> TransferEncodingFlags {
    let mut flags = TransferEncodingFlags {
        chunked: false,
        unsupported: false,
    };
    for token in value.split(',') {
        let token = token.trim();
        if token.is_empty() || token.eq_ignore_ascii_case("identity") {
            continue;
        }
        if token.eq_ignore_ascii_case("chunked") {
            flags.chunked = true;
        } else {
            flags.unsupported = true;
        }
    }
    flags
}

/// Serialises a response head onto a wire buffer using HTTP/1.1 framing.
///
/// Emits status line + headers + framing line + optional
/// `Connection: close` + the empty terminator line. The framing
/// line is picked from the body variant:
///
/// - `Buffered(_)` / `Stream(_)` → `Content-Length: N`.
/// - `ChunkedStream(_)`         → `Transfer-Encoding: chunked`.
///
/// **Caller-supplied `Content-Length` and `Transfer-Encoding`
/// headers are dropped.** Framing is owned by the body type, not
/// the caller. If the caller wants chunked, they build a
/// `ChunkedStream` body; setting `Transfer-Encoding: chunked` on a
/// `Buffered(_)` body has no effect on the wire.
///
/// Body bytes are *not* appended — the connection isolate writes
/// them separately so streaming can write chunk by chunk.
pub fn encode_response_head(
    response: &crate::types::HttpResponse,
    connection_close: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);

    let version_str = match response.version {
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        _ => "HTTP/1.1",
    };
    let status = response.status;
    let reason = status.canonical_reason().unwrap_or("");
    out.extend_from_slice(version_str.as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(status.as_str().as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(reason.as_bytes());
    out.extend_from_slice(b"\r\n");

    let mut wrote_connection = false;
    for (name, value) in response.headers.iter() {
        if name == http::header::CONTENT_LENGTH || name == http::header::TRANSFER_ENCODING {
            // We pick the framing header based on the body variant,
            // so a caller-supplied `Content-Length` /
            // `Transfer-Encoding` is dropped. Anything else we emit
            // verbatim.
            continue;
        } else if name == http::header::CONNECTION {
            if connection_close {
                continue;
            }
            wrote_connection = true;
        }
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    // Framing: `Content-Length` when known, `Transfer-Encoding:
    // chunked` when not. The connection writer follows the same
    // branch when it frames each chunk on the wire.
    match response.body.declared_length() {
        Some(length) => {
            out.extend_from_slice(b"Content-Length: ");
            out.extend_from_slice(length.to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        None => {
            out.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
        }
    }
    if connection_close && !wrote_connection {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// Serialises a buffered response (head + body) onto wire bytes.
///
/// Convenience for buffered responses; equivalent to
/// [`encode_response_head`] followed by appending the body bytes.
/// Panics if the response body is a streaming source — streamed
/// responses use the head encoder and write chunks separately.
pub fn encode_response(response: &crate::types::HttpResponse, connection_close: bool) -> Vec<u8> {
    let mut out = encode_response_head(response, connection_close);
    match &response.body {
        crate::types::HttpResponseBody::Buffered(bytes) => out.extend_from_slice(bytes),
        crate::types::HttpResponseBody::Stream(_)
        | crate::types::HttpResponseBody::ChunkedStream(_) => {
            panic!("encode_response cannot serialise a streaming body; use encode_response_head")
        }
    }
    out
}

/// Serialises a client-side request onto wire bytes.
///
/// Always emits `Content-Length` (zero when no body) and
/// `Connection: close` — one request per connection, no keep-alive
/// on the client. For the keepalive caller use
/// [`encode_keepalive_request`]. Caller-supplied `Connection` and
/// `Content-Length` headers are dropped — the encoder owns framing.
pub fn encode_request(request: &HttpRequest) -> Vec<u8> {
    encode_request_internal(request, true)
}

/// Like [`encode_request`] but omits `Connection: close` so HTTP/1.1
/// keep-alive defaults apply and the server holds the socket open
/// for the next request. Used by [`crate::KeepaliveConnection`].
pub fn encode_keepalive_request(request: &HttpRequest) -> Vec<u8> {
    encode_request_internal(request, false)
}

fn encode_request_internal(request: &HttpRequest, connection_close: bool) -> Vec<u8> {
    let body_bytes: &[u8] = match &request.body {
        crate::types::HttpRequestBody::Buffered(bytes) => bytes,
        crate::types::HttpRequestBody::Stream(_) => {
            panic!("encode_request cannot serialise a streaming body")
        }
    };
    let mut out = Vec::with_capacity(128 + body_bytes.len());

    let version_str = match request.version {
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        _ => "HTTP/1.1",
    };
    out.extend_from_slice(request.method.as_str().as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(request.path.as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(version_str.as_bytes());
    out.extend_from_slice(b"\r\n");

    for (name, value) in request.headers.iter() {
        if name == http::header::CONTENT_LENGTH || name == http::header::CONNECTION {
            continue;
        }
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"Content-Length: ");
    out.extend_from_slice(body_bytes.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    if connection_close {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body_bytes);
    out
}

/// Result of attempting to parse a response head out of a buffer.
#[derive(Debug)]
pub enum ResponseParseProgress {
    /// Buffer does not yet contain a complete response head.
    NeedMore,
    /// Buffer contains a complete response head.
    Complete {
        /// Parsed status line, version, headers, and declared body length.
        head: HttpResponseHead,
        /// Byte index where the body begins inside the input buffer.
        head_len: usize,
    },
    /// Parsing rejected the response before completion.
    Failed(ResponseParseError),
}

/// Parsed HTTP response head with no body.
#[derive(Debug)]
pub struct HttpResponseHead {
    /// HTTP wire version (1.0 or 1.1).
    pub version: Version,
    /// Status code.
    pub status: StatusCode,
    /// Reason phrase (may be empty).
    pub reason: String,
    /// Parsed headers.
    pub headers: HeaderMap,
    /// Declared body length from `Content-Length`. The first-form client
    /// requires this for any response that frames a body.
    pub content_length: usize,
    /// `true` when `Transfer-Encoding: chunked` was present.
    pub chunked: bool,
}

/// Attempts to parse an HTTP/1.x response head out of `buffer`.
///
/// Pure: does not consume `buffer`. Pairs with `parse_request_head` on the
/// server side. Re-uses the same `httparse`-backed parser path so the
/// same byte-budget invariants apply (header section bounded by
/// `limits.max_header_bytes`, header count bounded by `limits.max_headers`).
pub fn parse_response_head(buffer: &[u8], limits: &HttpLimits) -> ResponseParseProgress {
    let mut inline_headers = [httparse::EMPTY_HEADER; MAX_INLINE_HEADERS];
    let mut heap_headers: Vec<httparse::Header<'_>>;
    let header_slots: &mut [httparse::Header<'_>] = if limits.max_headers <= MAX_INLINE_HEADERS {
        &mut inline_headers[..limits.max_headers]
    } else {
        heap_headers = vec![httparse::EMPTY_HEADER; limits.max_headers];
        &mut heap_headers
    };
    let mut response = httparse::Response::new(header_slots);

    match response.parse(buffer) {
        Ok(httparse::Status::Partial) => {
            if buffer.len() >= limits.max_header_bytes {
                ResponseParseProgress::Failed(ResponseParseError::HeadersTooLarge)
            } else {
                ResponseParseProgress::NeedMore
            }
        }
        Ok(httparse::Status::Complete(head_len)) => {
            if head_len > limits.max_header_bytes {
                return ResponseParseProgress::Failed(ResponseParseError::HeadersTooLarge);
            }
            match build_response_head(&response, limits) {
                Ok(head) => ResponseParseProgress::Complete { head, head_len },
                Err(error) => ResponseParseProgress::Failed(error),
            }
        }
        Err(httparse::Error::TooManyHeaders) => {
            ResponseParseProgress::Failed(ResponseParseError::HeadersTooLarge)
        }
        Err(_) => ResponseParseProgress::Failed(ResponseParseError::BadStatusLine),
    }
}

fn build_response_head(
    parsed: &httparse::Response<'_, '_>,
    limits: &HttpLimits,
) -> Result<HttpResponseHead, ResponseParseError> {
    let version = match parsed.version {
        Some(0) => Version::HTTP_10,
        Some(1) => Version::HTTP_11,
        Some(_) => return Err(ResponseParseError::UnsupportedHttpVersion),
        None => return Err(ResponseParseError::BadStatusLine),
    };
    let code = parsed.code.ok_or(ResponseParseError::BadStatusLine)?;
    let status = StatusCode::from_u16(code).map_err(|_| ResponseParseError::BadStatusLine)?;
    let reason = parsed.reason.unwrap_or("").to_owned();

    let mut headers = HeaderMap::with_capacity(parsed.headers.len());
    let mut content_length: Option<usize> = None;
    let mut transfer_encoding_chunked = false;
    let mut transfer_encoding_unsupported = false;

    for header in parsed.headers.iter() {
        if header.name.is_empty() {
            continue;
        }
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| ResponseParseError::BadStatusLine)?;
        let value =
            HeaderValue::from_bytes(header.value).map_err(|_| ResponseParseError::BadStatusLine)?;

        if name == http::header::CONTENT_LENGTH {
            let value_str = std::str::from_utf8(header.value)
                .map_err(|_| ResponseParseError::InvalidContentLength)?;
            let trimmed = value_str.trim();
            if trimmed.is_empty() || !trimmed.as_bytes().iter().all(u8::is_ascii_digit) {
                return Err(ResponseParseError::InvalidContentLength);
            }
            let parsed_len: usize = trimmed
                .parse()
                .map_err(|_| ResponseParseError::InvalidContentLength)?;
            if parsed_len > limits.max_body_bytes {
                return Err(ResponseParseError::BodyTooLarge);
            }
            if let Some(prev) = content_length {
                if prev != parsed_len {
                    return Err(ResponseParseError::InvalidContentLength);
                }
            }
            content_length = Some(parsed_len);
        } else if name == http::header::TRANSFER_ENCODING {
            let value_str = std::str::from_utf8(header.value).unwrap_or("");
            let parsed = parse_transfer_encoding(value_str);
            transfer_encoding_chunked |= parsed.chunked;
            transfer_encoding_unsupported |= parsed.unsupported;
        }

        headers.append(name, value);
    }

    if transfer_encoding_unsupported {
        return Err(ResponseParseError::UnsupportedTransferEncoding);
    }

    if transfer_encoding_chunked && content_length.is_some() {
        return Err(ResponseParseError::InvalidContentLength);
    }

    let body_forbidden = matches!(status.as_u16(), 100..=199 | 204 | 304);
    let content_length = match content_length {
        Some(n) => n,
        None if body_forbidden || transfer_encoding_chunked => 0,
        None => return Err(ResponseParseError::MissingContentLength),
    };

    Ok(HttpResponseHead {
        version,
        status,
        reason,
        headers,
        content_length,
        chunked: transfer_encoding_chunked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;

    fn limits() -> HttpLimits {
        HttpLimits::default()
    }

    #[test]
    fn parses_simple_get() {
        let buf = b"GET /counter HTTP/1.1\r\nHost: localhost\r\n\r\n";
        match parse_request_head(buf, &limits()) {
            ParseProgress::Complete { head, head_len } => {
                assert_eq!(head.method, Method::GET);
                assert_eq!(head.path, "/counter");
                assert_eq!(head.version, Version::HTTP_11);
                assert_eq!(head.content_length, 0);
                assert!(!head.connection_close);
                assert_eq!(head_len, buf.len());
            }
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn parses_post_with_content_length() {
        let buf =
            b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 11\r\n\r\nhello world";
        match parse_request_head(buf, &limits()) {
            ParseProgress::Complete { head, head_len } => {
                assert_eq!(head.method, Method::POST);
                assert_eq!(head.path, "/echo");
                assert_eq!(head.content_length, 11);
                assert_eq!(&buf[head_len..], b"hello world");
            }
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn request_header_limit_counts_head_not_already_read_body() {
        let head = b"POST /x HTTP/1.1\r\nContent-Length: 64\r\n\r\n";
        let mut buf = head.to_vec();
        buf.extend(vec![b'a'; 64]);
        let limits = HttpLimits {
            max_header_bytes: head.len(),
            max_body_bytes: 128,
            ..HttpLimits::default()
        };
        match parse_request_head(&buf, &limits) {
            ParseProgress::Complete {
                head: parsed,
                head_len,
            } => {
                assert_eq!(head_len, head.len());
                assert_eq!(parsed.content_length, 64);
            }
            other => panic!("expected complete despite body bytes, got {other:?}"),
        }
    }

    #[test]
    fn signals_need_more_on_partial_head() {
        let buf = b"GET /counter HTTP/1.1\r\nHost: loc";
        match parse_request_head(buf, &limits()) {
            ParseProgress::NeedMore => {}
            other => panic!("expected NeedMore, got {other:?}"),
        }
    }

    #[test]
    fn rejects_chunked_request_body_when_streaming_disabled() {
        let buf = b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n";
        let limits = HttpLimits::default(); // inbound_stream_chunk_size = None
        match parse_request_head(buf, &limits) {
            ParseProgress::Failed(RequestParseError::UnsupportedTransferEncoding) => {}
            other => panic!("expected UnsupportedTransferEncoding, got {other:?}"),
        }
    }

    #[test]
    fn accepts_chunked_request_body_when_streaming_enabled() {
        let buf = b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n";
        let limits = HttpLimits {
            inbound_stream_chunk_size: Some(1024),
            ..HttpLimits::default()
        };
        match parse_request_head(buf, &limits) {
            ParseProgress::Complete { head, head_len } => {
                assert!(head.chunked);
                assert_eq!(head.content_length, 0);
                assert_eq!(head_len, buf.len());
            }
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn accepts_chunked_request_transfer_encoding_tokens() {
        let buf = b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: identity, chunked \r\n\r\n";
        let limits = HttpLimits {
            inbound_stream_chunk_size: Some(1024),
            ..HttpLimits::default()
        };
        match parse_request_head(buf, &limits) {
            ParseProgress::Complete { head, .. } => assert!(head.chunked),
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unsupported_request_transfer_encoding_token() {
        let buf =
            b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: gzip, chunked\r\n\r\n";
        let limits = HttpLimits {
            inbound_stream_chunk_size: Some(1024),
            ..HttpLimits::default()
        };
        match parse_request_head(buf, &limits) {
            ParseProgress::Failed(RequestParseError::UnsupportedTransferEncoding) => {}
            other => panic!("expected UnsupportedTransferEncoding, got {other:?}"),
        }
    }

    #[test]
    fn rejects_content_length_plus_transfer_encoding() {
        let buf = b"POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n";
        let limits = HttpLimits {
            inbound_stream_chunk_size: Some(1024),
            ..HttpLimits::default()
        };
        match parse_request_head(buf, &limits) {
            ParseProgress::Failed(RequestParseError::InvalidContentLength) => {}
            other => panic!("expected InvalidContentLength, got {other:?}"),
        }
    }

    #[test]
    fn rejects_oversized_body_via_content_length() {
        let huge = HttpLimits::default().max_body_bytes + 1;
        let buf_str =
            format!("POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: {huge}\r\n\r\n");
        match parse_request_head(buf_str.as_bytes(), &limits()) {
            ParseProgress::Failed(RequestParseError::BodyTooLarge) => {}
            other => panic!("expected BodyTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn rejects_oversized_header_section() {
        let huge_value = "x".repeat(HttpLimits::default().max_header_bytes);
        let buf = format!("GET / HTTP/1.1\r\nX-Huge: {huge_value}\r\n\r\n");
        match parse_request_head(buf.as_bytes(), &limits()) {
            ParseProgress::Failed(RequestParseError::HeadersTooLarge) => {}
            other => panic!("expected HeadersTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn rejects_absolute_form_request_target() {
        let buf = b"GET http://example.com/path HTTP/1.1\r\nHost: localhost\r\n\r\n";
        match parse_request_head(buf, &limits()) {
            ParseProgress::Failed(RequestParseError::UnsupportedRequestTarget) => {}
            other => panic!("expected UnsupportedRequestTarget, got {other:?}"),
        }
    }

    #[test]
    fn http10_defaults_to_connection_close() {
        let buf = b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n";
        match parse_request_head(buf, &limits()) {
            ParseProgress::Complete { head, .. } => {
                assert_eq!(head.version, Version::HTTP_10);
                assert!(head.connection_close);
            }
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn connection_close_header_overrides_http11_default() {
        let buf = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        match parse_request_head(buf, &limits()) {
            ParseProgress::Complete { head, .. } => {
                assert_eq!(head.version, Version::HTTP_11);
                assert!(head.connection_close);
            }
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn connection_close_wins_over_trailing_keep_alive() {
        // Regression: previously the per-token loop unset connection_close
        // when `keep-alive` followed `close`. RFC 7230 §6.1: a `close`
        // token anywhere in the Connection header commits the sender to
        // closing.
        let buf = b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close, keep-alive\r\n\r\n";
        match parse_request_head(buf, &limits()) {
            ParseProgress::Complete { head, .. } => {
                assert!(
                    head.connection_close,
                    "close must win over a trailing keep-alive token"
                );
            }
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn connection_close_wins_when_keep_alive_appears_first() {
        let buf = b"GET / HTTP/1.1\r\nHost: x\r\nConnection: keep-alive, close\r\n\r\n";
        match parse_request_head(buf, &limits()) {
            ParseProgress::Complete { head, .. } => {
                assert!(head.connection_close, "close must win regardless of order");
            }
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn conflicting_content_length_returns_400_not_411() {
        // RFC 7230 §3.3.2: two Content-Length headers with different
        // values must be rejected as 400 Bad Request, not 411 Length
        // Required. Regression for a previous mapping bug.
        let buf =
            b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello";
        match parse_request_head(buf, &limits()) {
            ParseProgress::Failed(error @ RequestParseError::InvalidContentLength) => {
                assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
            }
            other => panic!("expected InvalidContentLength + 400, got {other:?}"),
        }
    }

    #[test]
    fn equal_content_length_repeated_is_accepted() {
        // RFC 7230 §3.3.2: two Content-Length headers carrying the same
        // numeric value are equivalent to a single one and parse cleanly.
        let buf =
            b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello";
        match parse_request_head(buf, &limits()) {
            ParseProgress::Complete { head, .. } => {
                assert_eq!(head.content_length, 5);
            }
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn invalid_content_length_value_returns_400() {
        let buf = b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: abc\r\n\r\n";
        match parse_request_head(buf, &limits()) {
            ParseProgress::Failed(error @ RequestParseError::InvalidContentLength) => {
                assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
            }
            other => panic!("expected InvalidContentLength, got {other:?}"),
        }
    }

    #[test]
    fn content_length_must_be_digits_only() {
        for bad in ["+5", "-1", "5.0", ""] {
            let buf = format!("POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: {bad}\r\n\r\n");
            match parse_request_head(buf.as_bytes(), &limits()) {
                ParseProgress::Failed(RequestParseError::InvalidContentLength) => {}
                other => panic!("expected InvalidContentLength for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn header_read_timeout_maps_to_408() {
        assert_eq!(
            RequestParseError::HeaderReadTimeout.status(),
            http::StatusCode::REQUEST_TIMEOUT,
        );
    }

    #[test]
    fn parses_with_more_than_inline_headers_via_heap_path() {
        // Regression: the inline header path uses a 64-slot stack array.
        // When the user configures HttpLimits::max_headers > 64, the
        // parser must fall back to a heap allocation rather than
        // truncating. We construct a request with 80 headers and a
        // limits override of 128.
        let mut buf = String::from("GET / HTTP/1.1\r\nHost: x\r\n");
        for i in 0..80 {
            buf.push_str(&format!("X-Custom-{i}: v{i}\r\n"));
        }
        buf.push_str("\r\n");

        let limits = HttpLimits {
            max_headers: 128,
            // Bump byte limit to fit the headers we just built.
            max_header_bytes: 64 * 1024,
            ..HttpLimits::default()
        };
        match parse_request_head(buf.as_bytes(), &limits) {
            ParseProgress::Complete { head, .. } => {
                // 81 = Host + 80 X-Custom-* headers.
                assert_eq!(head.headers.len(), 81);
            }
            other => panic!("expected complete via heap path, got {other:?}"),
        }
    }

    #[test]
    fn encode_response_produces_valid_wire_bytes() {
        let mut response = crate::types::HttpResponse::with_status(StatusCode::OK);
        response.body = b"hello".to_vec().into();
        let bytes = encode_response(&response, false);
        let text = std::str::from_utf8(&bytes).expect("utf8");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 5\r\n"));
        assert!(text.ends_with("\r\nhello"));
    }

    #[test]
    fn encode_response_emits_connection_close_when_requested() {
        let response = crate::types::HttpResponse::with_status(StatusCode::OK);
        let bytes = encode_response(&response, true);
        let text = std::str::from_utf8(&bytes).expect("utf8");
        assert!(text.contains("Connection: close\r\n"));
    }

    #[test]
    fn encode_response_overrides_framing_headers() {
        let mut response = crate::types::HttpResponse::with_status(StatusCode::OK);
        response.body = b"hello".to_vec().into();
        response.headers.insert(
            http::header::CONTENT_LENGTH,
            HeaderValue::from_static("999"),
        );
        response.headers.insert(
            http::header::CONNECTION,
            HeaderValue::from_static("keep-alive"),
        );

        let bytes = encode_response(&response, true);
        let text = std::str::from_utf8(&bytes).expect("utf8");
        assert!(text.contains("Content-Length: 5\r\n"));
        assert!(!text.contains("Content-Length: 999"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(!text.contains("keep-alive"));
    }

    #[test]
    fn encode_request_emits_content_length_and_close() {
        let req = HttpRequest {
            method: Method::GET,
            path: "/x".to_owned(),
            version: Version::HTTP_11,
            headers: HeaderMap::new(),
            body: crate::types::HttpRequestBody::Buffered(Vec::new()),
        };
        let bytes = encode_request(&req);
        let text = std::str::from_utf8(&bytes).expect("utf8");
        assert!(text.starts_with("GET /x HTTP/1.1\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
        assert!(text.contains("Connection: close\r\n"));
    }

    #[test]
    fn encode_keepalive_request_omits_connection_header() {
        // The keepalive caller (pool consumer) gets a wire body with
        // no `Connection: close`; HTTP/1.1 default is keep-alive, so
        // the server holds the socket open for the next request.
        let req = HttpRequest::get("/x").build();
        let bytes = encode_keepalive_request(&req);
        let text = std::str::from_utf8(&bytes).expect("utf8");
        assert!(!text.contains("Connection: close"));
        assert!(text.contains("Content-Length: 0\r\n"));
    }

    #[test]
    fn encode_request_overrides_user_keep_alive() {
        // Caller-supplied `Connection` is always dropped; framing is
        // owned by the encoder. Caller-supplied `Content-Length` is
        // also dropped in favour of the actual body length.
        let req = HttpRequest::get("/x")
            .header("Connection", "keep-alive")
            .header("Content-Length", "999")
            .build();
        let bytes = encode_request(&req);
        let text = std::str::from_utf8(&bytes).expect("utf8");
        assert!(text.contains("Connection: close\r\n"));
        assert!(!text.contains("keep-alive"));
        // Content-Length must reflect the actual body length, not the
        // user-supplied value.
        assert!(text.contains("Content-Length: 0\r\n"));
        assert!(!text.contains("Content-Length: 999"));
    }

    #[test]
    fn encode_request_keeps_caller_supplied_headers() {
        let req = HttpRequest::post("/echo")
            .header("Host", "example")
            .body(b"hello".to_vec())
            .build();
        let bytes = encode_request(&req);
        let text = std::str::from_utf8(&bytes).expect("utf8");
        // `http` crate canonicalises header names to lowercase via
        // `HeaderName::as_str()`, mirroring `encode_response`. Wire format
        // is case-insensitive per RFC 7230 §3.2.
        assert!(text.contains("host: example\r\n"));
        assert!(text.contains("Content-Length: 5\r\n"));
        assert!(text.ends_with("\r\nhello"));
    }

    #[test]
    fn parses_simple_response() {
        let buf = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        match parse_response_head(buf, &limits()) {
            ResponseParseProgress::Complete { head, head_len } => {
                assert_eq!(head.status, StatusCode::OK);
                assert_eq!(head.version, Version::HTTP_11);
                assert_eq!(head.reason, "OK");
                assert_eq!(head.content_length, 5);
                assert_eq!(&buf[head_len..], b"hello");
            }
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn response_header_limit_counts_head_not_already_read_body() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\n";
        let mut buf = head.to_vec();
        buf.extend(vec![b'a'; 64]);
        let limits = HttpLimits {
            max_header_bytes: head.len(),
            max_body_bytes: 128,
            ..HttpLimits::default()
        };
        match parse_response_head(&buf, &limits) {
            ResponseParseProgress::Complete {
                head: parsed,
                head_len,
            } => {
                assert_eq!(head_len, head.len());
                assert_eq!(parsed.content_length, 64);
            }
            other => panic!("expected complete despite body bytes, got {other:?}"),
        }
    }

    #[test]
    fn response_partial_yields_need_more() {
        let buf = b"HTTP/1.1 200 OK\r\nContent-Len";
        match parse_response_head(buf, &limits()) {
            ResponseParseProgress::NeedMore => {}
            other => panic!("expected NeedMore, got {other:?}"),
        }
    }

    #[test]
    fn response_chunked_transfer_encoding_accepted() {
        let buf = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        match parse_response_head(buf, &limits()) {
            ResponseParseProgress::Complete { head, head_len } => {
                assert!(head.chunked);
                assert_eq!(head.content_length, 0);
                assert_eq!(head_len, buf.len());
            }
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn response_chunked_transfer_encoding_accepts_token_list() {
        let buf = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: identity, chunked \r\n\r\n";
        match parse_response_head(buf, &limits()) {
            ResponseParseProgress::Complete { head, .. } => assert!(head.chunked),
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn response_unsupported_transfer_encoding_token_rejected() {
        let buf = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n";
        match parse_response_head(buf, &limits()) {
            ResponseParseProgress::Failed(ResponseParseError::UnsupportedTransferEncoding) => {}
            other => panic!("expected UnsupportedTransferEncoding, got {other:?}"),
        }
    }

    #[test]
    fn response_content_length_plus_transfer_encoding_rejected() {
        let buf = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n";
        match parse_response_head(buf, &limits()) {
            ResponseParseProgress::Failed(ResponseParseError::InvalidContentLength) => {}
            other => panic!("expected InvalidContentLength, got {other:?}"),
        }
    }

    #[test]
    fn response_missing_content_length_rejected_for_200() {
        let buf = b"HTTP/1.1 200 OK\r\nServer: x\r\n\r\n";
        match parse_response_head(buf, &limits()) {
            ResponseParseProgress::Failed(ResponseParseError::MissingContentLength) => {}
            other => panic!("expected MissingContentLength, got {other:?}"),
        }
    }

    #[test]
    fn response_204_no_content_length_required() {
        let buf = b"HTTP/1.1 204 No Content\r\nServer: x\r\n\r\n";
        match parse_response_head(buf, &limits()) {
            ResponseParseProgress::Complete { head, .. } => {
                assert_eq!(head.status, StatusCode::NO_CONTENT);
                assert_eq!(head.content_length, 0);
            }
            other => panic!("expected complete (204 needs no Content-Length), got {other:?}"),
        }
    }

    #[test]
    fn response_invalid_content_length_returns_typed_error() {
        let buf = b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\n";
        match parse_response_head(buf, &limits()) {
            ResponseParseProgress::Failed(ResponseParseError::InvalidContentLength) => {}
            other => panic!("expected InvalidContentLength, got {other:?}"),
        }
    }

    #[test]
    fn response_oversized_body_via_content_length_rejected() {
        let huge = HttpLimits::default().max_body_bytes + 1;
        let buf = format!("HTTP/1.1 200 OK\r\nContent-Length: {huge}\r\n\r\n");
        match parse_response_head(buf.as_bytes(), &limits()) {
            ResponseParseProgress::Failed(ResponseParseError::BodyTooLarge) => {}
            other => panic!("expected BodyTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn response_oversized_header_section_rejected() {
        let huge_value = "x".repeat(HttpLimits::default().max_header_bytes);
        let buf = format!("HTTP/1.1 200 OK\r\nX-Huge: {huge_value}\r\n\r\n");
        match parse_response_head(buf.as_bytes(), &limits()) {
            ResponseParseProgress::Failed(ResponseParseError::HeadersTooLarge) => {}
            other => panic!("expected HeadersTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn response_bad_status_line_rejected() {
        let buf = b"NOT-HTTP/garbage\r\n\r\n";
        match parse_response_head(buf, &limits()) {
            ResponseParseProgress::Failed(ResponseParseError::BadStatusLine) => {}
            other => panic!("expected BadStatusLine, got {other:?}"),
        }
    }
}
