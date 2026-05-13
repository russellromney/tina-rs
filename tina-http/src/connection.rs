//! Per-connection isolate for the native HTTP/1.1 server.
//!
//! One [`HttpConnection`] isolate owns one TCP stream. It reads bytes,
//! parses one request head, accumulates the body up to `Content-Length`,
//! calls the service isolate via `tina_runtime::call`, serialises the
//! response, and writes it.
//!
//! # Keep-alive
//!
//! Set [`crate::HttpLimits::keepalive_idle_timeout`] to a `Some(d)` to
//! keep the connection open after each response and serve the next
//! request on the same stream. Per-request close intent is still
//! honored:
//!
//! - the request was HTTP/1.0 (default close);
//! - the request carried `Connection: close`;
//! - any parse / service error closes immediately;
//! - peer EOF between requests closes cleanly;
//! - the idle timer expires before the next request's head completes.
//!
//! When `keepalive_idle_timeout` is `None` (the default), the
//! connection serves one request per accept and closes — the legacy
//! first-form behaviour.
//!
//! Pipelining is not supported: any bytes that arrive after a
//! request's body but before its response is written are reset between
//! iterations and effectively dropped. A well-behaved HTTP/1.1 client
//! waits for each response before sending the next request.
//!
//! Backpressure mapping at the service boundary:
//!
//! | Service `CallOutcome`            | Wire response                |
//! |----------------------------------|------------------------------|
//! | `Replied(HttpResponse)`          | The response itself          |
//! | `Full`                           | `503 Service Unavailable`    |
//! | `Closed`                         | `500 Internal Server Error`  |
//! | `Timeout`                        | `504 Gateway Timeout`        |
//!
//! Parser failures map per [`crate::types::RequestParseError::status`].

use std::time::Duration;

use http::StatusCode;
use tina::prelude::*;
use tina::{CallContext, RequestContext, reply_to_request};
use tina_runtime::{
    CallError, CallOutcome, call, sleep, tcp_close_stream, tcp_read, tcp_write, tls_close,
    tls_read, tls_write,
};

use crate::body_metrics::BodyMetrics;
use crate::parse::{HttpRequestHead, ParseProgress, encode_response_head, parse_request_head};
use crate::streaming::{
    RequestChunkReply, RequestStream, ResponseChunkMsg, ResponseChunkReply, ResponseStream,
};
use crate::transport::HttpTransport;
use crate::types::{HttpLimits, HttpRequest, HttpResponse, HttpResponseBody, RequestParseError};

/// Bytes the connection isolate asks for per `tcp_read`. Bounded so a
/// single read does not pull more than this into the runtime, regardless
/// of what the kernel has buffered.
const READ_CHUNK: usize = 4096;

/// Inbound message variants for [`HttpConnection`].
///
/// External code typically only sends [`HttpConnectionMsg::Begin`] once;
/// every other variant is a runtime-call continuation produced by the
/// connection itself.
#[derive(Debug, Clone)]
pub enum HttpConnectionMsg {
    /// Kick off the read loop. Sent once by the listener after spawn.
    Begin,
    /// `tcp_read` reply.
    Read(Result<Vec<u8>, CallError>),
    /// Per-iteration head/idle deadline. Carries the request
    /// generation it was scheduled for so that a stale deadline from
    /// a previous keepalive iteration is recognised and dropped.
    ///
    /// On the first iteration, this is the slow-loris guard armed
    /// from [`HttpLimits::header_read_timeout`]. On subsequent
    /// iterations (when keepalive is on), it is also the idle guard
    /// armed from [`HttpLimits::keepalive_idle_timeout`]. If the
    /// next request's head does not parse before the deadline fires,
    /// the connection stops and runtime cleanup closes the stream.
    HeaderDeadline {
        generation: u64,
        result: Result<(), CallError>,
    },
    /// Service `call` reply.
    ServiceReturned(CallOutcome<HttpResponse>),
    /// `tcp_write` reply.
    Wrote(Result<usize, CallError>),
    /// `tcp_close_stream` reply.
    Closed(Result<(), CallError>),
    /// Streaming response: chunk source's reply to a pulled `Next`.
    StreamChunk(CallOutcome<ResponseChunkReply>),
    /// Streaming response: reply from the `Cancel` call sent to the
    /// source when the wire is abandoned. The connection only
    /// needed to fire the message; the reply (or timeout/closed)
    /// is not actionable.
    StreamSourceCancelDone(CallOutcome<ResponseChunkReply>),
    /// Streaming request: service asks the connection for the next
    /// chunk of the inbound body. Replies with [`RequestChunkReply`].
    RequestBodyNext,
    /// Streaming request: continuation from a `tcp_read` issued while
    /// serving a `RequestBodyNext` call whose buffer was empty. The
    /// outer call context (the service's `RequestBodyNext` call)
    /// propagates through this continuation, so `Effect::Reply` here
    /// answers the service.
    BodyChunkRead(Result<Vec<u8>, CallError>),
}

impl HttpConnectionMsg {
    /// Convenience: build a `RequestBodyNext` for use at a service
    /// call site without spelling out the variant.
    pub fn body_next() -> Self {
        Self::RequestBodyNext
    }
}

