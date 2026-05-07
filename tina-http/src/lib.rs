//! Native HTTP/1.1 service stack for tina-rs.
//!
//! Tina speaks HTTP without a Tokio edge. HTTP/1.1 only. `Content-Length`
//! request bodies only — no chunked request bodies, no pipelining, no
//! `Expect: 100-continue`.
//!
//! User registers an [`HttpListener`] with a service-isolate address.
//! The listener spawns one [`HttpConnection`] per accepted socket.
//! Services receive [`HttpRequest`] and reply [`HttpResponse`].
//!
//! Backpressure surfaces as typed `CallOutcome::{Replied, Full, Closed,
//! Timeout}`, mapped to HTTP status codes by an explicit policy.
//!
//! Tina owns the listener, every connection isolate, every read buffer,
//! the parser invocation, every write, and the close path. No library
//! owns the socket, blocks on `Read`/`Write`, spawns threads, or hides
//! buffers.
//!
//! Out of scope here: TLS and any production-performance claim.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod client;
pub mod connection;
pub mod listener;
pub mod parse;
pub mod pool;
pub mod request_builder;
pub mod router;
pub mod streaming;
pub mod types;

pub use client::{HttpClient, HttpClientMsg, OutboundCall};
pub use connection::{HttpConnection, HttpConnectionMsg, response_for_call_outcome};
pub use listener::{HttpListener, HttpListenerMsg};
pub use parse::{
    HttpResponseHead, ParseProgress, ResponseParseProgress, encode_request, encode_response,
    parse_request_head, parse_response_head,
};
pub use pool::{HttpConnectionPool, HttpPoolMsg};
pub use request_builder::RequestBuilder;
pub use router::{RouteHandler, Router, StatefulHandler, StatefulRouter};
pub use streaming::{
    RequestChunkReply, RequestStream, ResponseChunkMsg, ResponseChunkReply, ResponseStream,
};
pub use types::{
    HttpClientConfig, HttpClientError, HttpLimits, HttpRequest, HttpRequestBody, HttpResponse,
    HttpResponseBody, HttpServerConfig, PoolConfig, RequestParseError, ResponseParseError,
};

// Re-exports from the `http` crate for convenient `tina_http::Method`,
// `tina_http::StatusCode`, etc., without forcing users to add `http` as a
// direct dependency.
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version, header};
