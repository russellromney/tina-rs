//! HTTP/1.1 keepalive connection isolate and origin key.
//!
//! [`KeepaliveConnection`] owns one TCP or TLS transport across many
//! requests. Unlike [`crate::HttpClient`], which connects + sends +
//! reads + closes per call, this isolate connects on first use and
//! reuses the same transport until the peer closes or an error
//! happens — at which point it drops the transport and reconnects
//! transparently on the next [`KeepaliveConnectionMsg::Request`].
//!
//! # Pool consumer pattern
//!
//! A keepalive pool is a [`tina_runtime::pool::WorkerPool`] over a
//! fixed list of `Address<KeepaliveConnectionMsg, KeepaliveOutcome>`
//! handles. Build one with [`build_keepalive_pool`].
//!
//! **Always release `Reuse`.** The connection isolate self-heals: on
//! `must_retire = true` it drops the bad transport and reconnects on
//! the next request automatically. Releasing `Retire` permanently
//! removes that slot from the pool, draining capacity to zero over
//! time if you do it on every must_retire. `Retire` is reserved for
//! the rare "the connection isolate itself is suspect" case (panic,
//! observed protocol violation) — most consumers never need it.
//!
//! ```text
//! acquire lease (Address<KeepaliveConnectionMsg, KeepaliveOutcome>)
//! call(*lease.handle(), KeepaliveConnectionMsg::request(req, t),
//!      deadline_remaining)
//!   -> KeepaliveOutcome::Request { result, must_retire }
//! release(lease, Reuse)   // always — the connection self-heals
//! ```
//!
//! The `must_retire` field is informational. Inspect it for tracing,
//! metrics, or to decide whether to retry the request from the
//! caller's side. The connection has already dropped the bad
//! transport before it sent the reply.
//!
//! # Shutdown
//!
//! `WorkerPool` does not own the connection isolates' lifetime. After
//! [`tina::pool::CloseMode::Drain`] or `Force`, the isolates keep
//! running with their open transports until you stop them. Use the
//! [`KeepaliveConnAddr`] handles returned from
//! [`build_keepalive_pool`] to call [`KeepaliveConnectionMsg::Stop`]
//! on each isolate and check for [`KeepaliveOutcome::Stopped`]; the
//! isolate closes its transport (if any) and exits. For host-side
//! shutdown, [`shutdown_keepalive_pool`] performs the pool close,
//! waits for `Drain` leases to return, then performs per-connection
//! stop calls once and returns a counted report.
//!
//! # Origin keying
//!
//! [`OriginKey`] captures the identity that two requests must share
//! to safely reuse a connection: scheme + `SocketAddr` + (for HTTPS)
//! server name + the configured DER trust roots themselves (stored,
//! not hashed — no collision risk). A pool's connections are
//! pre-bound to one [`HttpTarget`] at registration, so cross-origin
//! reuse is impossible at the connection-isolate level: the only
//! way to send to a different origin is to register a different
//! pool. `OriginKey` is the value type that names that identity for
//! diagnostics and equality checks.
//!
//! # Out of scope (first form)
//!
//! - No idle-connection timeout. A long-idle stale socket is
//!   discovered on the next request via a write/read failure and
//!   reported as `must_retire`.
//! - No hidden retry. A `must_retire` reply is the consumer's signal
//!   to choose whether to acquire another lease and retry, or
//!   surface the failure.
//! - No HTTP/2, no chunked transfer, no expect-100-continue. Same
//!   contract as [`crate::HttpClient`].
//! - No HTTP `Upgrade` (e.g., to WebSocket). The connection assumes
//!   the response framing remains HTTP/1.1.

use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};

use http::HeaderValue;
use http::header::HOST;
use tina::pool::{CloseMode, PoolConfig};
use tina::prelude::*;
use tina::{CallContext, RequestContext, reply_to_request};
use tina_runtime::pool::{WorkerPool, WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallError, CallOutcome, ThreadedRuntime, ThreadedRuntimeError, sleep, tcp_close_stream,
    tcp_connect, tcp_read, tcp_write, tls_close, tls_connect, tls_read, tls_write,
};

use crate::chunked_decoder::{ChunkedDecoder, FeedAllResult};
use crate::parse::{
    HttpResponseHead, ResponseParseProgress, encode_keepalive_request,
    is_valid_origin_form_request_target, parse_response_head,
};
use crate::target::{HttpHostPolicy, HttpTarget};
use crate::transport::HttpTransport;
use crate::types::{
    HttpClientConfig, HttpClientError, HttpRequest, HttpResponse, HttpTransportPhase,
};

/// Bytes the connection asks for per `tcp_read`. Same as
/// [`crate::HttpClient`].
const READ_CHUNK: usize = 4096;

/// Identity that two requests must share to safely reuse a keepalive
/// connection.
///
/// HTTPS variants store the configured DER trust roots verbatim so
/// two configs with different roots never collide, even when their
/// server name and address agree. The cost is one `Vec<Vec<u8>>`
/// clone at construction time; the keys live for the pool's
/// lifetime, not per-request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OriginKey {
    /// Plain HTTP keyed by socket address.
    Http {
        /// Peer socket address. The keepalive contract is "same TCP
        /// peer"; two different IPs that happen to host the same
        /// site must not share a connection.
        addr: SocketAddr,
    },
    /// HTTPS keyed by socket address, SNI server name, and the DER
    /// trust roots themselves.
    Https {
        /// Peer socket address.
        addr: SocketAddr,
        /// SNI / certificate verification name used at handshake.
        server_name: String,
        /// Configured DER trust roots, stored verbatim. Two configs
        /// whose roots differ produce different keys; there is no
        /// hashing step and therefore no collision risk for a
        /// security-relevant identity comparison.
        trust_roots_der: Vec<Vec<u8>>,
    },
}

