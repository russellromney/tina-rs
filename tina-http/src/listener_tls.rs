//! Listener isolate for the native HTTPS/1.1 server.
//!
//! [`HttpsListener`] is the TLS analogue of [`crate::HttpListener`]. It
//! binds a TLS listener (cert chain + private key, DER-encoded), accepts
//! TLS streams, and spawns one [`crate::HttpConnection`] per accepted
//! TLS stream (with [`crate::HttpTransport::Tls`] as the transport).
//!
//! Startup is **call-shaped**: the user issues
//! `call(listener, HttpsListenerMsg::Start, deadline)` and receives
//! `Ok(HttpsReady { local_addr })` once the bind completes, or
//! `Err(HttpsStartupError::Bind { source })` if `tls_bind` failed. No
//! child connection isolate is spawned on bind failure; the runtime
//! does not park a TLS listener resource.
//!
//! After `Ready`, the accept loop is the same shape as the plain TCP
//! listener: one accept call in flight, spawn a connection on success,
//! re-accept; orphan-close on accept-after-stop.
//!
//! Cert/key inputs are **explicit DER**. There are no PEM defaults and
//! no system roots. PEM helpers, where added, live in test code only.

use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, TlsListenerId, TlsStreamId, tls_accept, tls_bind, tls_close, tls_close_listener,
};

use crate::connection::{HttpConnection, HttpConnectionMsg};
use crate::transport::HttpTransport;
use crate::types::{HttpLimits, HttpRequest, HttpResponse, HttpServerConfig};

/// DER-encoded TLS server identity. The runtime hands these to
/// `rustls::ServerConfig::with_single_cert`. Inputs are explicit:
/// the caller supplies a certificate chain (leaf first, then any
/// intermediates) and a PKCS#8 private key.
#[derive(Debug, Clone)]
pub struct TlsServerIdentity {
    /// DER-encoded certificate chain. The leaf certificate is first,
    /// followed by any intermediates the server should present. The
    /// root is not included.
    pub certificate_chain_der: Vec<Vec<u8>>,
    /// DER-encoded PKCS#8 private key matching the leaf certificate.
    pub private_key_der: Vec<u8>,
}

impl TlsServerIdentity {
    /// Builds a `TlsServerIdentity` from DER bytes. Convenience over
    /// the public fields so call sites read `from_der(...)` rather
    /// than spelling the struct out.
    pub fn from_der(certificate_chain: Vec<Vec<u8>>, private_key: Vec<u8>) -> Self {
        Self {
            certificate_chain_der: certificate_chain,
            private_key_der: private_key,
        }
    }
}

/// Server-side knobs for a native HTTPS listener.
///
/// Composes [`HttpServerConfig`] with a [`TlsServerIdentity`] and two
/// distinct TLS lane deadlines:
///
/// - `tls_accept_timeout` is the per-call deadline on `tls_accept`. The
///   runtime's TLS lane is single-threaded, so an in-flight `tls_accept`
///   (which busy-polls for a new connection plus drives the TLS
///   handshake) blocks every other TLS op on that lane — including a
///   live connection's `tls_read`/`tls_write`. A short
///   `tls_accept_timeout` (e.g. 250ms) makes the listener re-issue
///   `tls_accept` every quarter-second, freeing the worker between
///   polls so connection reads/writes can drain. The listener treats
///   `Timeout` on accept as "no connection in this slice; re-poll".
/// - `tls_io_timeout` is the per-call deadline on `tls_read`,
///   `tls_write`, and `tls_close` for an accepted connection. It
///   bounds how long a single lane operation may stall.
///
/// First form is **not** designed for high HTTPS concurrency: the
/// runtime serialises every TLS op (accept, handshake, read, write,
/// close) onto one worker thread, so observed throughput is one TLS
/// op at a time. Each connection's per-op latency includes any
/// in-flight `tls_accept` slice. Production HTTPS performance is
/// explicitly out of scope.
#[derive(Debug, Clone)]
pub struct HttpsServerConfig {
    /// Plain-HTTP knobs (limits, service-call timeout, mailbox sizes).
    pub http: HttpServerConfig,
    /// DER cert chain + private key.
    pub identity: TlsServerIdentity,
    /// Per-call deadline on `tls_accept`. Short on purpose; see struct
    /// docs.
    pub tls_accept_timeout: Duration,
    /// Per-call deadline on `tls_read`, `tls_write`, `tls_close`.
    pub tls_io_timeout: Duration,
}

