//! Bounded outbound HTTP connection pool.
//!
//! [`HttpConnectionPool`] is a service-shaped admission gate in front of
//! an [`crate::HttpClient`]. Users invoke it the same way as any other
//! Tina service:
//!
//! ```rust,ignore
//! call(pool, HttpPoolMsg::Submit(OutboundCall { target, request }), timeout)
//!     .then(MyMsg::HttpReturned)
//! ```
//!
//! Slot free: pool forwards via `call(client, ...).then(Returned)`,
//! and the continuation chain replies to the original Submit caller.
//! Slot busy: new Submit replies `Err(HttpClientError::PoolFull)`
//! immediately. No waiter queue. No idle reuse.
//!
//! Capacity is fixed at 1; constructing a pool with any other value
//! panics. Multi-slot pools need multiple `HttpClient` instances and
//! a placement policy — that is a separate slice.
//!
//! Topology: register one `HttpClient` per pool. If two pools share
//! one client, pool B's call into the client can return
//! `CallOutcome::Full` (because pool A is using it), which the pool
//! maps to `HttpClientError::Busy`. Submit-callers then see `Busy`
//! coming out of a "PoolFull"-shaped situation and cannot
//! distinguish.

use std::marker::PhantomData;

use tina::prelude::*;
use tina::{CallContext, RequestContext, reply_to};
use tina_runtime::{CallOutcome, RuntimeCall, call};

use crate::client::{HttpClientMsg, OutboundCall};
use crate::types::{HttpClientError, HttpResponse, PoolConfig};

/// Messages handled by [`HttpConnectionPool`].
///
/// `Submit` is the user-callable variant. `Returned` is an internal
/// continuation produced by the pool's own `call(client, ...)`; user
/// code never constructs it directly.
#[derive(Debug)]
pub enum HttpPoolMsg {
    /// User-side: run this outbound call through the pool. The reply
    /// (`Result<HttpResponse, HttpClientError>`) lands via the
    /// runtime's continuation chain.
    Submit(OutboundCall),
    /// Internal: the underlying `HttpClient` returned for the in-flight
    /// Submit.
    Returned(
        RequestContext<Result<HttpResponse, HttpClientError>>,
        CallOutcome<Result<HttpResponse, HttpClientError>>,
    ),
}

/// Service-shaped admission-controlled HTTP connection pool.
pub struct HttpConnectionPool<S: Shard + 'static> {
    config: PoolConfig,
    client: Address<HttpClientMsg, Result<HttpResponse, HttpClientError>>,
    in_flight: bool,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> HttpConnectionPool<S> {
    /// Constructs a pool fronting `client` with the given config.
    ///
    /// # Panics
    ///
    /// Panics when `config.capacity != 1`. See module docs.
    pub fn new(
        config: PoolConfig,
        client: Address<HttpClientMsg, Result<HttpResponse, HttpClientError>>,
    ) -> Self {
        assert_eq!(
            config.capacity, 1,
            "first-form HttpConnectionPool requires capacity = 1",
        );
        Self {
            config,
            client,
            in_flight: false,
            _shard: PhantomData,
        }
    }
}

impl<S: Shard + 'static> Isolate for HttpConnectionPool<S> {
    tina::isolate_types! {
        message: HttpPoolMsg,
        reply: Result<HttpResponse, HttpClientError>,
        send: tina::Outbound<std::convert::Infallible>,
        spawn: std::convert::Infallible,
        call: RuntimeCall<HttpPoolMsg>,
        shard: S,
    }

    fn handle(&mut self, msg: HttpPoolMsg, _ctx: &mut Context<'_, S, Self::Reply>) -> Effect<Self> {
        match msg {
            HttpPoolMsg::Submit(_) => reply(Err(HttpClientError::Closed)),
            HttpPoolMsg::Returned(request, outcome) => {
                self.in_flight = false;
                let result = match outcome {
                    CallOutcome::Replied(inner) => inner,
                    CallOutcome::Full => Err(HttpClientError::Busy),
                    CallOutcome::Closed => Err(HttpClientError::Closed),
                    CallOutcome::Timeout => Err(HttpClientError::Timeout),
                    CallOutcome::Rejected(_) => Err(HttpClientError::Closed),
                };
                reply_to(request, result)
            }
        }
    }

    fn handle_call(&mut self, msg: HttpPoolMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            HttpPoolMsg::Submit(outbound) => {
                if self.in_flight {
                    return call_ctx.reply(Err(HttpClientError::PoolFull));
                }
                self.in_flight = true;
                let request = call_ctx.into_request_context();
                call(
                    self.client,
                    HttpClientMsg::Call(Box::new(outbound)),
                    self.config.client_call_timeout,
                )
                .then_with_request(request, HttpPoolMsg::Returned)
            }
            HttpPoolMsg::Returned(_, _) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}
