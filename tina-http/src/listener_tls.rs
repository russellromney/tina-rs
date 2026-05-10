//! HTTPS/1.1 listener over the runtime TLS lane. Startup is
//! call-shaped: `call(listener, Start, t)` returns
//! `Result<HttpsReady, HttpsStartupError>`. After `Ready`, the
//! accept loop spawns one [`crate::HttpConnection`] per accepted TLS
//! stream. Cert/key inputs are explicit DER — no PEM defaults, no
//! system roots.

use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, TlsListenerId, TlsStreamId, tls_accept, tls_bind, tls_close, tls_close_listener,
};

use crate::body_metrics::BodyMetrics;
use crate::connection::{HttpConnection, HttpConnectionMsg};
use crate::transport::HttpTransport;
use crate::types::{HttpLimits, HttpRequest, HttpResponse, HttpServerConfig};

/// DER cert chain (leaf first) + PKCS#8 private key. Fed to
/// `rustls::ServerConfig::with_single_cert`.
#[derive(Debug, Clone)]
pub struct TlsServerIdentity {
    pub certificate_chain_der: Vec<Vec<u8>>,
    pub private_key_der: Vec<u8>,
}

impl TlsServerIdentity {
    pub fn from_der(certificate_chain: Vec<Vec<u8>>, private_key: Vec<u8>) -> Self {
        Self {
            certificate_chain_der: certificate_chain,
            private_key_der: private_key,
        }
    }
}

/// HTTPS listener config. Two distinct TLS deadlines because the
/// runtime's TLS lane has one worker thread per shard: a long
/// in-flight `tls_accept` blocks live connections' reads/writes.
///
/// `tls_accept_timeout` is short (default 250ms) so the listener
/// re-polls and the worker yields between accepts. `tls_io_timeout`
/// bounds individual `tls_read`/`tls_write`/`tls_close` ops. First
/// form is not designed for high HTTPS concurrency.
#[derive(Debug, Clone)]
pub struct HttpsServerConfig {
    pub http: HttpServerConfig,
    pub identity: TlsServerIdentity,
    pub tls_accept_timeout: Duration,
    pub tls_io_timeout: Duration,
}

impl HttpsServerConfig {
    /// Dev preset: 250ms accept, 30s I/O.
    pub fn dev(identity: TlsServerIdentity) -> Self {
        Self {
            http: HttpServerConfig::dev(),
            identity,
            tls_accept_timeout: Duration::from_millis(250),
            tls_io_timeout: Duration::from_secs(30),
        }
    }

    /// Pressure preset: 100ms accept, 1s I/O.
    pub fn pressure(identity: TlsServerIdentity) -> Self {
        Self {
            http: HttpServerConfig::pressure(),
            identity,
            tls_accept_timeout: Duration::from_millis(100),
            tls_io_timeout: Duration::from_secs(1),
        }
    }
}

/// HTTPS listener is bound. `local_addr` carries the kernel-assigned
/// port when the caller bound `:0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpsReady {
    pub local_addr: SocketAddr,
}

/// HTTPS listener could not start. No child spawned, no TLS resource
/// held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpsStartupError {
    /// `tls_bind` failed. `source` is the typed runtime error
    /// (`TlsCertificate`, `Io`, `TlsFull`, ...).
    Bind { source: CallError },
    /// Second `Start` on the same listener. The first bind still
    /// owns the listener — no second bind happens.
    AlreadyStarted,
}