impl HttpsServerConfig {
    /// Builds a config from an identity with development-preset HTTP
    /// knobs, a 250ms accept timeout, and a 30s I/O timeout.
    pub fn dev(identity: TlsServerIdentity) -> Self {
        Self {
            http: HttpServerConfig::dev(),
            identity,
            tls_accept_timeout: Duration::from_millis(250),
            tls_io_timeout: Duration::from_secs(30),
        }
    }

    /// Builds a config with pressure-preset HTTP knobs, a 100ms
    /// accept timeout, and a 1s I/O timeout.
    pub fn pressure(identity: TlsServerIdentity) -> Self {
        Self {
            http: HttpServerConfig::pressure(),
            identity,
            tls_accept_timeout: Duration::from_millis(100),
            tls_io_timeout: Duration::from_secs(1),
        }
    }
}

/// Reply variant: HTTPS listener is bound and accepting TLS streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpsReady {
    /// Actual bound socket address (with the kernel-assigned port if
    /// the caller bound `:0`).
    pub local_addr: SocketAddr,
}

/// Reply variant: HTTPS listener could not start. The listener isolate
/// stops without spawning any child or holding a TLS resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpsStartupError {
    /// `tls_bind` failed. `source` carries the typed runtime error
    /// (`TlsCertificate` for invalid cert/key, `Io` for socket
    /// problems, `TlsFull` for lane overload, etc.).
    Bind { source: CallError },
}

/// Inbound message variants for [`HttpsListener`].
///
/// Only `Start` and `Stop` are sent by user code. The rest are runtime
/// continuations produced by the listener's own `tls_*` calls.
#[derive(Debug, Clone)]
pub enum HttpsListenerMsg {
    /// Kick off the bind. Sent once by the host via
    /// `call(listener, Start, deadline).reply(...)`. The reply lands
    /// when bind either succeeds (`Ok(HttpsReady)`) or fails
    /// (`Err(HttpsStartupError::Bind)`).
    Start,
    /// `tls_bind` reply.
    Bound(Result<(TlsListenerId, SocketAddr), CallError>),
    /// `tls_accept` reply.
    Accepted(Result<(TlsStreamId, SocketAddr), CallError>),
    /// Request to stop accepting and close the listener. Already-spawned
    /// connection isolates run to completion through normal cleanup.
    Stop,
    /// `tls_close_listener` reply.
    ListenerClosed(Result<(), CallError>),
    /// `tls_close` reply for an orphan stream the listener decided to
    /// drop (kernel-already-accepted connection arriving after `Stop`).
    StreamClosed(Result<(), CallError>),
}