impl OriginKey {
    /// Builds the key for an [`HttpTarget`].
    pub fn from_target(target: &HttpTarget) -> Self {
        match target {
            HttpTarget::Http { addr, .. } => Self::Http { addr: *addr },
            HttpTarget::Https {
                addr,
                server_name,
                trust_roots,
                ..
            } => Self::Https {
                addr: *addr,
                server_name: server_name.clone(),
                trust_roots_der: trust_roots.root_certificates_der.clone(),
            },
        }
    }
}

/// Inbound message variants for [`KeepaliveConnection`].
///
/// `Request` and `Stop` are user-callable. The remaining variants are
/// continuations from runtime calls the connection issues itself.
#[derive(Debug, Clone)]
pub enum KeepaliveConnectionMsg {
    /// Run one request on this connection. The connection connects on
    /// first use, then reuses the transport until the peer closes or
    /// an error happens.
    ///
    /// `request_timeout` is the wall-clock budget for the entire
    /// request — connect (if needed), write, read head, read body.
    /// Pass `Deadline::remaining_or_zero(now)` from a caller-owned
    /// deadline to propagate budgets honestly across hops.
    Request {
        request: Box<HttpRequest>,
        request_timeout: Duration,
    },
    /// Stop the isolate. Closes the underlying transport (if any),
    /// then the isolate exits via [`tina::stop`]. Reply is sent
    /// before the isolate stops; consumers waiting on the reply will
    /// see `KeepaliveOutcome::Stopped`.
    ///
    /// Used during pool shutdown to avoid leaking transports past
    /// pool close. See module docs for the recommended sequence.
    Stop,

    Connected(Result<(tina_runtime::StreamId, SocketAddr, SocketAddr), CallError>),
    TlsConnected(Result<tina_runtime::TlsStreamId, CallError>),
    Wrote(Result<usize, CallError>),
    Read(Result<Vec<u8>, CallError>),
    Closed(Result<(), CallError>),
    /// `generation` distinguishes the deadline for *this* request
    /// from stale deadlines scheduled by prior requests on the same
    /// (long-lived) connection isolate. Without it, a 2s deadline
    /// armed for request N would arrive during request N+1 and fail
    /// it spuriously.
    Deadline {
        generation: u64,
        result: Result<(), CallError>,
    },
}

impl KeepaliveConnectionMsg {
    /// Convenience constructor for the user-facing `Request` variant.
    pub fn request(request: HttpRequest, request_timeout: Duration) -> Self {
        Self::Request {
            request: Box::new(request),
            request_timeout,
        }
    }
}

/// Reply payload from [`KeepaliveConnection`].
#[derive(Debug, Clone)]
pub enum KeepaliveOutcome {
    /// One request finished. `must_retire` is true when the
    /// connection just dropped its transport because it was no
    /// longer safe to reuse: error during connect/write/read, the
    /// server returned `Connection: close`, or the peer closed
    /// before the body completed. The connection has already
    /// reconnected-on-next-request semantics — the consumer can
    /// safely release `Reuse` either way.
    Request {
        result: Result<HttpResponse, HttpClientError>,
        must_retire: bool,
    },
    /// The isolate received [`KeepaliveConnectionMsg::Stop`] and is
    /// shutting down. Subsequent messages will fail with
    /// `CallOutcome::Closed` once the runtime tears the address
    /// down.
    Stopped,
}

impl KeepaliveOutcome {
    /// Convenience: returns the request result if this is a `Request`
    /// reply; panics otherwise. Tests use this; consumers should
    /// match the variant explicitly.
    pub fn into_request_result(self) -> (Result<HttpResponse, HttpClientError>, bool) {
        match self {
            Self::Request {
                result,
                must_retire,
            } => (result, must_retire),
            Self::Stopped => panic!("expected Request outcome, got Stopped"),
        }
    }
}

/// Per-slot keepalive HTTP/1.1 connection.
///
/// One isolate owns one transport. Generic over the user's `Shard`
/// type so the pool's slots can be placed wherever the user wants.
pub struct KeepaliveConnection<S: Shard + 'static> {
    target: HttpTarget,
    origin: OriginKey,
    config: HttpClientConfig,
    transport: Option<HttpTransport>,
    in_flight: Option<InFlight>,
    /// Bytes for an in-flight request that is still waiting on
    /// `tcp_connect` / `tls_connect`. Moved into `InFlight.pending_write`
    /// when the `Connected` continuation fires; `None` outside the
    /// cold-connect path.
    pending_connect_bytes: Option<Vec<u8>>,
    /// Monotonic per-request counter. Bumped at the start of every
    /// `handle_request`. The `Deadline` continuation carries the
    /// generation in its payload; a generation mismatch flags the
    /// message as stale and a no-op.
    request_generation: u64,
    _shard: PhantomData<S>,
}

