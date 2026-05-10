//! HTTP/1.1 keepalive connection isolate and origin key.
//!
//! [`KeepaliveConnection`] owns one TCP or TLS transport across many
//! requests. Unlike [`crate::HttpClient`], which connects + sends +
//! reads + closes per call, this isolate connects on first use and
//! reuses the same transport until the consumer asks it to
//! [`KeepaliveCall::Reset`] or the peer closes.
//!
//! # Pool consumer pattern
//!
//! A keepalive pool is a [`tina_runtime::pool::WorkerPool`] over a
//! fixed list of `Address<KeepaliveConnectionMsg, KeepaliveOutcome>`
//! handles. Build one with [`build_keepalive_pool`].
//!
//! ```text
//! acquire lease (Address<KeepaliveConnectionMsg, KeepaliveOutcome>)
//! call(*lease.handle(), KeepaliveCall::Request(req), deadline_remaining)
//!   -> KeepaliveOutcome { result, must_retire }
//! if outcome.must_retire {
//!     call(*lease.handle(), KeepaliveCall::Reset, t).reply(...)
//!     release(lease, Retire)
//! } else {
//!     release(lease, Reuse)
//! }
//! ```
//!
//! The consumer is responsible for honouring `must_retire`. The pool
//! does not see the response and cannot override `Reuse` to `Retire`
//! by itself; the connection isolate is the source of truth and signals
//! retirement explicitly through its reply.
//!
//! # Origin keying
//!
//! [`OriginKey`] captures the identity that two requests must share to
//! safely reuse a connection: scheme + `SocketAddr` + (for HTTPS)
//! server name + a fingerprint of the configured trust roots. A
//! pool's connections are pre-bound to one [`HttpTarget`] at registration,
//! so cross-origin reuse is impossible by construction — the only way
//! to send to a different origin is to register a different pool.
//!
//! # Out of scope (first form)
//!
//! - No idle-connection timeout. Connections sit `Disconnected` after
//!   `Reset` and reconnect on next use. A long-idle stale socket is
//!   discovered on the next request via a write/read failure and
//!   reported as `must_retire`.
//! - No hidden retry. A `must_retire` reply is the consumer's signal
//!   to choose whether to acquire another lease and retry, or surface
//!   the failure.
//! - No HTTP/2, no chunked transfer, no expect-100-continue. Same
//!   contract as [`crate::HttpClient`].

use std::collections::hash_map::DefaultHasher;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;

use http::HeaderValue;
use http::header::HOST;
use tina::pool::PoolConfig;
use tina::prelude::*;
use tina_runtime::pool::{WorkerPool, WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallError, ThreadedRuntime, ThreadedRuntimeError, sleep, tcp_close_stream, tcp_connect,
    tcp_read, tcp_write, tls_close, tls_connect, tls_read, tls_write,
};

use crate::parse::{HttpResponseHead, ResponseParseProgress, encode_request, parse_response_head};
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
/// HTTPS variants fold the configured DER trust roots into a
/// `trust_fingerprint` so two configs with different roots never
/// collide, even when their server name and address agree.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OriginKey {
    /// Plain HTTP keyed by socket address.
    Http {
        /// Peer socket address. The keepalive contract is "same TCP
        /// peer"; two different IPs that happen to host the same site
        /// must not share a connection.
        addr: SocketAddr,
    },
    /// HTTPS keyed by socket address, SNI server name, and a
    /// fingerprint of the trust roots.
    Https {
        /// Peer socket address.
        addr: SocketAddr,
        /// SNI / certificate verification name used at handshake.
        server_name: String,
        /// Stable hash of the configured DER trust roots. Two
        /// configs whose roots differ produce different fingerprints
        /// and never share connections.
        trust_fingerprint: u64,
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
            } => {
                let mut hasher = DefaultHasher::new();
                trust_roots.root_certificates_der.len().hash(&mut hasher);
                for der in &trust_roots.root_certificates_der {
                    der.hash(&mut hasher);
                }
                Self::Https {
                    addr: *addr,
                    server_name: server_name.clone(),
                    trust_fingerprint: hasher.finish(),
                }
            }
        }
    }
}