/// User code sends `Start` and `Stop`; the rest are continuations
/// from the listener's own `tls_*` calls.
#[derive(Debug, Clone)]
pub enum HttpsListenerMsg {
    Start,
    Bound(Result<(TlsListenerId, SocketAddr), CallError>),
    Accepted(Result<(TlsStreamId, SocketAddr), CallError>),
    /// Stop accepting and close the listener. Spawned connections
    /// drain on their own.
    Stop,
    ListenerClosed(Result<(), CallError>),
    /// `tls_close` reply for an orphan stream accepted after `Stop`.
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
    /// Optional shared body-pressure counters threaded into every
    /// connection child (parity with the plain HTTP listener).
    metrics: Option<BodyMetrics>,
    listener: Option<TlsListenerId>,
    /// Set on the first `Start`. Second Start replies `AlreadyStarted`.
    started: bool,
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
            metrics: None,
            listener: None,
            started: false,
            stopping: false,
            _shard: PhantomData,
        }
    }

    /// Attaches a [`BodyMetrics`] to this listener. Mirror of
    /// [`crate::HttpListener::with_metrics`] so a single
    /// `BodyMetrics` instance can cover both transports on one shard.
    pub fn with_metrics(mut self, metrics: BodyMetrics) -> Self {
        self.metrics = Some(metrics);
        self
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
            HttpsListenerMsg::Start => {
                if self.started {
                    return reply(Err(HttpsStartupError::AlreadyStarted));
                }
                self.started = true;
                tls_bind(
                    self.bind_addr,
                    self.identity.certificate_chain_der.clone(),
                    self.identity.private_key_der.clone(),
                )
                .reply(HttpsListenerMsg::Bound)
            }

            HttpsListenerMsg::Bound(Ok((listener, local_addr))) => {
                self.listener = Some(listener);
                if self.stopping {
                    // Stop arrived while bind was in flight. Close
                    // the just-bound listener; no reply.
                    let listener = self.listener.take().expect("set just above");
                    return tls_close_listener(listener).reply(HttpsListenerMsg::ListenerClosed);
                }
                let ready_effect: Effect<Self> = reply(Ok(HttpsReady { local_addr }));
                let accept_effect: Effect<Self> =
                    tls_accept(listener, self.tls_accept_timeout).reply(HttpsListenerMsg::Accepted);
                batch(vec![ready_effect, accept_effect])
            }
            HttpsListenerMsg::Bound(Err(source)) => {
                batch(vec![reply(Err(HttpsStartupError::Bind { source })), stop()])
            }

            HttpsListenerMsg::Accepted(Ok((stream, _peer))) => {
                if self.stopping {
                    // Orphan-close the just-accepted stream.
                    return tls_close(stream, self.tls_io_timeout)
                        .reply(HttpsListenerMsg::StreamClosed);
                }
                let Some(listener) = self.listener else {
                    // Defensive: invariant says listener=Some when
                    // !stopping. If violated, orphan-close.
                    return tls_close(stream, self.tls_io_timeout)
                        .reply(HttpsListenerMsg::StreamClosed);
                };
                let child = self.build_connection_child(stream);
                batch(vec![
                    spawn(child),
                    tls_accept(listener, self.tls_accept_timeout).reply(HttpsListenerMsg::Accepted),
                ])
            }
            HttpsListenerMsg::Accepted(Err(error)) => {
                if self.stopping {
                    // The pending accept already cleared via the
                    // lane's close-wins synthetic `TargetClosed`.
                    return stop();
                }
                let Some(listener) = self.listener else {
                    return stop();
                };
                // Re-accept on transient errors. `Timeout` = empty
                // accept slice; `TlsHandshake` = malformed peer
                // bytes (no `TlsStreamId` allocated). Everything
                // else would busy-loop — close out.
                match error {
                    CallError::Timeout | CallError::TlsHandshake => {
                        tls_accept(listener, self.tls_accept_timeout)
                            .reply(HttpsListenerMsg::Accepted)
                    }
                    _ => {
                        self.stopping = true;
                        if let Some(listener) = self.listener.take() {
                            tls_close_listener(listener).reply(HttpsListenerMsg::ListenerClosed)
                        } else {
                            stop()
                        }
                    }
                }
            }

            HttpsListenerMsg::Stop => {
                if self.stopping {
                    return noop();
                }
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
    fn build_connection_child(&self, stream: TlsStreamId) -> ChildDefinition<HttpConnection<S, M>> {
        ChildDefinition::new(
            HttpConnection::<S, M>::with_transport_and_metrics(
                HttpTransport::Tls(stream),
                self.service,
                self.limits,
                self.service_call_timeout,
                self.tls_io_timeout,
                self.metrics.clone(),
            ),
            self.connection_mailbox_capacity,
        )
        .with_initial_message(HttpConnectionMsg::Begin)
    }
}