struct InFlight {
    /// Generation stamped at request start; used to ignore stale
    /// `Deadline` continuations from prior requests.
    generation: u64,
    pending_write: Vec<u8>,
    read_buf: Vec<u8>,
    decoded_chunked_body: Vec<u8>,
    chunked_decoder: Option<ChunkedDecoder>,
    chunked_raw_consumed: usize,
    parsed_head: Option<HttpResponseHead>,
    head_len: usize,
    reply_to: Option<RequestContext<KeepaliveOutcome>>,
}

impl<S: Shard + 'static> KeepaliveConnection<S> {
    /// Builds a new connection bound to `target`. The connection does
    /// not connect at construction; the first `Request` triggers
    /// `tcp_connect` / `tls_connect`.
    pub fn new(target: HttpTarget, config: HttpClientConfig) -> Self {
        let origin = OriginKey::from_target(&target);
        Self {
            target,
            origin,
            config,
            transport: None,
            in_flight: None,
            pending_connect_bytes: None,
            request_generation: 0,
            _shard: PhantomData,
        }
    }

    /// Origin this connection is bound to. Useful for diagnostic
    /// logging — pool slots are anonymous from the pool's
    /// perspective; this accessor lets a debug formatter or trace
    /// fact name the served origin.
    pub fn origin(&self) -> &OriginKey {
        &self.origin
    }
}