/// Inbound message variants for [`KeepaliveConnection`].
///
/// `Request` and `Reset` are user-callable. The remaining variants are
/// continuations from runtime calls the connection issues itself.
#[derive(Debug, Clone)]
pub enum KeepaliveConnectionMsg {
    /// Run one request on this connection. The connection connects on
    /// first use, then reuses the transport until [`Self::Reset`] or
    /// the peer closes.
    ///
    /// `request_timeout` is the wall-clock budget for the entire
    /// request — connect (if needed), write, read head, read body.
    /// Pass `Deadline::remaining_or_zero(now)` from a caller-owned
    /// deadline to propagate budgets honestly across hops.
    Request {
        request: Box<HttpRequest>,
        request_timeout: Duration,
    },
    /// Drop the underlying transport. Replies once close completes.
    /// The consumer sends this before releasing a lease with
    /// [`tina::pool::ReleaseDisposition::Retire`] so the OS socket is
    /// freed promptly and a stale connection cannot be served to the
    /// next caller.
    Reset,

    Connected(Result<(tina_runtime::StreamId, SocketAddr, SocketAddr), CallError>),
    TlsConnected(Result<tina_runtime::TlsStreamId, CallError>),
    Wrote(Result<usize, CallError>),
    Read(Result<Vec<u8>, CallError>),
    Closed(Result<(), CallError>),
    /// `generation` distinguishes the deadline for *this* request from
    /// stale deadlines scheduled by prior requests on the same
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
///
/// `Request` calls reply with a [`KeepaliveOutcome`]; `Reset` calls
/// reply with [`KeepaliveOutcome::Reset`]. One reply enum keeps the
/// runtime call-shape uniform.
#[derive(Debug, Clone)]
pub enum KeepaliveOutcome {
    /// One request finished. `must_retire` is true when the connection
    /// is no longer safe to reuse: error during connect/write/read,
    /// the server returned `Connection: close`, or the peer closed
    /// before the body completed.
    Request {
        result: Result<HttpResponse, HttpClientError>,
        must_retire: bool,
    },
    /// `Reset` completed (or there was nothing to close).
    Reset,
}

impl KeepaliveOutcome {
    /// Convenience: returns the request result if this is a `Request`
    /// reply; panics otherwise. Tests use this; consumers should match
    /// the variant explicitly.
    pub fn into_request_result(self) -> (Result<HttpResponse, HttpClientError>, bool) {
        match self {
            Self::Request {
                result,
                must_retire,
            } => (result, must_retire),
            Self::Reset => panic!("expected Request outcome, got Reset"),
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
    /// Monotonic per-request counter. Bumped at the start of every
    /// `handle_request`. Continuations that target a specific request
    /// (today: `Deadline`) carry the generation in their payload; a
    /// generation mismatch flags the message as stale and a no-op.
    request_generation: u64,
    /// Set true while a `Reset` is in flight so the `Closed`
    /// continuation knows to reply to the caller (not silently drop).
    resetting: bool,
    _shard: PhantomData<S>,
}

struct InFlight {
    /// Generation stamped at request start; used to ignore stale
    /// `Deadline` continuations from prior requests.
    generation: u64,
    request_bytes: Vec<u8>,
    pending_write: Vec<u8>,
    read_buf: Vec<u8>,
    parsed_head: Option<HttpResponseHead>,
    head_len: usize,
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
            request_generation: 0,
            resetting: false,
            _shard: PhantomData,
        }
    }

    /// Origin this connection is bound to. Inspect to verify the
    /// pool's slot serves the origin you expect.
    pub fn origin(&self) -> &OriginKey {
        &self.origin
    }
}

// Hand-rolled `Isolate`: macro requires a concrete shard, we want
// `KeepaliveConnection` to work with any user shard.
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
            } => self.handle_request(*request, request_timeout),

            KeepaliveConnectionMsg::Reset => self.handle_reset(),

            KeepaliveConnectionMsg::Connected(Ok((stream, _local, _peer))) => {
                if self.in_flight.is_none() {
                    return tcp_close_stream(stream).reply(KeepaliveConnectionMsg::Closed);
                }
                self.transport = Some(HttpTransport::Tcp(stream));
                let bytes = self
                    .in_flight
                    .as_ref()
                    .expect("in_flight set during connect")
                    .request_bytes
                    .clone();
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
                        .reply(KeepaliveConnectionMsg::Closed);
                }
                self.transport = Some(HttpTransport::Tls(stream));
                let bytes = self
                    .in_flight
                    .as_ref()
                    .expect("in_flight set during connect")
                    .request_bytes
                    .clone();
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
                // Stale deadline from a prior request (in_flight is
                // None or this generation does not match the current
                // request). Drop silently.
                let current_gen = self.in_flight.as_ref().map(|f| f.generation);
                if current_gen != Some(generation) {
                    return noop();
                }
                // Partial-state transport is no longer safe to reuse;
                // surface Timeout and the caller should Retire.
                self.fail_request(HttpClientError::Timeout, true)
            }

            KeepaliveConnectionMsg::Closed(_) => {
                // Two reasons we'd see Closed:
                // - Resetting: reply Reset to the consumer.
                // - Background close after a stale post-Connected
                //   wakeup or a finished request that elected to
                //   close: nothing to do.
                if self.resetting {
                    self.resetting = false;
                    self.transport = None;
                    return reply(KeepaliveOutcome::Reset);
                }
                noop()
            }
        }
    }
}

