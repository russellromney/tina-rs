//! Native HTTP/1.1, HTTPS/1.1, and a first-form cleartext HTTP/2
//! server for tina-rs. No Tokio edge. No HTTP/1 pipelining, no
//! `Expect: 100-continue`.
//!
//! # Body model
//!
//! Bodies are either **buffered** (whole `Vec<u8>` resident in the
//! connection isolate) or **streamed** (pulled chunk-by-chunk from
//! a chunk-source isolate).
//!
//! ## Request bodies
//!
//! `Content-Length` or `Transfer-Encoding: chunked`. Streaming
//! activates when [`HttpLimits::inbound_stream_chunk_size`] is
//! `Some(N)` and the request declares a non-zero `Content-Length`
//! or chunked encoding. Dispatch happens as soon as the head
//! parses; the service pulls body chunks via
//! `call(stream.source, HttpConnectionMsg::body_next(), timeout)`.
//! Clean `Eof` and truncated [`RequestChunkReply::Error`] are
//! distinct. Chunked request bodies require streaming to be
//! enabled; when disabled they are rejected as
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
//! body IO/timeout error counts. `BodyMetrics::with_body_capacity`
//! additionally enables two weighted capacity surfaces
//! (`request` and `response` body bytes) charged to one
//! shard-local shared scope.
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
//! HTTP/2: [`Http2Listener`] is a prior-knowledge cleartext h2c
//! server first form. It owns the TCP stream, frame parsing, bounded
//! stream table, connection/stream flow-control windows, reset/close
//! outcomes, and service dispatch to the same [`HttpRequest`] /
//! [`HttpResponse`] shape where honest. Request and response bodies are
//! buffered under explicit byte caps for ordinary unary services.
//! gRPC requests can be exposed through an HTTP/2 pull source, and
//! responses can stream from Tina chunk sources with DATA flow-control
//! and real trailers. It is not an HTTPS/2 or ALPN claim and not a full
//! RFC feature clone.
//!
//! HTTP/2 client: [`Http2ClientConnection`] is a native client isolate
//! over one stream — cleartext h2c or h2 over TLS — with bounded
//! stream-slot admission, typed [`Http2ClientOutcome`], outbound
//! flow-control pacing, and outbound-direction protocol facts. A
//! [`Http2Target::Tls`] target dials the TLS rail with `h2` ALPN; a
//! server that declines `h2` returns
//! [`Http2ClientOutcome::TlsAlpnMismatch`], distinct from cert/name
//! failures. Request bodies are buffered ([`Http2ClientRequest`]) or
//! streamed from a chunk source ([`Http2ClientStreamingRequest`], the
//! same `IterBodySource` pull protocol the server uses). h2/TLS is
//! request-response (half-duplex) today; full-duplex needs a runtime TLS
//! reactor.
//!
//! gRPC: [`GrpcRouter`] is the server. [`GrpcClient`] is the native
//! client — Tina is a native gRPC client, not only a server. The server
//! layers unary, server-streaming, client-streaming, and bidirectional
//! streaming `prost` messages with typed [`GrpcStatus`] trailers; the
//! client today covers the **unary** path over h2c, decoding into a
//! typed [`GrpcUnaryOutcome`] where a non-OK status is the caller
//! outcome (never hidden in a success) and the received status is a
//! `GrpcFinalStatusReceived` protocol fact. Both reject compression and
//! cap message bytes. Streaming gRPC clients, interceptors, reflection,
//! production pooled clients, and TLS ALPN are later slices.
//!
//! WebSocket: [`websocket_upgrade`] validates server-side HTTP/1.1
//! upgrades for [`HttpListener`] and [`HttpsListener`]. After the
//! `101 Switching Protocols` response, the same connection isolate owns
//! the upgraded TCP/TLS stream. The 087 echo path remains:
//! handle `WebSocketSessionMsg::Text` and reply with
//! `WebSocketSessionOutcome::Text`. Room/fanout code can instead store
//! the `WebSocketSessionHandle` from `WebSocketSessionMsg::SessionOpen`
//! and route bounded sends back through the session owner with
//! `handle.text_effect::<Room>("...", timeout)`. Room/admin code can send
//! `WebSocketSessionMsg::Shutdown` to close stored handles through the same
//! bounded owner path. Upgrade code can inspect
//! `WebSocketUpgradeRequest::offered_subprotocols()` and select a protocol
//! with `accept_subprotocol(...)`. Data fragmentation is reassembled under
//! `max_message_bytes`, with control frames allowed between fragments. There is
//! no permessage-deflate compression or native broad client in this slice;
//! extension offers are visible and ignored unless a future slice explicitly
//! negotiates one.
//!
//! Rooms that fan out to a bounded member set can use
//! [`WebSocketMemberTable`] for admit/broadcast/shutdown bookkeeping. The
//! table owns a capacity-bounded `BTreeMap<WebSocketSessionId,
//! WebSocketSessionHandle>` and folds `WebSocketSessionMsg::SendOutcome`
//! into slow-peer / closed / protocol / timeout eviction with a typed
//! [`SendOutcomeAction`]. Idle-eviction policy, the close timer, and any
//! recurring tick stay in the room isolate.
//!
//! Still out of scope: HTTP/2 TLS ALPN, ACME, mTLS, SNI routing,
//! system roots, certificate reload, redirects, cookies. For mature
//! outbound web-client behaviour use the `tina-reqwest-bridge` crate.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod body_metrics;
pub mod chunked_decoder;
pub mod client;
pub mod connection;
pub mod grpc;
pub mod grpc_client;
pub mod http2;
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
pub mod websocket;
pub mod websocket_room;

