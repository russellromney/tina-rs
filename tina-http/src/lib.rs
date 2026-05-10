//! Native HTTP/1.1 and HTTPS/1.1 for tina-rs. No Tokio edge. No
//! pipelining, no `Expect: 100-continue`.
//!
//! # Body model
//!
//! Bodies are either **buffered** (whole `Vec<u8>` resident in the
//! connection isolate) or **streamed** (pulled chunk-by-chunk from
//! a chunk-source isolate).
//!
//! ## Request bodies
//!
//! `Content-Length` framing only. Streaming activates when
//! [`HttpLimits::inbound_stream_chunk_size`] is `Some(N)` and the
//! request declares a non-zero `Content-Length`. Dispatch happens
//! as soon as the head parses; the service pulls body chunks via
//! `call(stream.source, HttpConnectionMsg::body_next(), timeout)`.
//! Clean `Eof` and truncated [`RequestChunkReply::Error`] are
//! distinct. Chunked request bodies are rejected as
//! [`RequestParseError::UnsupportedTransferEncoding`].
//!
//! ## Response bodies
//!
//! Two framings, picked at the call site:
//!
//! - [`HttpResponse::stream_known_length`] —
//!   `Content-Length: N`. Source must deliver exactly N bytes.
//! - [`HttpResponse::stream_chunked`] —
//!   `Transfer-Encoding: chunked`. No length declared up front;
//!   the connection writes the chunked terminator on source `Eof`.
//!
//! There is no "guess a length" path. If you don't know the
//! length, you say so.
//!
//! For the common iterator-style source, [`IterBodySource`] wraps
//! any `Iterator<Item = Vec<u8>> + Send + 'static` into a chunk
//! source — no custom `Isolate` impl needed.
//!
//! # Body pressure
//!
//! Attach a [`BodyMetrics`] to either listener via
//! `with_metrics(metrics.clone())` to record request/response
//! bytes resident, peak high-water, body-cap full counts, and
//! body IO/timeout error counts.
//! [`BodyPressureReport::drained`] is the "no-leak" terminal
//! assertion. A shared capacity-report shape lives in a separate
//! slice; this report folds into it when that ships.
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

pub mod body_metrics;
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

pub use body_metrics::{BodyMetrics, BodyPressureReport};
pub use client::{HttpClient, HttpClientMsg, OutboundCall};
pub use connection::{HttpConnection, HttpConnectionMsg, response_for_call_outcome};
pub use keepalive::{
    KeepaliveConnAddr, KeepaliveConnection, KeepaliveConnectionMsg, KeepaliveOutcome,
    KeepalivePoolHandles, OriginKey, build_keepalive_pool,
};
pub use listener::{HttpListener, HttpListenerMsg};
pub use listener_tls::{
    HttpsListener, HttpsListenerMsg, HttpsReady, HttpsServerConfig, HttpsStartupError,
    TlsServerIdentity,
};
pub use parse::{
    HttpResponseHead, ParseProgress, ResponseParseProgress, encode_keepalive_request,
    encode_request, encode_response, parse_request_head, parse_response_head,
};
pub use pool::{HttpConnectionPool, HttpPoolMsg};
pub use request_builder::RequestBuilder;
pub use router::{RouteHandler, Router, StatefulHandler, StatefulRouter};
// `ResponseStream` and `ChunkedResponseStream` exist as types
// behind the `HttpResponseBody` variants, but callers never name
// them: the loud-API constructors `HttpResponse::stream_known_length`
// and `HttpResponse::stream_chunked` build them directly from a
// `(length, source)` or `source` argument. `ResponseStream` stays
// re-exported because the older `HttpResponse::with_stream`
// constructor takes one by value; `ChunkedResponseStream` does not
// need a public name since there is no `with_chunked_stream`
// equivalent — `stream_chunked` is the only constructor.
pub use streaming::{
    IterBodySource, RequestChunkReply, RequestStream, ResponseChunkMsg, ResponseChunkReply,
    ResponseStream,
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