impl<S: Shard + 'static> KeepaliveConnection<S> {
    fn handle_request(&mut self, request: HttpRequest, request_timeout: Duration) -> Effect<Self> {
        if self.in_flight.is_some() {
            return reply(KeepaliveOutcome::Request {
                result: Err(HttpClientError::Busy),
                // Busy is a programming error on the caller side, not
                // a transport-health signal. The transport itself is
                // fine; the pool slot is the thing that's busy.
                must_retire: false,
            });
        }
        if self.resetting {
            return reply(KeepaliveOutcome::Request {
                result: Err(HttpClientError::Busy),
                must_retire: false,
            });
        }
        let request = match apply_host_policy(request, &self.target) {
            Ok(request) => request,
            Err(error) => {
                return reply(KeepaliveOutcome::Request {
                    result: Err(error),
                    must_retire: false,
                });
            }
        };
        // Keepalive: omit `Connection: close`. Server then defaults
        // to keep-alive on HTTP/1.1, leaving the socket reusable.
        let request_bytes = encode_request(&request, false);

        self.request_generation = self.request_generation.saturating_add(1);
        let generation = self.request_generation;
        self.in_flight = Some(InFlight {
            generation,
            request_bytes: request_bytes.clone(),
            pending_write: Vec::new(),
            read_buf: Vec::new(),
            parsed_head: None,
            head_len: 0,
        });

        let deadline_effect: Effect<Self> = sleep(request_timeout)
            .reply(move |result| KeepaliveConnectionMsg::Deadline { generation, result });

        if let Some(transport) = self.transport {
            // Reuse path: skip connect, queue the write directly.
            self.in_flight
                .as_mut()
                .expect("in_flight just set")
                .pending_write = request_bytes;
            let _ = transport;
            batch(vec![self.write_more(), deadline_effect])
        } else {
            // Cold path: connect first; the Connected continuation
            // installs the transport and starts writing.
            let connect_effect: Effect<Self> = match &self.target {
                HttpTarget::Http { addr, .. } => {
                    tcp_connect(*addr).reply(KeepaliveConnectionMsg::Connected)
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
                .reply(KeepaliveConnectionMsg::TlsConnected),
            };
            batch(vec![connect_effect, deadline_effect])
        }
    }

    fn handle_reset(&mut self) -> Effect<Self> {
        // Reset never runs concurrently with a Request: the consumer
        // serialises (await Request reply, then send Reset, await
        // Reset reply, then release with Retire). Defensive: if a
        // request is somehow in flight, fail it before resetting.
        if self.in_flight.is_some() {
            // Drop the in-flight state; we won't reply for it because
            // the consumer is asking us to reset, which means they've
            // already received the Request reply (or aren't waiting).
            self.in_flight = None;
        }
        let Some(transport) = self.transport.take() else {
            return reply(KeepaliveOutcome::Reset);
        };
        self.resetting = true;
        match transport {
            HttpTransport::Tcp(stream) => {
                tcp_close_stream(stream).reply(KeepaliveConnectionMsg::Closed)
            }
            HttpTransport::Tls(stream) => {
                tls_close(stream, self.config.request_timeout).reply(KeepaliveConnectionMsg::Closed)
            }
        }
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
                tcp_write(stream, bytes).reply(KeepaliveConnectionMsg::Wrote)
            }
            HttpTransport::Tls(stream) => tls_write(stream, bytes, self.config.request_timeout)
                .reply(KeepaliveConnectionMsg::Wrote),
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
                tcp_read(stream, READ_CHUNK).reply(KeepaliveConnectionMsg::Read)
            }
            HttpTransport::Tls(stream) => tls_read(stream, READ_CHUNK, self.config.request_timeout)
                .reply(KeepaliveConnectionMsg::Read),
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
                    in_flight.parsed_head = Some(head);
                    in_flight.head_len = head_len;
                }
                ResponseParseProgress::Failed(error) => {
                    return self.fail_request(HttpClientError::Parse(error), true);
                }
            }
        }

        if body_complete(in_flight) {
            // Honor the server's Connection header on the response.
            // `close` token anywhere in the value forbids reuse.
            let must_retire = response_says_close(in_flight);
            self.deliver_success(must_retire)
        } else {
            self.read_more()
        }
    }

    fn deliver_success(&mut self, must_retire: bool) -> Effect<Self> {
        let in_flight = self.in_flight.take().expect("in_flight present at delivery");
        let head = in_flight
            .parsed_head
            .expect("head parsed before delivery");
        let body_end = in_flight.head_len + head.content_length;
        let body = in_flight.read_buf[in_flight.head_len..body_end].to_vec();
        let response = HttpResponse {
            status: head.status,
            version: head.version,
            headers: head.headers,
            body: crate::HttpResponseBody::Buffered(body),
        };
        if must_retire {
            // Drop the transport now; the next request on this slot
            // will reconnect. Issue a fire-and-forget close so the FD
            // is released; the Closed continuation no-ops.
            let close_effect = self.close_transport_fire_and_forget();
            let reply_effect: Effect<Self> = reply(KeepaliveOutcome::Request {
                result: Ok(response),
                must_retire,
            });
            match close_effect {
                Some(close) => batch(vec![reply_effect, close]),
                None => reply_effect,
            }
        } else {
            reply(KeepaliveOutcome::Request {
                result: Ok(response),
                must_retire,
            })
        }
    }

    fn fail_request(&mut self, error: HttpClientError, must_retire: bool) -> Effect<Self> {
        // Take in_flight first so a duplicate continuation can't
        // re-fire delivery.
        let _ = self.in_flight.take();
        let close_effect = if must_retire {
            self.close_transport_fire_and_forget()
        } else {
            None
        };
        let reply_effect: Effect<Self> = reply(KeepaliveOutcome::Request {
            result: Err(error),
            must_retire,
        });
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
                tcp_close_stream(stream).reply(KeepaliveConnectionMsg::Closed)
            }
            HttpTransport::Tls(stream) => {
                tls_close(stream, self.config.request_timeout).reply(KeepaliveConnectionMsg::Closed)
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

/// Type alias for the keepalive pool isolate's address.
pub type KeepaliveWorkerPool = WorkerPool<KeepaliveConnAddr, ()>;

/// Type alias for the keepalive pool's message and reply types.
pub type KeepaliveWorkerPoolMsg = WorkerPoolMsg<KeepaliveConnAddr>;
pub type KeepaliveWorkerPoolReply = WorkerPoolReply<KeepaliveConnAddr>;

/// Builds and registers a keepalive pool plus its connection isolates.
///
/// Pre-spawns `pool_config.capacity` [`KeepaliveConnection`] isolates
/// bound to `target`, then registers a [`WorkerPool`] over them. Each
/// connection isolate runs on the runtime's shard `S`. Returns the
/// pool's address; the consumer drives it with the
/// [`tina_runtime::pool`] vocabulary.
///
/// `connection_mailbox_capacity` sizes each connection isolate's
/// mailbox; one slot per outstanding request plus headroom for
/// continuations is plenty (default suggestion: 16).
///
/// `pool_mailbox_capacity` sizes the pool isolate's mailbox; size to
/// `>= max_waiters + expected burst`.
pub fn build_keepalive_pool<S>(
    runtime: &ThreadedRuntime<S, tina_runtime::DefaultThreadedMailboxFactory>,
    target: HttpTarget,
    client_config: HttpClientConfig,
    pool_config: PoolConfig,
    connection_mailbox_capacity: usize,
    pool_mailbox_capacity: usize,
) -> Result<Address<WorkerPoolMsg<KeepaliveConnAddr>, WorkerPoolReply<KeepaliveConnAddr>>, ThreadedRuntimeError>
where
    S: Shard + Send + 'static,
{
    let mut handles: Vec<KeepaliveConnAddr> = Vec::with_capacity(pool_config.capacity);
    for _ in 0..pool_config.capacity {
        let conn = KeepaliveConnection::<S>::new(target.clone(), client_config);
        let address = runtime.register_with_capacity::<KeepaliveConnection<S>, Infallible>(
            conn,
            connection_mailbox_capacity,
        )?;
        handles.push(address);
    }
    let pool: WorkerPool<KeepaliveConnAddr, S> = WorkerPool::new(pool_config, handles);
    runtime.register_with_capacity::<WorkerPool<KeepaliveConnAddr, S>, Infallible>(
        pool,
        pool_mailbox_capacity,
    )
}
