//! Listener isolate for the native HTTP/1.1 server.
//!
//! [`HttpListener`] binds to an address, accepts inbound TCP connections,
//! and spawns one [`HttpConnection`] per accepted socket. Each connection
//! delivers the parsed request into a typed service handle (request call,
//! event admission, or raw address adapter), writes the response, and closes.
//!
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;

use tina::prelude::*;
use tina::{CallContext, RequestContext, reply_to};
use tina_runtime::{
    CallError, ListenerId, StreamId, tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream,
};

use crate::body_metrics::BodyMetrics;
use crate::connection::{HttpConnection, HttpConnectionMsg};
use crate::delivery::ServiceDelivery;
use crate::transport::HttpTransport;
use crate::types::{FromHttpRequest, HttpLimits, HttpRequest, HttpResponse, HttpServerConfig};

/// Inbound message variants for [`HttpListener`].
#[derive(Debug, Clone)]
pub enum HttpListenerMsg {
    /// Kick off the bind. Sent once by the host after registering.
    Start,
    /// `tcp_bind` reply.
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    /// `tcp_accept` reply.
    Accepted(Result<(StreamId, SocketAddr), CallError>),
    /// Request to stop accepting new connections and close the listener
    /// socket. Already-spawned connection isolates run to completion on
    /// their own; the runtime's shutdown drain handles the rest.
    Stop,
    /// `tcp_close_listener` reply.
    ListenerClosed(Result<(), CallError>),
    /// `tcp_close_stream` reply for a stream the listener decided to
    /// drop (typically a kernel-already-accepted connection that
    /// arrived after `Stop`).
    StreamClosed(Result<(), CallError>),
}

/// HTTP listener is bound. `local_addr` carries the kernel-assigned
/// port when the caller bound `:0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpReady {
    pub local_addr: SocketAddr,
}

/// Address type for a plain HTTP listener.
pub type HttpListenerAddress = Address<HttpListenerMsg, Result<HttpReady, HttpStartupError>>;

/// Plain HTTP listener could not start. No child spawned, no listener
/// resource held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpStartupError {
    /// `tcp_bind` failed. `source` is the typed runtime error.
    Bind { source: CallError },
    /// Second `Start` on the same listener. The first bind still owns
    /// the listener — no second bind happens.
    AlreadyStarted,
    /// `Stop` arrived while startup was in flight.
    Stopped,
}

/// Listener isolate.
///
/// Generic over the user's `Shard` and the service's message type `M`
/// (default [`HttpRequest`]). Prefer the typed constructors:
///
/// - [`HttpListener::for_requests`] / [`HttpListener::for_request_service`]
/// - [`HttpListener::for_events`] / [`HttpListener::for_event_service`]
/// - [`HttpListener::for_split_service`]
///
/// The raw [`HttpListener::new`] / [`HttpListener::with_config`] path remains
/// for ordinary address adapters and multi-turn services that still own a
/// callable address. Split-service installs should not name
/// `tina::ServiceMessage` at the call site — use the typed constructors.
pub struct HttpListener<S: Shard + 'static, M: Send + 'static = HttpRequest> {
    pub(crate) bind_addr: SocketAddr,
    pub(crate) delivery: ServiceDelivery<M>,
    pub(crate) limits: HttpLimits,
    pub(crate) service_call_timeout: Duration,
    pub(crate) connection_mailbox_capacity: usize,
    /// Optional shared body-pressure counters threaded into every
    /// connection child. `None` means no metrics — same default
    /// behaviour as before.
    pub(crate) metrics: Option<BodyMetrics>,
    listener: Option<ListenerId>,
    pending_start_reply: Option<RequestContext<Result<HttpReady, HttpStartupError>>>,
    /// Set on the first `Start`. Second Start is a no-op.
    started: bool,
    stopping: bool,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static, M: Send + 'static> HttpListener<S, M> {
    /// Internal constructor shared by raw and typed install paths.
    pub(crate) fn from_delivery(
        bind_addr: SocketAddr,
        delivery: ServiceDelivery<M>,
        config: HttpServerConfig,
    ) -> Self {
        Self {
            bind_addr,
            delivery,
            limits: config.limits,
            service_call_timeout: config.service_call_timeout,
            connection_mailbox_capacity: config.connection_mailbox_capacity,
            metrics: None,
            listener: None,
            pending_start_reply: None,
            started: false,
            stopping: false,
            _shard: PhantomData,
        }
    }

