//! Listener isolate for the native HTTP/1.1 server.
//!
//! [`HttpListener`] binds to an address, accepts inbound TCP connections,
//! and spawns one [`HttpConnection`] per accepted socket. Each connection
//! handles one request, calls the user's service isolate, writes the
//! response, and closes.
//!
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, ListenerId, StreamId, tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream,
};

use crate::connection::{HttpConnection, HttpConnectionMsg};
use crate::types::{HttpLimits, HttpRequest, HttpResponse, HttpServerConfig};

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

/// Listener isolate.
///
/// Generic over the user's `Shard` and the service's message type
/// `M`. `M` defaults to `HttpRequest` for sync-reply services;
/// multi-turn services declare an enum that wraps `HttpRequest` and
/// supply `From<HttpRequest>`.
pub struct HttpListener<S: Shard + 'static, M: From<HttpRequest> + Send + 'static = HttpRequest> {
    bind_addr: SocketAddr,
    service: Address<M, HttpResponse>,
    limits: HttpLimits,
    service_call_timeout: Duration,
    connection_mailbox_capacity: usize,
    listener: Option<ListenerId>,
    /// Set on the first `Start`. Second Start is a no-op.
    started: bool,
    stopping: bool,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> HttpListener<S, M> {
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
    pub fn new(
        bind_addr: SocketAddr,
        service: Address<M, HttpResponse>,
        limits: HttpLimits,
        service_call_timeout: Duration,
        connection_mailbox_capacity: usize,
    ) -> Self {
        Self {
            bind_addr,
            service,
            limits,
            service_call_timeout,
            connection_mailbox_capacity,
            listener: None,
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
impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> Isolate for HttpListener<S, M> {
    tina::isolate_types! {
        message: HttpListenerMsg,
        reply: (),
        send: tina::Outbound<std::convert::Infallible>,
        spawn: ChildDefinition<HttpConnection<S, M>>,
        call: tina_runtime::RuntimeCall<HttpListenerMsg>,
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
                tcp_bind(self.bind_addr).reply(HttpListenerMsg::Bound)
            }

            HttpListenerMsg::Bound(Ok((listener, local_addr))) => {
                self.listener = Some(listener);
                let _ = local_addr;
                if self.stopping {
                    // Stop arrived while bind was in flight. Close
                    // the just-bound listener immediately rather
                    // than starting the accept loop, otherwise the
                    // listener would leak until full shutdown.
                    let listener = self.listener.take().expect("set just above");
                    return tcp_close_listener(listener)
                        .reply(HttpListenerMsg::ListenerClosed);
                }
                tcp_accept(listener).reply(HttpListenerMsg::Accepted)
            }
            HttpListenerMsg::Bound(Err(_)) => stop(),

            HttpListenerMsg::Accepted(Ok((stream, _peer))) => {
                if self.stopping {
                    // Stop already ran; the listener socket may already
                    // be closed (we took `self.listener` then), or the
                    // close is in flight. The kernel may still have
                    // queued a connection for us before our close took
                    // effect. We close the orphan stream and do not
                    // touch the listener again.
                    return tcp_close_stream(stream).reply(HttpListenerMsg::StreamClosed);
                }
                let Some(listener) = self.listener else {
                    // Defensive: invariant says listener=Some when
                    // !stopping. If violated, orphan-close.
                    return tcp_close_stream(stream).reply(HttpListenerMsg::StreamClosed);
                };
                let child = self.build_connection_child(stream);
                batch(vec![
                    spawn(child),
                    tcp_accept(listener).reply(HttpListenerMsg::Accepted),
                ])
            }
            HttpListenerMsg::Accepted(Err(_)) => {
                // Accept failed (likely listener was closed). Drop into
                // close path if still open, else stop.
                if let Some(listener) = self.listener.take() {
                    tcp_close_listener(listener).reply(HttpListenerMsg::ListenerClosed)
                } else {
                    stop()
                }
            }

            HttpListenerMsg::Stop => {
                self.stopping = true;
                if let Some(listener) = self.listener.take() {
                    tcp_close_listener(listener).reply(HttpListenerMsg::ListenerClosed)
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
}

impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> HttpListener<S, M> {
    fn build_connection_child(&self, stream: StreamId) -> ChildDefinition<HttpConnection<S, M>> {
        ChildDefinition::new(
            HttpConnection::<S, M>::new(
                stream,
                self.service,
                self.limits,
                self.service_call_timeout,
            ),
            self.connection_mailbox_capacity,
        )
        .with_initial_message(HttpConnectionMsg::Begin)
    }
}