// Hand-rolled `Isolate`: the macro requires a concrete shard, we
// want `KeepaliveConnection` to work with any user shard.
impl<S: Shard + 'static> Isolate for KeepaliveConnection<S> {
    tina::isolate_types! {
        message: KeepaliveConnectionMsg,
        reply: KeepaliveOutcome,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: tina_runtime::RuntimeCall<KeepaliveConnectionMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: KeepaliveConnectionMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            KeepaliveConnectionMsg::Request {
                request,
                request_timeout,
            } => self.handle_request(*request, request_timeout, None),

            KeepaliveConnectionMsg::Stop => self.handle_stop(None),

            KeepaliveConnectionMsg::Connected(Ok((stream, _local, _peer))) => {
                if self.in_flight.is_none() {
                    return tcp_close_stream(stream).then(KeepaliveConnectionMsg::Closed);
                }
                self.transport = Some(HttpTransport::Tcp(stream));
                let bytes = self
                    .pending_connect_bytes
                    .take()
                    .expect("pending_connect_bytes set during cold-connect path");
                self.in_flight
                    .as_mut()
                    .expect("in_flight set during connect")
                    .pending_write = bytes;
                self.write_more()
            }
            KeepaliveConnectionMsg::Connected(Err(_)) => {
                self.fail_request(HttpClientError::Connect, true)
            }

            KeepaliveConnectionMsg::TlsConnected(Ok(stream)) => {
                if self.in_flight.is_none() {
                    return tls_close(stream, self.config.request_timeout)
                        .then(KeepaliveConnectionMsg::Closed);
                }
                self.transport = Some(HttpTransport::Tls(stream));
                let bytes = self
                    .pending_connect_bytes
                    .take()
                    .expect("pending_connect_bytes set during cold-connect path");
                self.in_flight
                    .as_mut()
                    .expect("in_flight set during connect")
                    .pending_write = bytes;
                self.write_more()
            }
            KeepaliveConnectionMsg::TlsConnected(Err(source)) => {
                let phase = HttpTransportPhase::Connect;
                self.fail_request(HttpClientError::Transport { phase, source }, true)
            }

            KeepaliveConnectionMsg::Wrote(Ok(count)) => self.handle_wrote(count),
            KeepaliveConnectionMsg::Wrote(Err(source)) => {
                let error = self.transport_or_flat_error(
                    HttpTransportPhase::Write,
                    source,
                    HttpClientError::Write,
                );
                self.fail_request(error, true)
            }

            KeepaliveConnectionMsg::Read(Ok(bytes)) => self.handle_bytes_read(bytes),
            KeepaliveConnectionMsg::Read(Err(source)) => {
                let error = self.transport_or_flat_error(
                    HttpTransportPhase::Read,
                    source,
                    HttpClientError::Read,
                );
                self.fail_request(error, true)
            }

            KeepaliveConnectionMsg::Deadline { generation, .. } => {
                // Stale deadline from a prior request: in_flight is
                // None or this generation does not match the current
                // request. Drop silently.
                let current_gen = self.in_flight.as_ref().map(|f| f.generation);
                if current_gen != Some(generation) {
                    return noop();
                }
                // Partial-state transport is no longer safe to
                // reuse; surface Timeout and the consumer treats it
                // like any other must_retire reply.
                self.fail_request(HttpClientError::Timeout, true)
            }

            KeepaliveConnectionMsg::Closed(_) => noop(),
        }
    }

    fn handle_call(
        &mut self,
        msg: KeepaliveConnectionMsg,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            KeepaliveConnectionMsg::Request {
                request,
                request_timeout,
            } => {
                let reply_to = call.into_request_context();
                self.handle_request(*request, request_timeout, Some(reply_to))
            }
            KeepaliveConnectionMsg::Stop => {
                let reply_to = call.into_request_context();
                self.handle_stop(Some(reply_to))
            }
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl<S: Shard + 'static> KeepaliveConnection<S> {
    fn handle_request(
        &mut self,
        request: HttpRequest,
        request_timeout: Duration,
        reply_to: Option<RequestContext<KeepaliveOutcome>>,
    ) -> Effect<Self> {
        if self.in_flight.is_some() {
            return reply_keepalive_outcome(
                reply_to,
                KeepaliveOutcome::Request {
                    result: Err(HttpClientError::Busy),
                    // Busy is a programming error on the caller side, not
                    // a transport-health signal. The transport itself is
                    // fine; the pool slot is the thing that's busy.
                    must_retire: false,
                },
            );
        }
        let request = match apply_host_policy(request, &self.target) {
            Ok(request) => request,
            Err(error) => {
                return reply_keepalive_outcome(
                    reply_to,
                    KeepaliveOutcome::Request {
                        result: Err(error),
                        must_retire: false,
                    },
                );
            }
        };
        let request_bytes = encode_keepalive_request(&request);

        // Bound to u64::MAX overflow with a loud panic. 2^64 requests
        // on one connection isolate is unreachable in practice;
        // saturating would silently confuse the stale-deadline
        // bookkeeping (every overflow request would share generation
        // u64::MAX), so a panic is the safer failure mode.
        self.request_generation = self
            .request_generation
            .checked_add(1)
            .expect("KeepaliveConnection request_generation overflowed u64");
        let generation = self.request_generation;
        self.in_flight = Some(InFlight {
            generation,
            pending_write: Vec::new(),
            read_buf: Vec::new(),
            decoded_chunked_body: Vec::new(),
            chunked_decoder: None,
            chunked_raw_consumed: 0,
            parsed_head: None,
            head_len: 0,
            reply_to,
        });

        let deadline_effect: Effect<Self> = sleep(request_timeout)
            .then(move |result| KeepaliveConnectionMsg::Deadline { generation, result });

        if self.transport.is_some() {
            // Reuse path: skip connect, queue the write directly.
            self.in_flight
                .as_mut()
                .expect("in_flight just set")
                .pending_write = request_bytes;
            batch(vec![self.write_more(), deadline_effect])
        } else {
            // Cold path: connect first; the Connected continuation
            // installs the transport and starts writing.
            self.pending_connect_bytes = Some(request_bytes);
            let connect_effect: Effect<Self> = match &self.target {
                HttpTarget::Http { addr, .. } => {
                    tcp_connect(*addr).then(KeepaliveConnectionMsg::Connected)
                }
                HttpTarget::Https {
                    addr,
                    server_name,
                    trust_roots,
                    host: _,
                } => tls_connect(
                    *addr,
                    server_name.clone(),
                    trust_roots.root_certificates_der.clone(),
                    request_timeout,
                )
                .then(KeepaliveConnectionMsg::TlsConnected),
            };
            batch(vec![connect_effect, deadline_effect])
        }
    }

    fn handle_stop(&mut self, reply_to: Option<RequestContext<KeepaliveOutcome>>) -> Effect<Self> {
        // Drop any in-flight state; the consumer may have been
        // waiting on a Request reply, but Stop wins.
        let _ = self.in_flight.take();
        let _ = self.pending_connect_bytes.take();
        let close_effect = self.close_transport_fire_and_forget();
        let reply_effect: Effect<Self> =
            reply_keepalive_outcome(reply_to, KeepaliveOutcome::Stopped);
        let stop_effect: Effect<Self> = stop();
        let mut effects = Vec::with_capacity(3);
        if let Some(close) = close_effect {
            effects.push(close);
        }
        effects.push(reply_effect);
        effects.push(stop_effect);
        batch(effects)
    }

    fn transport_or_flat_error(
        &self,
        phase: HttpTransportPhase,
        source: CallError,
        flat: HttpClientError,
    ) -> HttpClientError {
        match self.transport {
            Some(HttpTransport::Tls(_)) => HttpClientError::Transport { phase, source },
            _ => flat,
        }
    }

    fn write_more(&mut self) -> Effect<Self> {
        let in_flight = self.in_flight.as_ref().expect("in_flight present");
        let transport = self.transport.expect("transport set before write");
        let bytes = in_flight.pending_write.clone();
        match transport {
            HttpTransport::Tcp(stream) => {
                tcp_write(stream, bytes).then(KeepaliveConnectionMsg::Wrote)
            }
            HttpTransport::Tls(stream) => tls_write(stream, bytes, self.config.request_timeout)
                .then(KeepaliveConnectionMsg::Wrote),
        }
    }

    fn handle_wrote(&mut self, count: usize) -> Effect<Self> {
        let Some(in_flight) = self.in_flight.as_mut() else {
            return noop();
        };
        if count == 0 {
            return self.fail_request(HttpClientError::Write, true);
        }
        if count >= in_flight.pending_write.len() {
            in_flight.pending_write.clear();
            self.read_more()
        } else {
            in_flight.pending_write.drain(..count);
            self.write_more()
        }
    }

    fn read_more(&mut self) -> Effect<Self> {
        let transport = self.transport.expect("transport set before read");
        match transport {
            HttpTransport::Tcp(stream) => {
                tcp_read(stream, READ_CHUNK).then(KeepaliveConnectionMsg::Read)
            }
            HttpTransport::Tls(stream) => tls_read(stream, READ_CHUNK, self.config.request_timeout)
                .then(KeepaliveConnectionMsg::Read),
        }
    }

    fn handle_bytes_read(&mut self, bytes: Vec<u8>) -> Effect<Self> {
        let Some(in_flight) = self.in_flight.as_mut() else {
            return noop();
        };
        if bytes.is_empty() {
            // Peer closed. If we already framed a response and the
            // body is complete, deliver success; otherwise it's a
            // truncation. Either way the transport is gone — must
            // retire.
            return if in_flight.parsed_head.is_some() && body_complete(in_flight) {
                self.deliver_success(true)
            } else {
                self.fail_request(HttpClientError::Closed, true)
            };
        }
        in_flight.read_buf.extend_from_slice(&bytes);

        if in_flight.parsed_head.is_none() {
            match parse_response_head(&in_flight.read_buf, &self.config.limits) {
                ResponseParseProgress::NeedMore => return self.read_more(),
                ResponseParseProgress::Complete { head, head_len } => {
                    if head.chunked {
                        in_flight.chunked_decoder =
                            Some(ChunkedDecoder::new(self.config.limits.max_body_bytes));
                    }
                    in_flight.parsed_head = Some(head);
                    in_flight.head_len = head_len;
                }
                ResponseParseProgress::Failed(error) => {
                    return self.fail_request(HttpClientError::Parse(error), true);
                }
            }
        }

        if in_flight
            .parsed_head
            .as_ref()
            .is_some_and(|head| head.chunked)
        {
            let decode_start = in_flight
                .head_len
                .saturating_add(in_flight.chunked_raw_consumed);
            let raw = if decode_start <= in_flight.read_buf.len() {
                &in_flight.read_buf[decode_start..]
            } else {
                &[]
            };
            let decoder = in_flight
                .chunked_decoder
                .as_mut()
                .expect("chunked decoder initialized with chunked head");
            match decoder.feed_all(raw, &mut in_flight.decoded_chunked_body) {
                (FeedAllResult::Complete, consumed) => {
                    in_flight.chunked_raw_consumed =
                        in_flight.chunked_raw_consumed.saturating_add(consumed);
                    self.deliver_success(true)
                }
                (FeedAllResult::Failed(error), consumed) => {
                    in_flight.chunked_raw_consumed =
                        in_flight.chunked_raw_consumed.saturating_add(consumed);
                    let client_error = match error {
                        crate::types::ChunkedError::BodyTooLarge => {
                            HttpClientError::Parse(crate::types::ResponseParseError::BodyTooLarge)
                        }
                        _ => HttpClientError::Parse(
                            crate::types::ResponseParseError::MalformedChunkedBody,
                        ),
                    };
                    self.fail_request(client_error, true)
                }
                (FeedAllResult::NeedMore, consumed) => {
                    in_flight.chunked_raw_consumed =
                        in_flight.chunked_raw_consumed.saturating_add(consumed);
                    self.read_more()
                }
            }
        } else if body_complete(in_flight) {
            // Honor the server's Connection header on the response.
            // `close` token anywhere in the value forbids reuse.
            let must_retire = response_says_close(in_flight);
            self.deliver_success(must_retire)
        } else {
            self.read_more()
        }
    }

    fn deliver_success(&mut self, must_retire: bool) -> Effect<Self> {
        let mut in_flight = self
            .in_flight
            .take()
            .expect("in_flight present at delivery");
        let head = in_flight.parsed_head.expect("head parsed before delivery");
        let body = if head.chunked {
            in_flight.decoded_chunked_body
        } else {
            let body_end = in_flight.head_len + head.content_length;
            in_flight.read_buf[in_flight.head_len..body_end].to_vec()
        };
        let response = HttpResponse {
            status: head.status,
            version: head.version,
            headers: head.headers,
            body: crate::HttpResponseBody::Buffered(body),
        };
        let reply_to = in_flight.reply_to.take();
        if must_retire {
            // Drop the transport now; the next request on this slot
            // will reconnect. Issue a fire-and-forget close so the
            // FD is released; the Closed continuation no-ops.
            let close_effect = self.close_transport_fire_and_forget();
            let reply_effect: Effect<Self> = reply_keepalive_outcome(
                reply_to,
                KeepaliveOutcome::Request {
                    result: Ok(response),
                    must_retire,
                },
            );
            match close_effect {
                Some(close) => batch(vec![reply_effect, close]),
                None => reply_effect,
            }
        } else {
            reply_keepalive_outcome(
                reply_to,
                KeepaliveOutcome::Request {
                    result: Ok(response),
                    must_retire,
                },
            )
        }
    }

    fn fail_request(&mut self, error: HttpClientError, must_retire: bool) -> Effect<Self> {
        // Take in_flight first so a duplicate continuation can't
        // re-fire delivery. Also clear pending_connect_bytes — if we
        // failed during the cold-connect path, the bytes are stale.
        let reply_to = self
            .in_flight
            .take()
            .and_then(|in_flight| in_flight.reply_to);
        let _ = self.pending_connect_bytes.take();
        let close_effect = if must_retire {
            self.close_transport_fire_and_forget()
        } else {
            None
        };
        let reply_effect: Effect<Self> = reply_keepalive_outcome(
            reply_to,
            KeepaliveOutcome::Request {
                result: Err(error),
                must_retire,
            },
        );
        match close_effect {
            Some(close) => batch(vec![reply_effect, close]),
            None => reply_effect,
        }
    }

    /// Drops the transport and issues a runtime close. Returns the
    /// effect for batching, or `None` if there was nothing to close.
    fn close_transport_fire_and_forget(&mut self) -> Option<Effect<Self>> {
        let transport = self.transport.take()?;
        let effect: Effect<Self> = match transport {
            HttpTransport::Tcp(stream) => {
                tcp_close_stream(stream).then(KeepaliveConnectionMsg::Closed)
            }
            HttpTransport::Tls(stream) => {
                tls_close(stream, self.config.request_timeout).then(KeepaliveConnectionMsg::Closed)
            }
        };
        Some(effect)
    }
}