/// HTTPS listener isolate.
///
/// Generic over the user's `Shard` and the service's message type
/// `M` (default [`HttpRequest`]).
pub struct HttpsListener<S: Shard + 'static, M: From<HttpRequest> + Send + 'static = HttpRequest> {
    bind_addr: SocketAddr,
    service: Address<M, HttpResponse>,
    limits: HttpLimits,
    service_call_timeout: Duration,
    connection_mailbox_capacity: usize,
    identity: TlsServerIdentity,
    tls_accept_timeout: Duration,
    tls_io_timeout: Duration,
    listener: Option<TlsListenerId>,
    stopping: bool,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> HttpsListener<S, M> {
    /// Builds an HTTPS listener that will bind to `bind_addr`,
    /// dispatch every parsed request to `service`, and spawn one
    /// [`HttpConnection`] per accepted TLS stream.
    pub fn new(
        bind_addr: SocketAddr,
        service: Address<M, HttpResponse>,
        config: HttpsServerConfig,
    ) -> Self {
        Self {
            bind_addr,
            service,
            limits: config.http.limits,
            service_call_timeout: config.http.service_call_timeout,
            connection_mailbox_capacity: config.http.connection_mailbox_capacity,
            identity: config.identity,
            tls_accept_timeout: config.tls_accept_timeout,
            tls_io_timeout: config.tls_io_timeout,
            listener: None,
            stopping: false,
            _shard: PhantomData,
        }
    }
}

impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> Isolate for HttpsListener<S, M> {
    tina::isolate_types! {
        message: HttpsListenerMsg,
        reply: Result<HttpsReady, HttpsStartupError>,
        send: tina::Outbound<std::convert::Infallible>,
        spawn: ChildDefinition<HttpConnection<S, M>>,
        call: tina_runtime::RuntimeCall<HttpsListenerMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: HttpsListenerMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            HttpsListenerMsg::Start => tls_bind(
                self.bind_addr,
                self.identity.certificate_chain_der.clone(),
                self.identity.private_key_der.clone(),
            )
            .reply(HttpsListenerMsg::Bound),

            HttpsListenerMsg::Bound(Ok((listener, local_addr))) => {
                self.listener = Some(listener);
                let ready_effect: Effect<Self> = reply(Ok(HttpsReady { local_addr }));
                let accept_effect: Effect<Self> = tls_accept(listener, self.tls_accept_timeout)
                    .reply(HttpsListenerMsg::Accepted);
                batch(vec![ready_effect, accept_effect])
            }
            HttpsListenerMsg::Bound(Err(source)) => {
                // Reply Err and stop. No child has been spawned, no
                // TlsListenerId is held — nothing to clean up.
                batch(vec![
                    reply(Err(HttpsStartupError::Bind { source })),
                    stop(),
                ])
            }

            HttpsListenerMsg::Accepted(Ok((stream, _peer))) => {
                if self.stopping {
                    // Stop already ran; close the orphan stream and do
                    // not re-issue accept.
                    return tls_close(stream, self.tls_io_timeout)
                        .reply(HttpsListenerMsg::StreamClosed);
                }
                let listener = self.listener.expect("listener set after bind");
                let child = self.build_connection_child(stream);
                batch(vec![
                    spawn(child),
                    tls_accept(listener, self.tls_accept_timeout)
                        .reply(HttpsListenerMsg::Accepted),
                ])
            }
            HttpsListenerMsg::Accepted(Err(_)) => {
                // Accept failed (likely listener was closed or handshake
                // failed). On handshake failure no `TlsStreamId` was
                // allocated by the runtime — see `accept_tls` in
                // tina-runtime — so we issue a fresh accept rather than
                // tearing down. Mirrors the TCP listener's policy of
                // surviving transient accept failures.
                if self.stopping {
                    if let Some(listener) = self.listener.take() {
                        return tls_close_listener(listener)
                            .reply(HttpsListenerMsg::ListenerClosed);
                    }
                    return stop();
                }
                let Some(listener) = self.listener else {
                    return stop();
                };
                tls_accept(listener, self.tls_accept_timeout).reply(HttpsListenerMsg::Accepted)
            }

            HttpsListenerMsg::Stop => {
                self.stopping = true;
                if let Some(listener) = self.listener.take() {
                    tls_close_listener(listener).reply(HttpsListenerMsg::ListenerClosed)
                } else {
                    stop()
                }
            }

            HttpsListenerMsg::ListenerClosed(_) => stop(),
            HttpsListenerMsg::StreamClosed(_) => noop(),
        }
    }
}

impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> HttpsListener<S, M> {
    fn build_connection_child(
        &self,
        stream: TlsStreamId,
    ) -> ChildDefinition<HttpConnection<S, M>> {
        ChildDefinition::new(
            HttpConnection::<S, M>::with_transport(
                HttpTransport::Tls(stream),
                self.service,
                self.limits,
                self.service_call_timeout,
                self.tls_io_timeout,
            ),
            self.connection_mailbox_capacity,
        )
        .with_initial_message(HttpConnectionMsg::Begin)
    }
}