    /// Attaches a [`BodyMetrics`] to this listener. Every connection
    /// spawned by the listener will charge body bytes into the
    /// metrics. Call before registering the listener.
    pub fn with_metrics(mut self, metrics: BodyMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

impl<S: Shard + 'static, M: FromHttpRequest + Send + 'static> HttpListener<S, M> {
    /// Builds a listener that will bind to `bind_addr`, dispatch every
    /// parsed request to `service`, and spawn one connection isolate
    /// per accept.
    ///
    /// `connection_mailbox_capacity` is the bounded mailbox size for
    /// each [`HttpConnection`] child. A small number (16-32) is plenty.
    ///
    /// `service_call_timeout` is the timeout passed to every
    /// [`tina_runtime::call`] into the service isolate. It is the
    /// upstream-side analogue of an HTTP request deadline.
    ///
    /// For capability-typed services prefer
    /// [`HttpListener::for_requests`], [`HttpListener::for_events`], or
    /// [`HttpListener::for_split_service`].
    pub fn new(
        bind_addr: SocketAddr,
        service: Address<M, HttpResponse>,
        limits: HttpLimits,
        service_call_timeout: Duration,
        connection_mailbox_capacity: usize,
    ) -> Self {
        Self {
            bind_addr,
            delivery: ServiceDelivery::call_from_http(service),
            limits,
            service_call_timeout,
            connection_mailbox_capacity,
            metrics: None,
            listener: None,
            pending_start_reply: None,
            started: false,
            stopping: false,
            _shard: PhantomData,
        }
    }

    /// Convenience constructor that absorbs an [`HttpServerConfig`].
    ///
    /// `HttpServerConfig` is `Copy`, so the caller can read
    /// `listener_mailbox_capacity` from the same value when calling
    /// `register_with_capacity`.
    pub fn with_config(
        bind_addr: SocketAddr,
        service: Address<M, HttpResponse>,
        config: HttpServerConfig,
    ) -> Self {
        Self::new(
            bind_addr,
            service,
            config.limits,
            config.service_call_timeout,
            config.connection_mailbox_capacity,
        )
    }
}

// Hand-rolled `Isolate` impl: the macro requires a concrete shard type;
// we want this generic so callers can pick their own shard.
impl<S: Shard + 'static, M: Send + 'static> Isolate for HttpListener<S, M> {
    tina::isolate_types! {
        message: HttpListenerMsg,
        reply: Result<HttpReady, HttpStartupError>,
        send: tina::Outbound<std::convert::Infallible>,
        spawn: ChildDefinition<HttpConnection<S, M>>,
        io: tina_runtime::RuntimeCall<HttpListenerMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: HttpListenerMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            HttpListenerMsg::Start => {
                if self.started {
                    // Idempotent: a second Start would overwrite
                    // self.listener and leak the first. Reply type
                    // is `()` so the contract is "trace shows one
                    // TcpBind".
                    return noop();
                }
                self.started = true;
                tcp_bind(self.bind_addr).then(HttpListenerMsg::Bound)
            }

            HttpListenerMsg::Bound(Ok((listener, local_addr))) => {
                self.listener = Some(listener);
                if self.stopping {
                    // Stop arrived while bind was in flight. Close
                    // the just-bound listener immediately rather
                    // than starting the accept loop, otherwise the
                    // listener would leak until full shutdown.
                    let listener = self.listener.take().expect("set just above");
                    let close = tcp_close_listener(listener).then(HttpListenerMsg::ListenerClosed);
                    return match self.pending_start_reply.take() {
                        Some(request) => batch(vec![
                            reply_to(request, Err(HttpStartupError::Stopped)),
                            close,
                        ]),
                        None => close,
                    };
                }
                let accept = tcp_accept(listener).then(HttpListenerMsg::Accepted);
                match self.pending_start_reply.take() {
                    Some(request) => batch(vec![
                        reply_to(request, Ok(HttpReady { local_addr })),
                        accept,
                    ]),
                    None => accept,
                }
            }
            HttpListenerMsg::Bound(Err(source)) => match self.pending_start_reply.take() {
                Some(request) => batch(vec![
                    reply_to(request, Err(HttpStartupError::Bind { source })),
                    stop(),
                ]),
                None => stop(),
            },

            HttpListenerMsg::Accepted(Ok((stream, _peer))) => {
                if self.stopping {
                    // Stop already ran; the listener socket may already
                    // be closed (we took `self.listener` then), or the
                    // close is in flight. The kernel may still have
                    // queued a connection for us before our close took
                    // effect. We close the orphan stream and do not
                    // touch the listener again.
                    return tcp_close_stream(stream).then(HttpListenerMsg::StreamClosed);
                }
                let Some(listener) = self.listener else {
                    // Defensive: invariant says listener=Some when
                    // !stopping. If violated, orphan-close.
                    return tcp_close_stream(stream).then(HttpListenerMsg::StreamClosed);
                };
                let child = self.build_connection_child(stream);
                batch(vec![
                    spawn(child),
                    tcp_accept(listener).then(HttpListenerMsg::Accepted),
                ])
            }
            HttpListenerMsg::Accepted(Err(error)) => {
                if self.stopping {
                    return stop();
                }
                let Some(listener) = self.listener else {
                    return stop();
                };
                match error {
                    // A bad peer can make the substrate surface an
                    // accept-side I/O error even though the listener
                    // socket is still usable. Keep the front door alive;
                    // the failed accept is already visible in the trace.
                    CallError::Io => tcp_accept(listener).then(HttpListenerMsg::Accepted),
                    _ => {
                        self.stopping = true;
                        if let Some(listener) = self.listener.take() {
                            tcp_close_listener(listener).then(HttpListenerMsg::ListenerClosed)
                        } else {
                            stop()
                        }
                    }
                }
            }

            HttpListenerMsg::Stop => {
                self.stopping = true;
                if let Some(listener) = self.listener.take() {
                    tcp_close_listener(listener).then(HttpListenerMsg::ListenerClosed)
                } else {
                    stop()
                }
            }

            HttpListenerMsg::ListenerClosed(_) => {
                // Even after the listener closes, an `Accepted(Ok)`
                // queued before close arrives can still be processed —
                // it routes to the orphan-close path above. We only
                // stop self once we are confident no more accepts will
                // arrive; in practice the runtime stops delivering
                // accepts after the listener close completes, so this
                // is the right place to exit the listener isolate.
                stop()
            }
            HttpListenerMsg::StreamClosed(_) => {
                // Orphan-close result. We do not stop on this — the
                // listener stays alive until ListenerClosed arrives so
                // any other queued Accepted(Ok) can also be drained
                // through the orphan-close path.
                noop()
            }
        }
    }

    fn handle_call(&mut self, msg: HttpListenerMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            HttpListenerMsg::Start => {
                if self.started {
                    return call.reply(Err(HttpStartupError::AlreadyStarted));
                }
                self.started = true;
                self.pending_start_reply = Some(call.into_request_context());
                tcp_bind(self.bind_addr).then(HttpListenerMsg::Bound)
            }
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl<S: Shard + 'static, M: Send + 'static> HttpListener<S, M> {
    fn build_connection_child(&self, stream: StreamId) -> ChildDefinition<HttpConnection<S, M>> {
        ChildDefinition::new(
            HttpConnection::<S, M>::with_delivery_and_metrics(
                HttpTransport::Tcp(stream),
                self.delivery,
                self.limits,
                self.service_call_timeout,
                Duration::ZERO,
                self.metrics.clone(),
            )
            .with_service_peer_monitor(self.connection_mailbox_capacity >= 3),
            self.connection_mailbox_capacity,
        )
        .with_initial_message(HttpConnectionMsg::Begin)
    }
}
