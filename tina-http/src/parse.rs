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

use http::{HeaderMap, HeaderName, HeaderValue, Method, Version};

use crate::types::{HttpLimits, HttpRequest, RequestParseError};

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
            body,
        }
    }
}

/// Attempts to parse an HTTP/1.x request head out of `buffer`.
///
/// This function is pure and does not consume `buffer`. The connection
/// isolate calls it after each `tcp_read`; on `NeedMore`, it reads more
/// bytes; on `Complete`, it slices the body off `buffer[head_len..]`.
pub fn parse_request_head(buffer: &[u8], limits: &HttpLimits) -> ParseProgress {
    if buffer.len() > limits.max_header_bytes {
        return ParseProgress::Failed(RequestParseError::HeadersTooLarge);
    }

    let mut headers = vec![httparse::EMPTY_HEADER; limits.max_headers];
    let mut req = httparse::Request::new(&mut headers);

    match req.parse(buffer) {
        Ok(httparse::Status::Partial) => {
            if buffer.len() == limits.max_header_bytes {
                ParseProgress::Failed(RequestParseError::HeadersTooLarge)
            } else {
                ParseProgress::NeedMore
            }
        }
        Ok(httparse::Status::Complete(head_len)) => {
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
    let mut transfer_encoding_invalid = false;
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
            let parsed_len: usize = value_str
                .trim()
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
            // First form: only `identity` is permitted; anything else
            // (including `chunked`) is rejected as unsupported.
            let value_str = std::str::from_utf8(header.value).unwrap_or("");
            if !value_str.eq_ignore_ascii_case("identity") {
                transfer_encoding_invalid = true;
            }
        } else if name == http::header::CONNECTION {
            let value_str = std::str::from_utf8(header.value).unwrap_or("");
            for token in value_str.split(',') {
                let token = token.trim();
                if token.eq_ignore_ascii_case("close") {
                    connection_close = true;
                } else if token.eq_ignore_ascii_case("keep-alive") {
                    connection_close = false;
                }
            }
        }

        headers.append(name, value);
    }

    if transfer_encoding_invalid {
        return Err(RequestParseError::UnsupportedTransferEncoding);
    }

    Ok(HttpRequestHead {
        method,
        path,
        version,
        headers,
        content_length: content_length.unwrap_or(0),
        connection_close,
    })
}

fn is_origin_form(target: &str) -> bool {
    // Origin-form is `/...` or `*` (asterisk-form for OPTIONS — we reject
    // it for first form). Authority-form (`example.com:80`) and
    // absolute-form (`http://example.com/`) are both rejected.
    target.starts_with('/')
}

/// Serialises a response onto a wire buffer using HTTP/1.1 framing.
///
/// First form: always emits `Content-Length`. Never emits chunked
/// transfer encoding. Adds `Connection: close` if the request asked for
/// it; otherwise relies on the version default.
pub fn encode_response(response: &crate::types::HttpResponse, connection_close: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(128 + response.body.len());

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

    let mut wrote_content_length = false;
    let mut wrote_connection = false;
    for (name, value) in response.headers.iter() {
        if name == http::header::CONTENT_LENGTH {
            wrote_content_length = true;
        } else if name == http::header::CONNECTION {
            wrote_connection = true;
        }
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !wrote_content_length {
        out.extend_from_slice(b"Content-Length: ");
        out.extend_from_slice(response.body.len().to_string().as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if connection_close && !wrote_connection {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&response.body);
    out
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
    fn signals_need_more_on_partial_head() {
        let buf = b"GET /counter HTTP/1.1\r\nHost: loc";
        match parse_request_head(buf, &limits()) {
            ParseProgress::NeedMore => {}
            other => panic!("expected NeedMore, got {other:?}"),
        }
    }

    #[test]
    fn rejects_chunked_request_body_in_first_form() {
        let buf = b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n";
        match parse_request_head(buf, &limits()) {
            ParseProgress::Failed(RequestParseError::UnsupportedTransferEncoding) => {}
            other => panic!("expected UnsupportedTransferEncoding, got {other:?}"),
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
    fn encode_response_produces_valid_wire_bytes() {
        let mut response = crate::types::HttpResponse::with_status(StatusCode::OK);
        response.body = b"hello".to_vec();
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
}
