//! Native HTTP/1.1 service stack for tina-rs (first form, server side).
//!
//! Phase 048a: Tina speaks HTTP without a Tokio edge.
//!
//! See `.intent/phases/048-native-http-service-stack/plan.md` for scope.
//!
//! # First form contract
//!
//! - HTTP/1.1 only. No HTTP/2, no gRPC, no Tower middleware, no web framework.
//! - `Content-Length` request bodies only. Chunked request bodies, pipelining,
//!   and `Expect: 100-continue` are explicit non-goals in the first form.
//! - User registers an [`HttpListener`] isolate that knows the bind address
//!   and a service-isolate address. The listener spawns one
//!   [`HttpConnection`] isolate per accepted socket. Service isolates handle
//!   typed [`HttpRequest`] messages and reply with [`HttpResponse`].
//! - Backpressure surfaces as runtime-call typed outcomes:
//!   `CallOutcome::{Replied, Full, Closed, Timeout}` map to HTTP status codes
//!   by an explicit policy.
//! - Tina owns the listener, every connection isolate, every read buffer,
//!   the parser invocation, every write, and the close path. No library
//!   that owns the socket, blocks on `Read`/`Write`, spawns threads, owns
//!   a pool, or hides buffers.
//!
//! # First form non-goals
//!
//! - No TLS termination at this layer.
//! - No connection pool primitive in 048a (lives in 048b alongside the
//!   client).
//! - No streaming bodies in 048a (lives in 048c).
//! - No routing helper in 048a (the service isolate handles its own
//!   `match (method, path)`).
//! - No production performance claim.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod parse;
pub mod types;

pub use parse::{ParseProgress, encode_response, parse_request_head};
pub use types::{HttpLimits, HttpRequest, HttpResponse, RequestParseError};

// Re-exports from the `http` crate for convenient `tina_http::Method`,
// `tina_http::StatusCode`, etc., without forcing users to add `http` as a
// direct dependency.
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version, header};
