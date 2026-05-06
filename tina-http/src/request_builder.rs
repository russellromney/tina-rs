//! Small builder for client-side [`HttpRequest`] construction.
//!
//! Method, path, headers, body. No query helpers, no JSON, no URL
//! parsing — those belong above this layer.
//!
//! ```rust,no_run
//! use tina_http::HttpRequest;
//! let req = HttpRequest::get("/counter")
//!     .header("Host", "example.com")
//!     .build();
//! ```

use http::{HeaderMap, HeaderName, HeaderValue, Method, Version};

use crate::types::HttpRequest;

impl HttpRequest {
    /// Starts a `GET` request builder for the given origin-form path.
    pub fn get(path: impl Into<String>) -> RequestBuilder {
        RequestBuilder::new(Method::GET, path.into())
    }

    /// Starts a `POST` request builder for the given origin-form path.
    pub fn post(path: impl Into<String>) -> RequestBuilder {
        RequestBuilder::new(Method::POST, path.into())
    }

    /// Starts a builder with an arbitrary method.
    pub fn builder(method: Method, path: impl Into<String>) -> RequestBuilder {
        RequestBuilder::new(method, path.into())
    }
}

/// Fluent builder for an [`HttpRequest`].
///
/// Consumes itself on each call so chaining is one expression. Build with
/// [`RequestBuilder::build`].
#[derive(Debug)]
pub struct RequestBuilder {
    method: Method,
    path: String,
    version: Version,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl RequestBuilder {
    fn new(method: Method, path: String) -> Self {
        Self {
            method,
            path,
            version: Version::HTTP_11,
            headers: HeaderMap::new(),
            body: Vec::new(),
        }
    }

    /// Sets the HTTP wire version. Defaults to `HTTP/1.1`.
    pub fn version(mut self, version: Version) -> Self {
        self.version = version;
        self
    }

    /// Appends a header. Panics on invalid name/value. Use
    /// [`RequestBuilder::try_header`] for fallible insertion.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        let name = HeaderName::from_bytes(name.as_bytes()).expect("valid header name");
        let value = HeaderValue::from_str(value).expect("valid header value");
        self.headers.append(name, value);
        self
    }

    /// Appends a header, returning the builder unchanged on invalid input.
    /// Useful when names/values come from untrusted sources.
    pub fn try_header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            self.headers.append(name, value);
        }
        self
    }

    /// Sets the body bytes. The encoder fills in `Content-Length`
    /// automatically based on the byte length.
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// Sets the body as a UTF-8 string with `Content-Type: text/plain`.
    pub fn text_body(mut self, body: impl Into<String>) -> Self {
        let body = body.into().into_bytes();
        self.body = body;
        self.headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        );
        self
    }

    /// Finalises the builder into an [`HttpRequest`] ready to hand to
    /// `HttpClient::request(...)`.
    pub fn build(self) -> HttpRequest {
        HttpRequest {
            method: self.method,
            path: self.path,
            version: self.version,
            headers: self.headers,
            body: crate::types::HttpRequestBody::Buffered(self.body),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_simple_get() {
        let req = HttpRequest::get("/counter").header("Host", "x").build();
        assert_eq!(req.method, Method::GET);
        assert_eq!(req.path, "/counter");
        assert_eq!(req.version, Version::HTTP_11);
        assert_eq!(req.body.declared_length(), 0);
        assert_eq!(
            req.headers.get("host").map(|v| v.as_bytes()),
            Some(b"x" as &[u8])
        );
    }

    #[test]
    fn builds_post_with_body() {
        let req = HttpRequest::post("/echo").body(b"hello".to_vec()).build();
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.body, b"hello");
    }

    #[test]
    fn text_body_sets_content_type() {
        let req = HttpRequest::post("/x").text_body("hello").build();
        assert_eq!(
            req.headers
                .get(http::header::CONTENT_TYPE)
                .map(|v| v.as_bytes()),
            Some(b"text/plain" as &[u8]),
        );
    }

    #[test]
    fn try_header_swallows_invalid() {
        // \x7f is not a legal header value byte.
        let req = HttpRequest::get("/").try_header("X-Bad", "\x7f").build();
        assert!(req.headers.is_empty());
    }
}
