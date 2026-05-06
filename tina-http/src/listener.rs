//! Listener isolate for the native HTTP/1.1 server.
//!
//! [`HttpListener`] binds to an address, accepts inbound TCP connections,
//! and spawns one [`HttpConnection`] per accepted socket. Each connection
//! handles one request, calls the user's service isolate, writes the
//! response, and closes.
//!
//! The bound socket address is published via a shared
//! `Arc<Mutex<Option<SocketAddr>>>` slot. This is the same `BoundAddr`
//! pattern used by the keyspace and chat Eiffel comparisons; phase 047's
//! "host observation handles" rock will replace it with a typed handle
//! when it lands.

use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, ListenerId, StreamId, tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream,
};

use crate::connection::{HttpConnection, HttpConnectionMsg};
use crate::types::{HttpLimits, HttpRequest, HttpResponse};

/// Shared slot the listener writes its bound socket address into. The
/// host (or tests) reads this once after registering the isolate to
/// learn the actual port the kernel chose.
pub type BoundAddrSlot = Arc<Mutex<Option<SocketAddr>>>;

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
/// Generic over the user's `Shard` so a single implementation works for
/// any shard placement chosen by the surrounding service.
pub struct HttpListener<S: Shard + 'static> {
    bind_addr: SocketAddr,
    bound_addr: BoundAddrSlot,
    service: Address<HttpRequest, HttpResponse>,
    limits: HttpLimits,
    service_call_timeout: Duration,
    connection_mailbox_capacity: usize,
    listener: Option<ListenerId>,
    stopping: bool,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> HttpListener<S> {
    /// Builds a listener that will bind to `bind_addr`, dispatch every
    /// parsed request to `service`, and spawn one connection isolate
    /// per accept.
    ///
    /// `connection_mailbox_capacity` is the bounded mailbox size for
    /// each [`HttpConnection`] child. A small number (16-32) is enough
    /// for first form: one Begin, one Read, one ServiceReturned, one
    /// Wrote, one Closed, plus headroom.
    ///
    /// `service_call_timeout` is the timeout passed to every
    /// [`tina_runtime::call`] into the service isolate. It is the
    /// upstream-side analogue of an HTTP request deadline.
    pub fn new(
        bind_addr: SocketAddr,
        bound_addr: BoundAddrSlot,
        service: Address<HttpRequest, HttpResponse>,
        limits: HttpLimits,
        service_call_timeout: Duration,
        connection_mailbox_capacity: usize,
    ) -> Self {
        Self {
            bind_addr,
            bound_addr,
            service,
            limits,
            service_call_timeout,
            connection_mailbox_capacity,
            listener: None,
            stopping: false,
            _shard: PhantomData,
        }
    }
}

// Hand-rolled `Isolate` impl: the macro requires a concrete shard type;
// we want this generic so callers can pick their own shard.
impl<S: Shard + 'static> Isolate for HttpListener<S> {
    tina::isolate_types! {
        message: HttpListenerMsg,
        reply: (),
        send: tina::Outbound<std::convert::Infallible>,
        spawn: ChildDefinition<HttpConnection<S>>,
        call: tina_runtime::RuntimeCall<HttpListenerMsg>,
        shard: S,
    }

    fn handle(&mut self, msg: HttpListenerMsg, _ctx: &mut Context<'_, S>) -> Effect<Self> {
        match msg {
            HttpListenerMsg::Start => tcp_bind(self.bind_addr).reply(HttpListenerMsg::Bound),

            HttpListenerMsg::Bound(Ok((listener, local_addr))) => {
                self.listener = Some(listener);
                if let Ok(mut slot) = self.bound_addr.lock() {
                    *slot = Some(local_addr);
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
                let listener = self.listener.expect("listener set after bind");
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

impl<S: Shard + 'static> HttpListener<S> {
    fn build_connection_child(&self, stream: StreamId) -> ChildDefinition<HttpConnection<S>> {
        ChildDefinition::new(
            HttpConnection::<S>::new(stream, self.service, self.limits, self.service_call_timeout),
            self.connection_mailbox_capacity,
        )
        .with_initial_message(HttpConnectionMsg::Begin)
    }
}