fn body_complete(state: &InFlight) -> bool {
    let Some(head) = state.parsed_head.as_ref() else {
        return false;
    };
    let needed = state.head_len + head.content_length;
    state.read_buf.len() >= needed
}

fn reply_keepalive_outcome<S: Shard + 'static>(
    reply_to: Option<RequestContext<KeepaliveOutcome>>,
    outcome: KeepaliveOutcome,
) -> Effect<KeepaliveConnection<S>> {
    match reply_to {
        Some(request) => reply_to_request(request, outcome),
        None => reply(outcome),
    }
}

fn response_says_close(state: &InFlight) -> bool {
    let head = match state.parsed_head.as_ref() {
        Some(h) => h,
        None => return false,
    };
    if matches!(head.version, http::Version::HTTP_10) {
        // HTTP/1.0 default is close unless the response explicitly
        // says keep-alive.
        let keep_alive = head
            .headers
            .get(http::header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .map(|s| {
                s.split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("keep-alive"))
            })
            .unwrap_or(false);
        return !keep_alive;
    }
    head.headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")))
        .unwrap_or(false)
}

fn apply_host_policy(
    mut request: HttpRequest,
    target: &HttpTarget,
) -> Result<HttpRequest, HttpClientError> {
    if !is_valid_origin_form_request_target(&request.path) {
        return Err(HttpClientError::InvalidRequestTarget);
    }
    let policy_value: Option<&str> = match target {
        HttpTarget::Http { host: None, .. } => None,
        HttpTarget::Http { host: Some(v), .. } => Some(v.as_str()),
        HttpTarget::Https {
            server_name,
            host: HttpHostPolicy::UseServerName,
            ..
        } => Some(server_name.as_str()),
        HttpTarget::Https {
            host: HttpHostPolicy::Explicit(v),
            ..
        } => Some(v.as_str()),
    };
    let Some(policy_value) = policy_value else {
        return Ok(request);
    };
    if policy_value.is_empty() {
        return Err(HttpClientError::InvalidHostHeaderValue);
    }
    if request.headers.contains_key(HOST) {
        return Err(HttpClientError::DuplicateHostHeader);
    }
    let value =
        HeaderValue::from_str(policy_value).map_err(|_| HttpClientError::InvalidHostHeaderValue)?;
    request.headers.insert(HOST, value);
    Ok(request)
}