/// Per-connection isolate.
///
/// Generic over the user's `Shard` type and the service's message
/// type `M`. `M` defaults to `HttpRequest` for sync-reply services;
/// multi-turn services declare an enum that wraps `HttpRequest` and
/// supply `From<HttpRequest>`.
pub struct HttpConnection<S: Shard, M: From<HttpRequest> + Send + 'static = HttpRequest> {
    transport: HttpTransport,
    /// Per-call deadline passed to TLS lane reads/writes/closes. Ignored
    /// on the TCP transport (TCP reads/writes have no per-call deadline
    /// today).
    tls_io_timeout: Duration,
    service: Address<M, HttpResponse>,
    limits: HttpLimits,
    service_call_timeout: Duration,
    /// Optional body-pressure counters. When `Some`, the connection
    /// charges inbound/outbound bytes on admission and releases on
    /// drain/drop, and increments full/timeout/IO-error counts on
    /// the appropriate edge transitions. When `None` no metrics are
    /// recorded — zero-overhead default.
    metrics: Option<BodyMetrics>,
    /// Bytes currently charged to `metrics.request_body_*`. Tracks
    /// what we owe the metrics on drop/close.
    metrics_request_charge: usize,
    /// Bytes currently charged to `metrics.response_body_*`.
    metrics_response_charge: usize,

    // Accumulating wire state.
    read_buf: Vec<u8>,
    parsed_head: Option<HttpRequestHead>,
    head_len: usize,

    // Outbound write state. `pending_response` is the bytes still to
    // write; `tcp_write` may accept fewer than we send, in which case
    // `handle_wrote` drains the accepted prefix and we re-issue the
    // remainder. Buffered responses write the head first (no body
    // charge), then promote `pending_buffered_body` into
    // `pending_response` and charge body bytes once the head drains.
    // This keeps the body-pressure counter honest: `current` only
    // reflects body bytes actually queued for the wire.
    pending_response: Vec<u8>,
    pending_buffered_body: Option<Vec<u8>>,

    // Streaming-response state. `Some` once we have written the head of
    // a streamed response and need to keep pulling chunks until `Eof`.
    // For `Content-Length` framing, `stream_bytes_remaining` decrements
    // as chunks are written and the connection closes when it hits
    // zero or the source replies `Eof`. For `Transfer-Encoding:
    // chunked` framing (`stream_chunked = true`), the byte counter is
    // unused and the loop runs until the source replies `Eof`; the
    // connection then writes the `0 CRLF CRLF` terminator and closes.
    stream_source: Option<Address<ResponseChunkMsg, ResponseChunkReply>>,
    stream_bytes_remaining: usize,
    stream_chunked: bool,
    stream_call_timeout: Duration,

    // Inbound streaming state.
    //
    // When the dispatch path chose the streaming variant, the
    // connection lazily pulls body bytes from the socket as the service
    // calls `RequestBodyNext`. Naming convention:
    //
    // - `inbound_total`: declared `Content-Length`.
    // - `inbound_received`: bytes read from the socket so far in the
    //   body region.
    // - `inbound_delivered`: bytes already replied to the service.
    // - `inbound_buffer`: bytes received from the socket but not yet
    //   delivered to the service (received - delivered).
    // - `inbound_chunk_size`: cap on a single chunk reply.
    //
    // Invariant: `inbound_received >= inbound_delivered` and
    // `inbound_received - inbound_delivered == inbound_buffer.len()`.
    inbound_total: usize,
    inbound_received: usize,
    inbound_delivered: usize,
    inbound_buffer: Vec<u8>,
    inbound_chunk_size: usize,

    // Chunked inbound request state.
    // `chunked_decoder` holds the incremental decoder while a chunked
    // request body is being consumed. `chunked_raw_buffer` holds
    // unconsumed raw bytes across `BodyChunkRead` continuations.
    // `inbound_chunked` is true once dispatch chose the chunked path.
    chunked_decoder: Option<crate::chunked_decoder::ChunkedDecoder>,
    chunked_raw_buffer: Vec<u8>,
    inbound_chunked: bool,

    // Captured at the first handler turn (`start()`), used to construct
    // the typed self-address for streaming-body dispatch.
    self_shard_id: Option<tina::ShardId>,
    self_isolate_id: Option<tina::IsolateId>,

    // Whether the connection should close after the current response.
    // Set on parse error, on service-call failure (Full/Closed/Timeout
    // are mapped to a synthetic response and a close), when the
    // request itself asks to close (HTTP/1.0, `Connection: close`),
    // when keepalive is disabled in `HttpLimits`, and on shutdown.
    will_close: bool,

    // Per-iteration deadline tracking.
    //
    // `request_generation` is bumped at the start of every request
    // iteration (initial and each keepalive round). Outstanding
    // `HeaderDeadline { generation }` messages from prior iterations
    // are recognised by generation mismatch and dropped silently.
    // `head_deadline_armed` flips false when the current head
    // parses, so a same-generation deadline that fires after parsing
    // is also a no-op.
    request_generation: u64,
    head_deadline_armed: bool,

    // Latch flipped the first time we reply `Eof` (or `Error`) on
    // the streaming-body chunk path. A buggy service that calls
    // `body_next` after Eof gets the same `Eof` reply without
    // touching the socket, so a single peer FIN cannot be charged
    // as multiple IO errors.
    body_eof_replied: bool,

    // Captured caller for a streaming request-body `body_next()` pull
    // while the connection waits on socket I/O before answering.
    pending_request_body_reply: Option<RequestContext<RequestChunkReply>>,

    _shard: std::marker::PhantomData<S>,
}

impl<S: Shard, M: From<HttpRequest> + Send + 'static> HttpConnection<S, M> {
    /// Builds a new connection isolate over a TCP transport. Convenience
    /// for the plain-HTTP path.
    pub fn new(
        stream: tina_runtime::StreamId,
        service: Address<M, HttpResponse>,
        limits: HttpLimits,
        service_call_timeout: Duration,
    ) -> Self {
        Self::with_transport(
            HttpTransport::Tcp(stream),
            service,
            limits,
            service_call_timeout,
            // TLS-only deadline; ignored on the TCP branch.
            Duration::ZERO,
        )
    }

    /// Builds a new connection isolate over an explicit transport. Used
    /// by `HttpsListener` to wire a TLS stream; the plain HTTP listener
    /// goes through [`HttpConnection::new`].
    pub fn with_transport(
        transport: HttpTransport,
        service: Address<M, HttpResponse>,
        limits: HttpLimits,
        service_call_timeout: Duration,
        tls_io_timeout: Duration,
    ) -> Self {
        Self::with_transport_and_metrics(
            transport,
            service,
            limits,
            service_call_timeout,
            tls_io_timeout,
            None,
        )
    }

    /// Builds a connection isolate that reports body pressure into
    /// the supplied [`BodyMetrics`]. Listeners thread one shared
    /// `BodyMetrics` through every connection they spawn so the
    /// snapshot reflects the whole shard.
    pub fn with_transport_and_metrics(
        transport: HttpTransport,
        service: Address<M, HttpResponse>,
        limits: HttpLimits,
        service_call_timeout: Duration,
        tls_io_timeout: Duration,
        metrics: Option<BodyMetrics>,
    ) -> Self {
        Self {
            transport,
            tls_io_timeout,
            service,
            limits,
            service_call_timeout,
            metrics,
            metrics_request_charge: 0,
            metrics_response_charge: 0,
            read_buf: Vec::new(),
            parsed_head: None,
            head_len: 0,
            pending_response: Vec::new(),
            pending_buffered_body: None,
            stream_source: None,
            stream_bytes_remaining: 0,
            stream_chunked: false,
            stream_call_timeout: service_call_timeout,
            inbound_total: 0,
            inbound_received: 0,
            inbound_delivered: 0,
            inbound_buffer: Vec::new(),
            inbound_chunk_size: 0,
            chunked_decoder: None,
            chunked_raw_buffer: Vec::new(),
            inbound_chunked: false,
            self_shard_id: None,
            self_isolate_id: None,
            will_close: false,
            request_generation: 0,
            head_deadline_armed: true,
            body_eof_replied: false,
            pending_request_body_reply: None,
            _shard: std::marker::PhantomData,
        }
    }
}

