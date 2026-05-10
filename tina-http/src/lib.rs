//! Native HTTP/1.1 and HTTPS/1.1 for tina-rs. No Tokio edge.
//! `Content-Length` request bodies only — no chunked, no pipelining,
//! no `Expect: 100-continue`.
//!
//! Server: [`HttpListener`] over TCP, [`HttpsListener`] over TLS.
//! HTTPS startup is call-shaped:
//! `call(listener, HttpsListenerMsg::Start, t)` returns
//! `Result<HttpsReady, HttpsStartupError>`.
//!
//! Client: [`HttpClient`] dispatches on [`HttpTarget`]. HTTPS targets
//! carry explicit DER [`TlsTrustRoots`], a server name validated by
//! rustls, and an [`HttpHostPolicy`] for the wire `Host:` header.
//!
//! Errors: HTTPS-aware paths produce
//! [`HttpClientError::Transport { phase, source }`] carrying typed
//! `CallError` reasons (`TlsName`, `TlsCertificate`, `TlsHandshake`,
//! `TlsFull`, `TlsClosed`, `Timeout`, `Io`). Plain TCP keeps the flat
//! `Connect`/`Read`/`Write`/`Closed`/`Timeout` variants.
//!
//! TLS state machine is `rustls`, driven by the runtime's
//! single-worker TLS lane. First form does not target high HTTPS
//! concurrency — see [`HttpsServerConfig`] for the lane-yield
//! trade-off.
//!
//! Out of scope: HTTP/2, ALPN, ACME, mTLS, SNI routing, system roots,
//! certificate reload, redirects, cookies. For mature outbound
//! web-client behaviour use the `tina-reqwest-bridge` crate.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod client;
pub mod connection;
pub mod keepalive;
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
    HttpResponseHead, ParseProgress, ResponseParseProgress, encode_keepalive_request,
    encode_request, encode_response, parse_request_head, parse_response_head,
};
pub use keepalive::{
    KeepaliveConnAddr, KeepaliveConnection, KeepaliveConnectionMsg, KeepaliveOutcome,
    KeepalivePoolHandles, OriginKey, build_keepalive_pool,
};
pub use pool::{HttpConnectionPool, HttpPoolMsg};
pub use request_builder::RequestBuilder;
pub use router::{RouteHandler, Router, StatefulHandler, StatefulRouter};
pub use streaming::{
    RequestChunkReply, RequestStream, ResponseChunkMsg, ResponseChunkReply, ResponseStream,
};
pub use target::{HttpHostPolicy, HttpTarget, TlsTrustRoots};
pub use transport::HttpTransport;
pub use types::{
    HttpClientConfig, HttpClientError, HttpLimits, HttpRequest, HttpRequestBody, HttpResponse,
    HttpResponseBody, HttpServerConfig, HttpTransportPhase, PoolConfig, RequestParseError,
    ResponseParseError,
};

// Re-exports from the `http` crate for convenient `tina_http::Method`,
// `tina_http::StatusCode`, etc., without forcing users to add `http` as a
// direct dependency.
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version, header};
