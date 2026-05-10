//! Native HTTP/1.1 service stack for tina-rs, with native HTTPS/1.1.
//!
//! Tina speaks HTTP and HTTPS without a Tokio edge. HTTP/1.1 only.
//! `Content-Length` request bodies only — no chunked request bodies,
//! no pipelining, no `Expect: 100-continue`.
//!
//! Plain HTTP:
//!
//! - User registers an [`HttpListener`] with a service-isolate address.
//!   The listener spawns one [`HttpConnection`] per accepted socket.
//! - Outbound calls go through [`HttpClient`] over an
//!   [`HttpTarget::Http`] socket address.
//!
//! Native HTTPS:
//!
//! - User registers an [`HttpsListener`] with an explicit
//!   [`TlsServerIdentity`] (DER cert chain + PKCS#8 key). Startup is
//!   call-shaped: `call(listener, HttpsListenerMsg::Start, t)` returns
//!   `Result<HttpsReady { local_addr }, HttpsStartupError>`. Successful
//!   accepts spawn an `HttpConnection` over an
//!   [`HttpTransport::Tls`] stream — same parser, same service-call
//!   shape, different transport.
//! - Outbound HTTPS goes through [`HttpClient`] over an
//!   [`HttpTarget::Https`] target carrying an explicit
//!   [`TlsTrustRoots`] DER root set, a server name verified by rustls
//!   during the handshake, and an [`HttpHostPolicy`] that decides the
//!   request `Host:` header.
//!
//! TLS errors stay TLS errors. Inbound the runtime's `CallError`
//! TLS variants surface unchanged. Outbound,
//! [`HttpClientError::Transport`] carries the typed
//! `HttpTransportPhase` plus the underlying [`tina_runtime::CallError`]
//! (`TlsName`, `TlsCertificate`, `TlsHandshake`, `TlsFull`,
//! `TlsClosed`, `Timeout`, `Io`, ...). Plain TCP failures keep their
//! flat `Connect` / `Write` / `Read` / `Closed` / `Timeout` variants
//! for source-compat.
//!
//! Backpressure surfaces as typed `CallOutcome::{Replied, Full, Closed,
//! Timeout}`, mapped to HTTP status codes by an explicit policy.
//!
//! Tina owns the listener, every connection isolate, every read buffer,
//! the parser invocation, every write, and the close path. No library
//! owns the socket, blocks on `Read`/`Write`, spawns threads, or hides
//! buffers. The TLS state machine is `rustls`, driven by the runtime's
//! single-worker TLS lane.
//!
//! Out of scope here: HTTP/2, gRPC, ALPN, ACME, certificate reload,
//! mTLS, SNI routing, system root certificates, proxies, redirects,
//! cookies, chunked transfer, and any production-performance claim.
//! For mature outbound web-client behaviour, the
//! [`tina-reqwest-bridge`] crate remains the explicit escape hatch.
//!
//! [`tina-reqwest-bridge`]: https://docs.rs/tina-reqwest-bridge/

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod client;
pub mod connection;
pub mod listener;
pub mod listener_tls;
pub mod parse;
pub mod pool;
pub mod request_builder;
pub mod router;
pub mod streaming;
pub mod target;
pub mod transport;
pub mod types;

pub use client::{HttpClient, HttpClientMsg, OutboundCall};
pub use connection::{HttpConnection, HttpConnectionMsg, response_for_call_outcome};
pub use listener::{HttpListener, HttpListenerMsg};
pub use listener_tls::{
    HttpsListener, HttpsListenerMsg, HttpsReady, HttpsServerConfig, HttpsStartupError,
    TlsServerIdentity,
};
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
pub use target::{HttpHostPolicy, HttpTarget, TlsTrustRoots};
pub use transport::{HttpListenerTransport, HttpTransport};
pub use types::{
    HttpClientConfig, HttpClientError, HttpLimits, HttpRequest, HttpRequestBody, HttpResponse,
    HttpResponseBody, HttpServerConfig, HttpTransportPhase, PoolConfig, RequestParseError,
    ResponseParseError,
};

// Re-exports from the `http` crate for convenient `tina_http::Method`,
// `tina_http::StatusCode`, etc., without forcing users to add `http` as a
// direct dependency.
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version, header};