// The `#[tina_runtime::isolate]` macro requires a concrete shard type; we
// write the `Isolate` impl by hand so a single `HttpConnection`
// implementation works for any user-chosen shard.
impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> Isolate for HttpConnection<S, M> {
    tina::isolate_types! {
        message: HttpConnectionMsg,
        reply: RequestChunkReply,
        send: tina::Outbound<std::convert::Infallible>,
        spawn: std::convert::Infallible,
        call: tina_runtime::RuntimeCall<HttpConnectionMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: HttpConnectionMsg,
        ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        // Capture self-identity once. Used by the streaming-request
        // dispatch path to hand the service a typed self-address.
        if self.self_isolate_id.is_none() {
            self.self_shard_id = Some(ctx.shard_id());
            self.self_isolate_id = Some(ctx.isolate_id());
        }
        match msg {
            HttpConnectionMsg::Begin => self.start(),

            HttpConnectionMsg::Read(Ok(bytes)) => self.handle_bytes_read(bytes),
            HttpConnectionMsg::Read(Err(_)) => {
                // Mid-body buffered read failure (head parsed, body
                // not yet complete). Distinct from a clean post-body
                // close; record so the metric reflects the truncation.
                if self.parsed_head.is_some() {
                    self.record_body_io_error();
                }
                let cancel = self.cancel_stream_source();
                let close = self.begin_close();
                match cancel {
                    Some(c) => batch(vec![c, close]),
                    None => close,
                }
            }

            HttpConnectionMsg::HeaderDeadline { generation, .. } => {
                self.handle_header_deadline(generation)
            }

            HttpConnectionMsg::ServiceReturned(outcome) => self.handle_service_outcome(outcome),

            HttpConnectionMsg::Wrote(Ok(count)) => self.handle_wrote(count),
            HttpConnectionMsg::Wrote(Err(_)) => {
                // Wire write failed while a body was still owed —
                // either still queued in `pending_response`, sitting
                // in `pending_buffered_body`, or being streamed.
                // Record the truncation so the metric is honest.
                if self.has_pending_body() {
                    self.record_body_io_error();
                }
                let cancel = self.cancel_stream_source();
                let close = self.begin_close();
                match cancel {
                    Some(c) => batch(vec![c, close]),
                    None => close,
                }
            }

            HttpConnectionMsg::StreamChunk(outcome) => self.handle_stream_chunk(outcome),
            HttpConnectionMsg::StreamSourceCancelDone(_) => noop(),

            HttpConnectionMsg::RequestBodyNext => self.handle_request_body_next(),

            HttpConnectionMsg::BodyChunkRead(result) => self.handle_body_chunk_read(result),

            HttpConnectionMsg::Closed(_) => stop(),
        }
    }

    fn handle_call(&mut self, msg: HttpConnectionMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        if self.self_isolate_id.is_none() {
            self.self_shard_id = Some(call.shard_id());
            self.self_isolate_id = Some(call.me().isolate());
        }
        match msg {
            HttpConnectionMsg::RequestBodyNext => {
                if self.pending_request_body_reply.is_some() {
                    return call.reject(tina::CallRejectedReason::UnsupportedMessage);
                }
                self.pending_request_body_reply = Some(call.into_request_context());
                self.handle_request_body_next()
            }
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> HttpConnection<S, M> {
    /// First-effect hook. Issues both the initial `tcp_read` and the
    /// slow-loris deadline `sleep` in one batch so they race the
    /// client's bytes against the configured timeout.
    fn start(&mut self) -> Effect<Self> {
        self.begin_request_iteration(self.limits.header_read_timeout)
    }

    /// Schedule the next request iteration: bump the generation,
    /// arm a deadline tagged with that generation, and start a new
    /// `tcp_read`. Used both for the initial request (deadline =
    /// `header_read_timeout`) and each keepalive iteration (deadline
    /// = `keepalive_idle_timeout`).
    fn begin_request_iteration(&mut self, deadline: Duration) -> Effect<Self> {
        // Bound the generation counter the same way the keepalive
        // client does — 2^64 iterations is unreachable, and silent
        // overflow would let stale deadlines mis-match.
        self.request_generation = self
            .request_generation
            .checked_add(1)
            .expect("HttpConnection request_generation overflowed u64");
        self.head_deadline_armed = true;
        let generation = self.request_generation;
        let deadline_effect: Effect<Self> = sleep(deadline)
            .reply(move |result| HttpConnectionMsg::HeaderDeadline { generation, result });
        let read_effect: Effect<Self> = self.read_more();
        batch(vec![read_effect, deadline_effect])
    }

    /// Resets per-request state between keepalive iterations. Drops
    /// any read-ahead bytes (no pipelining), clears the parsed head,
    /// and resets streaming-body bookkeeping. Does not touch
    /// `request_generation` or `head_deadline_armed` — those are
    /// updated by [`Self::begin_request_iteration`].
    fn reset_for_next_request(&mut self) {
        self.release_request_all();
        self.release_response_all();
        self.read_buf.clear();
        self.parsed_head = None;
        self.head_len = 0;
        self.pending_response.clear();
        self.pending_buffered_body = None;
        // In normal flow the source is already cleared by
        // finish_stream_eof or handle_wrote. If it is still Some
        // here, the source is leaked; this method cannot issue
        // effects so we just drop the address.
        self.stream_source = None;
        self.stream_bytes_remaining = 0;
        self.stream_chunked = false;
        self.inbound_total = 0;
        self.inbound_received = 0;
        self.inbound_delivered = 0;
        self.inbound_buffer.clear();
        self.inbound_chunk_size = 0;
        self.chunked_decoder = None;
        self.chunked_raw_buffer.clear();
        self.inbound_chunked = false;
        self.body_eof_replied = false;
        self.pending_request_body_reply = None;
        self.will_close = false;
    }

    fn read_more(&mut self) -> Effect<Self> {
        match self.transport {
            HttpTransport::Tcp(stream) => {
                tcp_read(stream, READ_CHUNK).reply(HttpConnectionMsg::Read)
            }
            HttpTransport::Tls(stream) => {
                tls_read(stream, READ_CHUNK, self.tls_io_timeout).reply(HttpConnectionMsg::Read)
            }
        }
    }

    fn handle_bytes_read(&mut self, bytes: Vec<u8>) -> Effect<Self> {
        if bytes.is_empty() {
            // Peer closed cleanly. If we already parsed a head and have
            // a partial body, this is a truncated request — close
            // without dispatching. If we haven't parsed yet, also close.
            if self.parsed_head.is_some() {
                // Truncation: peer sent FIN before declared length.
                // Distinct from a clean post-body close.
                self.record_body_io_error();
            }
            let cancel = self.cancel_stream_source();
            let close = self.begin_close();
            return match cancel {
                Some(c) => batch(vec![c, close]),
                None => close,
            };
        }

        let pre_len = self.read_buf.len();
        self.read_buf.extend_from_slice(&bytes);

        if self.parsed_head.is_none() {
            match parse_request_head(&self.read_buf, &self.limits) {
                ParseProgress::NeedMore => return self.read_more(),
                ParseProgress::Complete { head, head_len } => {
                    let body_already_in_buf = self.read_buf.len().saturating_sub(head_len);
                    self.parsed_head = Some(head);
                    self.head_len = head_len;
                    if !self.parsed_head.as_ref().unwrap().chunked {
                        self.charge_request(body_already_in_buf);
                    }
                    self.head_deadline_armed = false;
                }
                ParseProgress::Failed(RequestParseError::BodyTooLarge) => {
                    self.head_deadline_armed = false;
                    self.record_body_full();
                    return self.send_parse_error(RequestParseError::BodyTooLarge);
                }
                ParseProgress::Failed(error) => {
                    self.head_deadline_armed = false;
                    return self.send_parse_error(error);
                }
            }
        } else {
            // Head already parsed. Every new byte read is body.
            let delta = self.read_buf.len() - pre_len;
            self.charge_request(delta);
        }

        self.maybe_dispatch_or_read_more()
    }

    /// Slow-loris / idle guard. Fires after
    /// [`HttpLimits::header_read_timeout`] on the first request and
    /// after [`HttpLimits::keepalive_idle_timeout`] on each keepalive
    /// iteration. If the head for the current generation has not
    /// parsed by the time this arrives, the connection stops and
    /// runtime cleanup closes the stream. Stale deadlines from
    /// previous generations are recognised and dropped.
    fn handle_header_deadline(&mut self, generation: u64) -> Effect<Self> {
        if generation != self.request_generation {
            // Stale deadline scheduled for a prior keepalive
            // iteration. The current iteration has its own deadline
            // racing the next request's head; this old one is now
            // a no-op.
            return noop();
        }
        if !self.head_deadline_armed {
            // Same generation but the head already parsed — the
            // deadline lost the race. No-op.
            return noop();
        }
        self.head_deadline_armed = false;
        // Slow-loris guard: the read lane has an outstanding
        // `tcp_read`, so issuing `tcp_close_stream` here would fail
        // with `CallError::ResourceBusy` (read and write lanes can run
        // concurrently, but explicit close cannot run while a lane is
        // pending). We could still try to write a 408 — the write
        // lane is free — but the close-after-write would then also
        // fail and the client would see no FIN. The cleanest path on
        // the current runtime is to stop the isolate; the runtime
        // cancels pending calls and drops the stream, which the
        // kernel observes as a clean connection close.
        //
        // Trade-off: no 408 reaches the slow client. RFC 7235 §5.5
        // recommends 408 but does not require it; a clean close is an
        // acceptable response to a slow-loris client. A future runtime
        // affordance — `tcp_cancel_read` or "close cancels pending
        // lanes" — would let us write the 408 first; tracked as a
        // 047/runtime ergonomics note.
        let cancel = self.cancel_stream_source();
        let stop_effect = stop();
        match cancel {
            Some(c) => batch(vec![c, stop_effect]),
            None => stop_effect,
        }
    }

    fn maybe_dispatch_or_read_more(&mut self) -> Effect<Self> {
        let head = self
            .parsed_head
            .as_ref()
            .expect("head parsed before dispatch");
        let streaming = self.limits.inbound_stream_chunk_size.is_some()
            && (head.content_length > 0 || head.chunked);
        if streaming {
            return self.dispatch_to_service();
        }
        let needed = self.head_len + head.content_length;
        if self.read_buf.len() < needed {
            self.read_more()
        } else {
            self.dispatch_to_service()
        }
    }

    fn dispatch_to_service(&mut self) -> Effect<Self> {
        let head = self
            .parsed_head
            .take()
            .expect("head parsed before dispatch");
        // Decide whether the connection closes after this response.
        // Force close when keepalive is disabled, or when the request
        // itself asks for it (HTTP/1.0 default, explicit
        // `Connection: close`).
        self.will_close = self.limits.keepalive_idle_timeout.is_none() || head.connection_close;

        // The body bytes are about to leave this isolate — either
        // handed to the service buffered, or moved into
        // `inbound_buffer` for streaming. Either way the charge
        // accounting flips: buffered releases everything now;
        // streaming keeps the inbound_buffer slice charged after
        // re-charging the moved prefix below.
        self.release_request_all();

        // Decide buffered vs streaming dispatch based on the limits.
        let request = match self.limits.inbound_stream_chunk_size {
            Some(chunk_size) if head.content_length > 0 || head.chunked => {
                let mut buf = std::mem::take(&mut self.read_buf);
                if head.chunked {
                    buf.drain(..self.head_len.min(buf.len()));
                    let mut decoder =
                        crate::chunked_decoder::ChunkedDecoder::new(self.limits.max_body_bytes);
                    let mut decoded = Vec::new();
                    let (progress, consumed) = {
                        let raw = &buf[..];
                        decoder.feed_all(raw, &mut decoded)
                    };
                    buf.drain(..consumed);
                    self.chunked_raw_buffer = buf;
                    self.inbound_total = 0;
                    self.inbound_received = consumed;
                    self.inbound_delivered = 0;
                    self.inbound_buffer = decoded;
                    self.inbound_chunk_size = chunk_size.max(1);
                    self.charge_request(self.inbound_buffer.len());
                    self.chunked_decoder = Some(decoder);
                    self.inbound_chunked = true;
                    match progress {
                        crate::chunked_decoder::FeedAllResult::Complete => {}
                        crate::chunked_decoder::FeedAllResult::Failed(_) => {
                            self.record_body_io_error();
                            self.will_close = true;
                            let response = HttpResponse::with_status(
                                crate::types::RequestParseError::MalformedChunkedBody.status(),
                            );
                            return self.start_writing(response);
                        }
                        crate::chunked_decoder::FeedAllResult::NeedMore => {}
                    }
                } else {
                    let body_end = self.head_len + head.content_length;
                    let prebuf_end = buf.len().min(body_end);
                    buf.truncate(prebuf_end);
                    buf.drain(..self.head_len.min(buf.len()));
                    self.inbound_total = head.content_length;
                    self.inbound_received = buf.len();
                    self.inbound_delivered = 0;
                    let pre_buf_len = buf.len();
                    self.inbound_buffer = buf;
                    self.inbound_chunk_size = chunk_size.max(1);
                    self.charge_request(pre_buf_len);
                }
                let me_chunk: Address<HttpConnectionMsg, RequestChunkReply> =
                    tina::Address::new_with_generation(
                        self.shard_id_for_self(),
                        self.isolate_id_for_self(),
                        tina::AddressGeneration::new(0),
                    );
                let stream = RequestStream {
                    content_length: self.inbound_total,
                    chunked: head.chunked,
                    source: me_chunk,
                };
                head.into_streaming_request(stream)
            }
            _ => {
                // Buffered: by the time we get here `read_buf` already
                // holds the full body — `maybe_dispatch_or_read_more`
                // returns to `read_more` until the buffer is full.
                //
                // Reuse `read_buf`'s allocation as the body and drop
                // anything else, so the per-connection memory budget
                // is just the body we hand to the service, not
                // `read_buf + body`.
                let body_end = self.head_len + head.content_length;
                let mut buf = std::mem::take(&mut self.read_buf);
                buf.truncate(body_end);
                buf.drain(..self.head_len);
                head.into_request(buf)
            }
        };
        call(self.service, M::from(request), self.service_call_timeout)
            .reply(HttpConnectionMsg::ServiceReturned)
    }

    /// Returns the shard id for self. The dispatch path needs this to
    /// build a typed self-address; we cannot use `ctx.me()` here
    /// because handler entrypoints take `&mut self` and `ctx` is at a
    /// higher scope. The values are recorded by the runtime when the
    /// isolate is registered and unchanging across handler turns —
    /// stash them in `start()` instead.
    fn shard_id_for_self(&self) -> tina::ShardId {
        self.self_shard_id.expect("shard id captured at start()")
    }

    fn isolate_id_for_self(&self) -> tina::IsolateId {
        self.self_isolate_id
            .expect("isolate id captured at start()")
    }

    /// Serves the next inbound body chunk to the calling service.
    ///
    /// - If we already have buffered bytes, drain a chunk from
    ///   `inbound_buffer`, advance `inbound_delivered`, reply.
    /// - If the buffer is empty but we have not received the full body
    ///   from the socket, issue a `tcp_read` and let the
    ///   `BodyChunkRead` continuation answer the service. The outer
    ///   call context (this `RequestBodyNext` call) propagates through
    ///   the `.reply(...)` chain, so a later `Effect::Reply` reaches
    ///   this caller.
    /// - If the buffer is empty and the full body has been delivered,
    ///   reply `Eof`.
    fn handle_request_body_next(&mut self) -> Effect<Self> {
        if self.body_eof_replied {
            return self.reply_request_body_chunk(RequestChunkReply::Eof);
        }
        if !self.inbound_buffer.is_empty() {
            return self.serve_chunk_from_buffer();
        }
        if self.inbound_chunked {
            if self
                .chunked_decoder
                .as_ref()
                .is_some_and(|d| d.is_complete())
            {
                self.body_eof_replied = true;
                return self.reply_request_body_chunk(RequestChunkReply::Eof);
            }
        } else {
            if self.inbound_delivered >= self.inbound_total {
                self.body_eof_replied = true;
                return self.reply_request_body_chunk(RequestChunkReply::Eof);
            }
        }
        let want = if self.inbound_chunked {
            let max = READ_CHUNK.min(self.inbound_chunk_size.max(1));
            self.chunked_decoder
                .as_ref()
                .map(|decoder| decoder.preferred_read_size(max))
                .unwrap_or(1)
        } else {
            self.inbound_total
                .saturating_sub(self.inbound_received)
                .min(READ_CHUNK)
                .min(self.inbound_chunk_size.max(1))
        };
        if want == 0 {
            self.body_eof_replied = true;
            return self.reply_request_body_chunk(RequestChunkReply::Eof);
        }
        match self.transport {
            HttpTransport::Tcp(stream) => {
                tcp_read(stream, want).reply(HttpConnectionMsg::BodyChunkRead)
            }
            HttpTransport::Tls(stream) => {
                tls_read(stream, want, self.tls_io_timeout).reply(HttpConnectionMsg::BodyChunkRead)
            }
        }
    }

    fn handle_body_chunk_read(&mut self, result: Result<Vec<u8>, CallError>) -> Effect<Self> {
        let bytes = match result {
            Ok(bytes) => bytes,
            // Surface the typed error so service can tell short
            // delivery (Eof) from truncation (Error). Distinguish
            // timeout from other IO errors in the metrics, and
            // latch eof so a follow-up body_next does not re-issue
            // the failing read.
            Err(error) => {
                match error {
                    CallError::Timeout => self.record_body_timeout(),
                    _ => self.record_body_io_error(),
                }
                self.body_eof_replied = true;
                return self.reply_request_body_chunk(RequestChunkReply::Error(error));
            }
        };
        if bytes.is_empty() {
            // Peer closed mid-body. Service notices via `delivered < expected`.
            // The wire was short, so this is a truncation event from a
            // metrics standpoint — the server cannot fulfil the
            // declared length.
            if self.inbound_chunked
                || self.inbound_delivered + self.inbound_buffer.len() < self.inbound_total
            {
                self.record_body_io_error();
            }
            self.body_eof_replied = true;
            return self.reply_request_body_chunk(RequestChunkReply::Eof);
        }
        if self.inbound_chunked {
            self.inbound_received += bytes.len();
            self.chunked_raw_buffer.extend_from_slice(&bytes);
            if let Some(ref mut decoder) = self.chunked_decoder {
                let prev_len = self.inbound_buffer.len();
                let max = READ_CHUNK.min(self.inbound_chunk_size.max(1));
                let (progress, consumed, next_want) = {
                    let raw = &self.chunked_raw_buffer[..];
                    let (progress, consumed) = decoder.feed_all(raw, &mut self.inbound_buffer);
                    let next_want = decoder.preferred_read_size(max);
                    (progress, consumed, next_want)
                };
                self.chunked_raw_buffer.drain(..consumed);
                let new_decoded = self.inbound_buffer.len() - prev_len;
                self.charge_request(new_decoded);
                match progress {
                    crate::chunked_decoder::FeedAllResult::Complete => {
                        if !self.inbound_buffer.is_empty() {
                            return self.serve_chunk_from_buffer();
                        }
                        self.body_eof_replied = true;
                        return self.reply_request_body_chunk(RequestChunkReply::Eof);
                    }
                    crate::chunked_decoder::FeedAllResult::Failed(_) => {
                        self.record_body_io_error();
                        self.body_eof_replied = true;
                        return self
                            .reply_request_body_chunk(RequestChunkReply::Error(CallError::Io));
                    }
                    crate::chunked_decoder::FeedAllResult::NeedMore => {
                        if !self.inbound_buffer.is_empty() {
                            return self.serve_chunk_from_buffer();
                        }
                        return match self.transport {
                            HttpTransport::Tcp(stream) => {
                                tcp_read(stream, next_want).reply(HttpConnectionMsg::BodyChunkRead)
                            }
                            HttpTransport::Tls(stream) => {
                                tls_read(stream, next_want, self.tls_io_timeout)
                                    .reply(HttpConnectionMsg::BodyChunkRead)
                            }
                        };
                    }
                }
            }
        } else {
            self.inbound_received += bytes.len();
            self.inbound_buffer.extend_from_slice(&bytes);
            self.charge_request(bytes.len());
        }
        self.serve_chunk_from_buffer()
    }

    fn serve_chunk_from_buffer(&mut self) -> Effect<Self> {
        let take = if self.inbound_chunked {
            self.inbound_chunk_size.min(self.inbound_buffer.len())
        } else {
            let remaining_total = self.inbound_total - self.inbound_delivered;
            self.inbound_chunk_size
                .min(self.inbound_buffer.len())
                .min(remaining_total)
        };
        let chunk: Vec<u8> = self.inbound_buffer.drain(..take).collect();
        self.inbound_delivered += take;
        // The chunk has left the connection; release its charge.
        self.release_request(take);
        self.reply_request_body_chunk(RequestChunkReply::Chunk(chunk))
    }

    fn reply_request_body_chunk(&mut self, chunk: RequestChunkReply) -> Effect<Self> {
        match self.pending_request_body_reply.take() {
            Some(request) => reply_to_request(request, chunk),
            None => reply(chunk),
        }
    }

    fn handle_service_outcome(&mut self, outcome: CallOutcome<HttpResponse>) -> Effect<Self> {
        let response = match outcome.into_result() {
            Ok(response) => response,
            Err(call_error) => {
                self.will_close = true;
                response_for_call_error(&call_error)
            }
        };
        self.start_writing(response)
    }

    fn send_parse_error(&mut self, error: RequestParseError) -> Effect<Self> {
        self.will_close = true;
        let response = HttpResponse::with_status(error.status());
        self.start_writing(response)
    }

    fn start_writing(&mut self, response: HttpResponse) -> Effect<Self> {
        // Write the head first, then the body. Splitting them keeps
        // the body-pressure counter honest: `pending_response`
        // contains *only* head bytes until the head drains, then
        // we promote the body and charge it. Without this split, a
        // partial head write would release body charge that hadn't
        // gone on the wire yet, making `current` lie.
        let head_bytes = encode_response_head(&response, self.will_close);
        match response.body {
            HttpResponseBody::Buffered(body_bytes) => {
                self.pending_response = head_bytes;
                if !body_bytes.is_empty() {
                    self.pending_buffered_body = Some(body_bytes);
                }
                self.write_pending()
            }
            HttpResponseBody::Stream(ResponseStream {
                content_length,
                source,
            }) => {
                self.pending_response = head_bytes;
                self.stream_source = Some(source);
                self.stream_bytes_remaining = content_length;
                self.stream_chunked = false;
                self.write_pending()
            }
            HttpResponseBody::ChunkedStream(crate::streaming::ChunkedResponseStream { source }) => {
                self.pending_response = head_bytes;
                self.stream_source = Some(source);
                self.stream_bytes_remaining = 0;
                self.stream_chunked = true;
                self.write_pending()
            }
        }
    }

    /// Issues a `tcp_write` for whatever still remains in
    /// `self.pending_response`. The drain happens in `handle_wrote` once
    /// we know how many bytes the runtime accepted; we do not pre-copy
    /// the buffer with an offset.
    fn write_pending(&mut self) -> Effect<Self> {
        let bytes = self.pending_response.clone();
        match self.transport {
            HttpTransport::Tcp(stream) => tcp_write(stream, bytes).reply(HttpConnectionMsg::Wrote),
            HttpTransport::Tls(stream) => {
                tls_write(stream, bytes, self.tls_io_timeout).reply(HttpConnectionMsg::Wrote)
            }
        }
    }

    fn handle_wrote(&mut self, count: usize) -> Effect<Self> {
        if count == 0 {
            // Wire stalled with no progress — treat as truncation
            // when a body was still owed.
            if self.has_pending_body() {
                self.record_body_io_error();
            }
            let cancel = self.cancel_stream_source();
            let close = self.begin_close();
            return match cancel {
                Some(c) => batch(vec![c, close]),
                None => close,
            };
        }
        // `pending_response` only ever contains head bytes (no
        // charge) or body bytes (charged). So whatever we just
        // sent, releasing `count` against the charge is exact:
        // head writes release zero (charge is zero); body writes
        // release exactly the body bytes that drained.
        let release = count.min(self.metrics_response_charge);
        self.release_response(release);
        if count >= self.pending_response.len() {
            self.pending_response.clear();
            // Buffer drained. Three cases:
            //  - buffered body was queued behind the head -> promote
            //    it, charge once, write it.
            //  - streaming source still owes bytes -> pull next.
            //  - nothing left -> either close or loop back for the
            //    next keepalive request.
            if let Some(body) = self.pending_buffered_body.take() {
                let n = body.len();
                self.pending_response = body;
                self.charge_response(n);
                return self.write_pending();
            }
            // Streaming branch: pull the next chunk if the source
            // still owes bytes. For known-length streams this means
            // `stream_bytes_remaining > 0`; for chunked streams we
            // pull until the source replies `Eof`.
            let should_pull = match (self.stream_source.is_some(), self.stream_chunked) {
                (true, true) => true,
                (true, false) => self.stream_bytes_remaining > 0,
                (false, _) => false,
            };
            if should_pull {
                self.pull_next_chunk()
            } else {
                self.stream_source = None;
                self.stream_chunked = false;
                self.finish_response()
            }
        } else {
            self.pending_response.drain(..count);
            self.write_pending()
        }
    }

    /// Called once a response has fully drained to the wire. Either
    /// closes the connection (one-shot mode, or per-request close
    /// intent) or resets state and starts the next keepalive
    /// iteration.
    fn finish_response(&mut self) -> Effect<Self> {
        if self.will_close {
            return self.begin_close();
        }
        // Keepalive iteration: idle timeout is whatever the user
        // configured. We only reach here when keepalive is on
        // (otherwise will_close was forced to true in
        // dispatch_to_service), so the unwrap is safe.
        let idle = self
            .limits
            .keepalive_idle_timeout
            .expect("keepalive iteration only happens when timeout is configured");
        self.reset_for_next_request();
        self.begin_request_iteration(idle)
    }

    /// True when this connection still owes the wire body bytes —
    /// either currently queued (`metrics_response_charge`), parked
    /// behind the head (`pending_buffered_body`), or coming from a
    /// streaming source.
    fn has_pending_body(&self) -> bool {
        if self.metrics_response_charge > 0 || self.pending_buffered_body.is_some() {
            return true;
        }
        match (self.stream_source.is_some(), self.stream_chunked) {
            (true, true) => true,
            (true, false) => self.stream_bytes_remaining > 0,
            (false, _) => false,
        }
    }

    /// Issues a `call(source, Next, t).reply(StreamChunk)` to pull the
    /// next chunk of a streamed response.
    fn pull_next_chunk(&mut self) -> Effect<Self> {
        let source = self.stream_source.expect("stream source set");
        call(source, ResponseChunkMsg::Next, self.stream_call_timeout)
            .reply(HttpConnectionMsg::StreamChunk)
    }

    fn handle_stream_chunk(&mut self, outcome: CallOutcome<ResponseChunkReply>) -> Effect<Self> {
        match outcome {
            CallOutcome::Replied(ResponseChunkReply::Chunk(bytes)) => {
                if bytes.is_empty() {
                    // Empty chunk = Eof. For known-length streams a
                    // short wire is a truncation event; for chunked
                    // streams it just means "no more data" and we
                    // emit the terminator.
                    if !self.stream_chunked && self.stream_bytes_remaining > 0 {
                        self.record_body_io_error();
                    }
                    return self.finish_stream_eof();
                }
                if self.stream_chunked {
                    self.write_chunked_data(bytes)
                } else {
                    self.write_known_length_chunk(bytes)
                }
            }
            CallOutcome::Replied(ResponseChunkReply::Eof) => {
                // Source finished. For known-length the declared
                // Content-Length is canonical; under-produce is a
                // truncation. For chunked we emit the `0 CRLF CRLF`
                // terminator and close after it drains.
                if !self.stream_chunked && self.stream_bytes_remaining > 0 {
                    self.record_body_io_error();
                }
                self.finish_stream_eof()
            }
            CallOutcome::Timeout => {
                // Source took too long to produce the next chunk.
                // The wire is now in an incomplete state — for
                // known-length streams the Content-Length is short,
                // for chunked streams there is no terminator. Both
                // are user-visible truncations. We record the
                // *cause* (timeout) and the *symptom*
                // (io_error/truncation) so a snapshot reader can
                // see both why and what.
                self.record_body_timeout();
                self.record_body_io_error();
                let cancel = self.cancel_stream_source();
                self.stream_chunked = false;
                let close = self.begin_close();
                match cancel {
                    Some(c) => batch(vec![c, close]),
                    None => close,
                }
            }
            CallOutcome::Full | CallOutcome::Closed | CallOutcome::Rejected(_) => {
                // Source died mid-stream. Close the wire — the
                // client sees a truncated body relative to the
                // framing. We do not try to inject an error
                // response on top of an already-emitted head.
                self.record_body_io_error();
                let cancel = self.cancel_stream_source();
                self.stream_chunked = false;
                let close = self.begin_close();
                match cancel {
                    Some(c) => batch(vec![c, close]),
                    None => close,
                }
            }
        }
    }

    /// Frames `bytes` for a known-length stream and queues the
    /// write. Truncates if the source over-produced.
    fn write_known_length_chunk(&mut self, bytes: Vec<u8>) -> Effect<Self> {
        let written_bytes = if bytes.len() > self.stream_bytes_remaining {
            // Source over-produced relative to declared length.
            // Truncate to keep the wire framing honest.
            let mut truncated = bytes;
            truncated.truncate(self.stream_bytes_remaining);
            self.stream_bytes_remaining = 0;
            truncated
        } else {
            self.stream_bytes_remaining -= bytes.len();
            bytes
        };
        let n = written_bytes.len();
        self.pending_response = written_bytes;
        self.charge_response(n);
        self.write_pending()
    }

    /// Frames `bytes` as one HTTP/1.1 chunked-transfer chunk:
    /// `<size in hex>\r\n<bytes>\r\n`. Body charge counts only the
    /// data bytes, not the framing overhead. Hex digits are
    /// lowercase for parity with curl/hyper/nginx — the spec
    /// allows either case but the wire convention is lowercase.
    fn write_chunked_data(&mut self, bytes: Vec<u8>) -> Effect<Self> {
        let n = bytes.len();
        let mut framed = format!("{:x}\r\n", n).into_bytes();
        framed.extend_from_slice(&bytes);
        framed.extend_from_slice(b"\r\n");
        self.pending_response = framed;
        self.charge_response(n);
        self.write_pending()
    }

    /// Source replied `Eof` (or empty chunk). For chunked, queue the
    /// `0 CRLF CRLF` terminator and let the next `handle_wrote`
    /// finish once it drains. For known-length, the declared
    /// `Content-Length` is canonical; if the source produced exactly
    /// that many bytes the connection can still be reused.
    fn finish_stream_eof(&mut self) -> Effect<Self> {
        self.stream_source = None;
        if self.stream_chunked {
            self.stream_chunked = false;
            // Terminator is framing, not body — no charge.
            self.pending_response = b"0\r\n\r\n".to_vec();
            self.write_pending()
        } else {
            if self.stream_bytes_remaining > 0 {
                self.will_close = true;
            }
            self.finish_response()
        }
    }

    /// Sends `Cancel` to the response body source if one is still
    /// referenced, then clears the reference. Called before the
    /// connection abandons the wire so the source can release files,
    /// downstream calls, and pending slots. Duplicate cancels are
    /// harmless — the source either already stopped or will drop the
    /// message after it finishes draining.
    fn cancel_stream_source(&mut self) -> Option<Effect<Self>> {
        let source = self.stream_source.take()?;
        Some(
            call(
                source,
                crate::streaming::ResponseChunkMsg::Cancel,
                // Short timeout: the message is queued immediately; we
                // only need the reply slot to close quickly so the
                // connection can stop without leaking the call.
                Duration::from_millis(1),
            )
            .reply(HttpConnectionMsg::StreamSourceCancelDone),
        )
    }

    fn begin_close(&mut self) -> Effect<Self> {
        // Any body bytes still resident in this isolate are about to
        // be dropped; release them so the metrics' `current` returns
        // to zero on a clean shutdown sequence.
        self.release_request_all();
        self.release_response_all();
        // Defensive: if a stream source is still referenced, tell it
        // to release state. Most callers already sent cancel on the
        // specific error path; duplicating here is harmless.
        let cancel = self.cancel_stream_source();
        let close = match self.transport {
            HttpTransport::Tcp(stream) => tcp_close_stream(stream).reply(HttpConnectionMsg::Closed),
            HttpTransport::Tls(stream) => {
                tls_close(stream, self.tls_io_timeout).reply(HttpConnectionMsg::Closed)
            }
        };
        match cancel {
            Some(c) => batch(vec![c, close]),
            None => close,
        }
    }

    // --- BodyMetrics helpers --------------------------------------

    fn charge_request(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if let Some(metrics) = &self.metrics {
            metrics.charge_request(n);
            self.metrics_request_charge += n;
        }
    }

    fn release_request(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let take = n.min(self.metrics_request_charge);
        if take == 0 {
            return;
        }
        if let Some(metrics) = &self.metrics {
            metrics.release_request(take);
        }
        self.metrics_request_charge -= take;
    }

    fn release_request_all(&mut self) {
        if self.metrics_request_charge == 0 {
            return;
        }
        if let Some(metrics) = &self.metrics {
            metrics.release_request(self.metrics_request_charge);
        }
        self.metrics_request_charge = 0;
    }

    fn charge_response(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if let Some(metrics) = &self.metrics {
            metrics.charge_response(n);
            self.metrics_response_charge += n;
        }
    }

    fn release_response(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let take = n.min(self.metrics_response_charge);
        if take == 0 {
            return;
        }
        if let Some(metrics) = &self.metrics {
            metrics.release_response(take);
        }
        self.metrics_response_charge -= take;
    }

    fn release_response_all(&mut self) {
        if self.metrics_response_charge == 0 {
            return;
        }
        if let Some(metrics) = &self.metrics {
            metrics.release_response(self.metrics_response_charge);
        }
        self.metrics_response_charge = 0;
    }

    fn record_body_full(&self) {
        if let Some(metrics) = &self.metrics {
            metrics.record_body_full();
        }
    }

    fn record_body_timeout(&self) {
        if let Some(metrics) = &self.metrics {
            metrics.record_body_timeout();
        }
    }

    fn record_body_io_error(&self) {
        if let Some(metrics) = &self.metrics {
            metrics.record_body_io_error();
        }
    }
}

// Drop catches isolates abandoned without a clean close path
// (panic, runtime stop, force-close). Without this, an isolate
// dropped mid-body would leave its charge resident in the shared
// metrics forever, breaking the "drained()" terminal assertion.
impl<S: Shard, M: From<HttpRequest> + Send + 'static> Drop for HttpConnection<S, M> {
    fn drop(&mut self) {
        if let Some(metrics) = &self.metrics {
            if self.metrics_request_charge > 0 {
                metrics.release_request(self.metrics_request_charge);
            }
            if self.metrics_response_charge > 0 {
                metrics.release_response(self.metrics_response_charge);
            }
        }
        self.metrics_request_charge = 0;
        self.metrics_response_charge = 0;
    }
}

/// Maps a runtime `CallError` from the service call into a synthetic HTTP
/// response.
///
/// Every variant of [`CallError`] is matched explicitly: adding a new
/// variant in `tina-runtime` causes a compile error here, forcing an
/// intentional decision rather than a silent default to `500`.
///
/// | `CallError`           | Status                       |
/// |-----------------------|------------------------------|
/// | `TargetFull`          | `503 Service Unavailable`    |
/// | `Timeout`             | `504 Gateway Timeout`        |
/// | `TargetClosed`        | `500 Internal Server Error`  |
/// | `InvalidResource`     | `500 Internal Server Error`  |
/// | `Io`                  | `500 Internal Server Error`  |
/// | `Unsupported`         | `500 Internal Server Error`  |
/// | `ResourceBusy`        | `500 Internal Server Error`  |
/// | `NotFound`            | `500 Internal Server Error`  |
/// | persistence variants  | `500 Internal Server Error`  |
/// | DNS/TLS/process/signal variants | `500 Internal Server Error` |
fn response_for_call_error(error: &CallError) -> HttpResponse {
    let status = match error {
        // Backpressure: service mailbox was full. Standard HTTP shape
        // for "try again later" is 503.
        CallError::TargetFull => StatusCode::SERVICE_UNAVAILABLE,
        // The service did not reply before our call timeout elapsed.
        CallError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        // Service address became unavailable (panicked, stopped,
        // stale). From the client's perspective this is a server-side
        // fault.
        CallError::TargetClosed => StatusCode::INTERNAL_SERVER_ERROR,
        // The remaining variants describe runtime-level faults that do
        // not have a clean HTTP-shaped equivalent. We collapse them all
        // to 500 so the wire response is still well-formed; the trace
        // carries the precise reason. Listed exhaustively so a future
        // CallError variant in tina-runtime forces a compile error here
        // rather than silently routing through a default.
        CallError::InvalidResource
        | CallError::NotFound
        | CallError::Io
        | CallError::Unsupported
        | CallError::ResourceBusy
        | CallError::CorruptRecord
        | CallError::CommitUncertain
        | CallError::StorageFull
        | CallError::StorageClosed
        | CallError::DnsFull
        | CallError::DnsClosed
        | CallError::TlsFull
        | CallError::TlsClosed
        | CallError::TlsCertificate
        | CallError::TlsName
        | CallError::TlsHandshake
        | CallError::SignalFull
        | CallError::SignalClosed
        | CallError::ProcessFull
        | CallError::ProcessClosed
        | CallError::KillUncertain
        | CallError::Rejected(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    HttpResponse::with_status(status)
}

/// Projects a [`CallOutcome`] into an HTTP response when it is *not* a
/// successful reply.
///
/// Returns `None` when the outcome carries a real reply; the caller is
/// expected to use that reply directly. Returns `Some(response)` for
/// `Full`, `Closed`, and `Timeout`, with the same status mapping used by
/// the connection isolate's runtime-call error path.
///
/// Exposed publicly so service-side code can build the same mapping
/// when wrapping a downstream call into its own response shape.
pub fn response_for_call_outcome(outcome: &CallOutcome<HttpResponse>) -> Option<HttpResponse> {
    match outcome {
        CallOutcome::Replied(_) => None,
        CallOutcome::Full => Some(HttpResponse::with_status(StatusCode::SERVICE_UNAVAILABLE)),
        CallOutcome::Closed => Some(HttpResponse::with_status(StatusCode::INTERNAL_SERVER_ERROR)),
        CallOutcome::Timeout => Some(HttpResponse::with_status(StatusCode::GATEWAY_TIMEOUT)),
        CallOutcome::Rejected(_) => {
            Some(HttpResponse::with_status(StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_call_error_maps_to_503() {
        assert_eq!(
            response_for_call_error(&CallError::TargetFull).status,
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }

    #[test]
    fn closed_call_error_maps_to_500() {
        assert_eq!(
            response_for_call_error(&CallError::TargetClosed).status,
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }

    #[test]
    fn timeout_call_error_maps_to_504() {
        assert_eq!(
            response_for_call_error(&CallError::Timeout).status,
            StatusCode::GATEWAY_TIMEOUT,
        );
    }

    #[test]
    fn full_outcome_projects_to_503() {
        let response = response_for_call_outcome(&CallOutcome::<HttpResponse>::Full)
            .expect("Full projects to a response");
        assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn closed_outcome_projects_to_500() {
        let response = response_for_call_outcome(&CallOutcome::<HttpResponse>::Closed)
            .expect("Closed projects to a response");
        assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn timeout_outcome_projects_to_504() {
        let response = response_for_call_outcome(&CallOutcome::<HttpResponse>::Timeout)
            .expect("Timeout projects to a response");
        assert_eq!(response.status, StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn replied_outcome_projects_to_none() {
        let response = response_for_call_outcome(&CallOutcome::Replied(HttpResponse::ok()));
        assert!(
            response.is_none(),
            "successful replies do not project to a synthetic response"
        );
    }
}