/// Type alias for the keepalive pool's resource handle.
pub type KeepaliveConnAddr = Address<KeepaliveConnectionMsg, KeepaliveOutcome>;

/// Bundle returned from [`build_keepalive_pool`]: the pool address
/// the consumer drives with [`tina_runtime::pool`] vocabulary, plus
/// the addresses of the underlying connection isolates.
///
/// Hold on to `connections` for shutdown — `WorkerPool` does not
/// own connection-isolate lifetime, and dropping the pool address
/// alone leaves the isolates running with their open transports.
/// Call [`KeepaliveConnectionMsg::Stop`] on each address after the
/// pool itself has closed (Drain or Force), or use
/// [`shutdown_keepalive_pool`] for the small host-side shutdown
/// helper/report.
#[derive(Debug, Clone)]
pub struct KeepalivePoolHandles {
    /// The pool address. Use with `WorkerPoolMsg::Acquire` etc.
    pub pool: Address<WorkerPoolMsg<KeepaliveConnAddr>, WorkerPoolReply<KeepaliveConnAddr>>,
    /// One address per pool slot. Call `Stop` on each on shutdown.
    pub connections: Vec<KeepaliveConnAddr>,
}

/// Host-side result for closing the pool's admission surface.
///
/// This is deliberately separate from per-connection `Stop` counts:
/// closing the [`WorkerPool`] prevents new leases, but it does not own
/// or stop the connection isolates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepalivePoolCloseOutcome {
    /// The pool accepted the close request and replied `Closed`.
    Closed,
    /// The close call timed out.
    TimedOut,
    /// The pool mailbox was full when the close call was attempted.
    MailboxFull,
    /// The pool address was already closed.
    AlreadyClosed,
    /// The pool rejected the close call.
    Rejected(tina::CallRejectedReason),
    /// The pool replied with a different `WorkerPoolReply` variant.
    UnexpectedReply,
}

/// Drain result before a keepalive shutdown helper stops connection
/// isolates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepalivePoolDrainOutcome {
    /// `CloseMode::Force` does not wait for leases to return.
    NotRequested,
    /// `CloseMode::Drain` observed no remaining pool leases.
    Drained,
    /// Admission did not close, so draining was not attempted.
    SkippedAdmissionNotClosed,
    /// The pool address was already closed before remaining leases
    /// could be observed. The helper will not stop connections under
    /// `Drain` without lease truth.
    PoolAlreadyClosed,
    /// The pressure report needed to prove drain completion was not
    /// available.
    PressureUnavailable,
    /// Drain deadline fired while leases remained.
    TimedOut { leased: usize },
}