pub use body_metrics::{BodyCapacityFull, BodyMetrics, BodyPressureReport};
pub use client::{HttpClient, HttpClientMsg, OutboundCall};
pub use connection::{HttpConnection, HttpConnectionMsg, response_for_call_outcome};
pub use grpc::{
    GrpcClientStreamingRequest, GrpcError, GrpcLimits, GrpcRawStreamingRequest,
    GrpcRawStreamingResponse, GrpcRequest, GrpcRequestStream, GrpcResponse, GrpcRouter,
    GrpcRouterMsg, GrpcServerStreamingResponse, GrpcStatus, GrpcStatusCode, GrpcStreamReply,
    GrpcStreamingCall, GrpcStreamingResponse, decode_streaming_request, decode_unary_request,
    encode_grpc_message, grpc_status_trailers, grpc_stream_finish, grpc_stream_message,
    grpc_unary_call_h2c_blocking,
};
pub use grpc_client::{GrpcClient, GrpcTarget, GrpcUnaryOutcome};
pub use http2::{
    AlpnProtocols, Http2ClientConnection, Http2ClientLimits, Http2ClientMsg, Http2ClientOutcome,
    Http2ClientReply, Http2ClientReport, Http2ClientRequest, Http2ClientResponse,
    Http2ClientStreamingRequest, Http2Connection, Http2ConnectionMsg, Http2ConnectionReply,
    Http2ConnectionReport, Http2Limits, Http2Listener, Http2ListenerMsg, Http2Outcome,
    Http2ProtocolError, Http2ServerConfig, Http2StreamReport, Http2StreamState, Http2Target,
};
pub use keepalive::{
    KeepaliveConnAddr, KeepaliveConnection, KeepaliveConnectionMsg, KeepaliveConnectionStopFailure,
    KeepaliveConnectionStopOutcome, KeepaliveOutcome, KeepalivePoolCloseOutcome,
    KeepalivePoolDrainOutcome, KeepalivePoolHandles, KeepalivePoolShutdownReport, OriginKey,
    build_keepalive_pool, shutdown_keepalive_pool,
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
    Http2RequestStream, IterBodySource, RequestChunkReply, RequestStream, ResponseChunkMsg,
    ResponseChunkReply, ResponseStream,
};
pub use target::{HttpHostPolicy, HttpTarget, TlsTrustRoots};
pub use transport::HttpTransport;
pub use types::{
    HttpClientConfig, HttpClientError, HttpLimits, HttpRequest, HttpRequestBody, HttpResponse,
    HttpResponseBody, HttpServerConfig, HttpTransportPhase, PoolConfig, RequestParseError,
    ResponseParseError,
};
pub use websocket::{
    WebSocketAccept, WebSocketCloseCode, WebSocketError, WebSocketLimits, WebSocketMessage,
    WebSocketOutboundQueue, WebSocketReportRequest, WebSocketSend, WebSocketSendError,
    WebSocketSendOutcome, WebSocketSessionHandle, WebSocketSessionId, WebSocketSessionMsg,
    WebSocketSessionOutcome, WebSocketSessionReport, WebSocketSessionReportOutcome,
    WebSocketUpgradeRequest, websocket_upgrade,
};
pub use websocket_room::{
    AdmitOutcome, SendOutcomeAction, WebSocketMemberTable, WebSocketMemberTableReport,
};

// Re-exports from the `http` crate for convenient `tina_http::Method`,
// `tina_http::StatusCode`, etc., without forcing users to add `http` as a
// direct dependency.
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version, header};