/// Per-connection result when a shutdown helper asks a slot to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepaliveConnectionStopOutcome {
    /// The connection replied `KeepaliveOutcome::Stopped`.
    Stopped,
    /// The stop call timed out.
    TimedOut,
    /// The connection mailbox was full when the stop call was attempted.
    MailboxFull,
    /// The connection address was already closed.
    AlreadyClosed,
    /// The connection rejected the stop call.
    Rejected(tina::CallRejectedReason),
    /// The connection replied with a non-`Stopped` payload.
    UnexpectedReply,
}

/// Names one pool slot whose connection did not newly stop during
/// shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepaliveConnectionStopFailure {
    /// Index in [`KeepalivePoolHandles::connections`].
    pub index: usize,
    /// Terminal stop outcome for that slot.
    pub outcome: KeepaliveConnectionStopOutcome,
}

/// Boring shutdown accounting for a keepalive pool.
///
/// `requested` is the number of connection isolates that were asked
/// to stop. Every requested connection lands in exactly one terminal
/// bucket. If pool admission did not close, or `Drain` could not
/// observe all leases returned before the deadline, `requested` is
/// zero and the connections are left alone. The helper performs one
/// pool-close call and one stop call per requested connection; it does
/// not retry `Stop` or claim a graceful transport drain beyond the
/// checked `KeepaliveOutcome::Stopped` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepalivePoolShutdownReport {
    /// Admission close result for the pool itself.
    pub pool_close: KeepalivePoolCloseOutcome,
    /// Drain result before connection isolates were stopped.
    pub drain: KeepalivePoolDrainOutcome,
    /// Number of connection isolates asked to stop.
    pub requested: usize,
    /// Stop calls that replied `KeepaliveOutcome::Stopped`.
    pub stopped: usize,
    /// Stop calls that timed out.
    pub timed_out: usize,
    /// Stop calls that were rejected, mailbox-full, or replied with
    /// an unexpected payload.
    pub rejected: usize,
    /// Stop calls made after the connection address was already closed.
    pub already_closed: usize,
    /// Per-slot outcomes for connections that did not newly stop.
    pub connection_failures: Vec<KeepaliveConnectionStopFailure>,
}

impl KeepalivePoolShutdownReport {
    fn new(pool_close: KeepalivePoolCloseOutcome) -> Self {
        Self {
            pool_close,
            drain: KeepalivePoolDrainOutcome::NotRequested,
            requested: 0,
            stopped: 0,
            timed_out: 0,
            rejected: 0,
            already_closed: 0,
            connection_failures: Vec::new(),
        }
    }

    fn begin_connection_stops(&mut self, requested: usize) {
        self.requested = requested;
    }

    fn record_connection(&mut self, index: usize, outcome: KeepaliveConnectionStopOutcome) {
        match outcome {
            KeepaliveConnectionStopOutcome::Stopped => self.stopped += 1,
            KeepaliveConnectionStopOutcome::TimedOut => {
                self.timed_out += 1;
                self.connection_failures
                    .push(KeepaliveConnectionStopFailure { index, outcome });
            }
            KeepaliveConnectionStopOutcome::AlreadyClosed => {
                self.already_closed += 1;
                self.connection_failures
                    .push(KeepaliveConnectionStopFailure { index, outcome });
            }
            KeepaliveConnectionStopOutcome::MailboxFull
            | KeepaliveConnectionStopOutcome::Rejected(_)
            | KeepaliveConnectionStopOutcome::UnexpectedReply => {
                self.rejected += 1;
                self.connection_failures
                    .push(KeepaliveConnectionStopFailure { index, outcome });
            }
        }
    }
}

/// Builds and registers a keepalive pool plus its connection
/// isolates.
///
/// Pre-spawns `pool_config.capacity` [`KeepaliveConnection`] isolates
/// bound to `target`, then registers a [`WorkerPool`] over them.
/// Each connection isolate runs on the runtime's shard `S`. Returns
/// a [`KeepalivePoolHandles`] bundle so the caller can drive the
/// pool *and* explicitly stop the connection isolates on shutdown.
///
/// `connection_mailbox_capacity` sizes each connection isolate's
/// mailbox; one slot per outstanding request plus headroom for
/// continuations is plenty (default suggestion: 16).
///
/// `pool_mailbox_capacity` sizes the pool isolate's mailbox; size
/// to `>= max_waiters + expected burst`.
pub fn build_keepalive_pool<S>(
    runtime: &ThreadedRuntime<S, tina_runtime::DefaultThreadedMailboxFactory>,
    target: HttpTarget,
    client_config: HttpClientConfig,
    pool_config: PoolConfig,
    connection_mailbox_capacity: usize,
    pool_mailbox_capacity: usize,
) -> Result<KeepalivePoolHandles, ThreadedRuntimeError>
where
    S: Shard + Send + 'static,
{
    let mut connections: Vec<KeepaliveConnAddr> = Vec::with_capacity(pool_config.capacity);
    for _ in 0..pool_config.capacity {
        let conn = KeepaliveConnection::<S>::new(target.clone(), client_config);
        let address = runtime.register_with_capacity::<KeepaliveConnection<S>, Infallible>(
            conn,
            connection_mailbox_capacity,
        )?;
        connections.push(address);
    }
    let pool: WorkerPool<KeepaliveConnAddr, S> = WorkerPool::new(pool_config, connections.clone());
    let pool_address = runtime
        .register_with_capacity::<WorkerPool<KeepaliveConnAddr, S>, Infallible>(
            pool,
            pool_mailbox_capacity,
        )?;
    Ok(KeepalivePoolHandles {
        pool: pool_address,
        connections,
    })
}

/// Closes pool admission and stops every keepalive connection isolate
/// once the selected close mode permits it.
///
/// This is a small copied shutdown shape for callers that build pools
/// with [`build_keepalive_pool`]. It keeps the two truths separate:
/// the pool close only controls future admission, `Drain` waits for
/// outstanding leases to return, then each connection is stopped with
/// its own checked call outcome. It does not retry `Stop` and does
/// not claim a graceful close beyond the typed
/// `KeepaliveOutcome::Stopped` reply. If admission close fails, or
/// `Drain` cannot prove leases settled before the deadline, no
/// connection isolates are stopped.
pub fn shutdown_keepalive_pool<S>(
    runtime: &ThreadedRuntime<S, tina_runtime::DefaultThreadedMailboxFactory>,
    handles: &KeepalivePoolHandles,
    mode: CloseMode,
    timeout: Duration,
) -> Result<KeepalivePoolShutdownReport, ThreadedRuntimeError>
where
    S: Shard + Send + 'static,
{
    let pool_close =
        match runtime.call_blocking(handles.pool, WorkerPoolMsg::Close(mode), timeout)? {
            CallOutcome::Replied(WorkerPoolReply::Closed) => KeepalivePoolCloseOutcome::Closed,
            CallOutcome::Replied(_) => KeepalivePoolCloseOutcome::UnexpectedReply,
            CallOutcome::Timeout => KeepalivePoolCloseOutcome::TimedOut,
            CallOutcome::Full => KeepalivePoolCloseOutcome::MailboxFull,
            CallOutcome::Closed => KeepalivePoolCloseOutcome::AlreadyClosed,
            CallOutcome::Rejected(reason) => KeepalivePoolCloseOutcome::Rejected(reason),
        };

    let mut report = KeepalivePoolShutdownReport::new(pool_close);

    if !matches!(
        pool_close,
        KeepalivePoolCloseOutcome::Closed | KeepalivePoolCloseOutcome::AlreadyClosed
    ) {
        report.drain = KeepalivePoolDrainOutcome::SkippedAdmissionNotClosed;
        return Ok(report);
    }

    report.drain = match mode {
        CloseMode::Force => KeepalivePoolDrainOutcome::NotRequested,
        CloseMode::Drain if matches!(pool_close, KeepalivePoolCloseOutcome::AlreadyClosed) => {
            KeepalivePoolDrainOutcome::PoolAlreadyClosed
        }
        CloseMode::Drain => wait_keepalive_pool_drain(runtime, handles, timeout)?,
    };

    let can_stop_connections = match mode {
        CloseMode::Force => true,
        CloseMode::Drain => matches!(report.drain, KeepalivePoolDrainOutcome::Drained),
    };
    if !can_stop_connections {
        return Ok(report);
    }

    report.begin_connection_stops(handles.connections.len());

    for (index, conn) in handles.connections.iter().copied().enumerate() {
        let outcome = match runtime.call_blocking(conn, KeepaliveConnectionMsg::Stop, timeout)? {
            CallOutcome::Replied(KeepaliveOutcome::Stopped) => {
                KeepaliveConnectionStopOutcome::Stopped
            }
            CallOutcome::Replied(_) => KeepaliveConnectionStopOutcome::UnexpectedReply,
            CallOutcome::Timeout => KeepaliveConnectionStopOutcome::TimedOut,
            CallOutcome::Closed => KeepaliveConnectionStopOutcome::AlreadyClosed,
            CallOutcome::Full => KeepaliveConnectionStopOutcome::MailboxFull,
            CallOutcome::Rejected(reason) => KeepaliveConnectionStopOutcome::Rejected(reason),
        };
        report.record_connection(index, outcome);
    }

    Ok(report)
}

fn wait_keepalive_pool_drain<S>(
    runtime: &ThreadedRuntime<S, tina_runtime::DefaultThreadedMailboxFactory>,
    handles: &KeepalivePoolHandles,
    timeout: Duration,
) -> Result<KeepalivePoolDrainOutcome, ThreadedRuntimeError>
where
    S: Shard + Send + 'static,
{
    let deadline = Instant::now() + timeout;
    let mut last_leased = handles.connections.len();

    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(KeepalivePoolDrainOutcome::TimedOut {
                leased: last_leased,
            });
        };
        if remaining.is_zero() {
            return Ok(KeepalivePoolDrainOutcome::TimedOut {
                leased: last_leased,
            });
        }

        match runtime.call_blocking(handles.pool, WorkerPoolMsg::PressureReport, remaining)? {
            CallOutcome::Replied(WorkerPoolReply::Pressure(report)) => {
                last_leased = report.leased;
                if report.leased == 0 {
                    return Ok(KeepalivePoolDrainOutcome::Drained);
                }
            }
            CallOutcome::Replied(_) => return Ok(KeepalivePoolDrainOutcome::PressureUnavailable),
            CallOutcome::Timeout => {
                return Ok(KeepalivePoolDrainOutcome::TimedOut {
                    leased: last_leased,
                });
            }
            CallOutcome::Full | CallOutcome::Closed | CallOutcome::Rejected(_) => {
                return Ok(KeepalivePoolDrainOutcome::PressureUnavailable);
            }
        }

        let nap = remaining.min(Duration::from_millis(10));
        if !nap.is_zero() {
            thread::sleep(nap);
        }
    }
}
