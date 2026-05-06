//! Bounded outbound HTTP connection pool.
//!
//! [`HttpConnectionPool`] is a service-shaped admission gate in front of
//! an [`crate::HttpClient`]. Users invoke it the same way as any other
//! Tina service:
//!
//! ```rust,ignore
//! call(pool, HttpPoolMsg::Submit(OutboundCall { target, request }), timeout)
//!     .reply(MyMsg::HttpReturned)
//! ```
//!
//! Slot free: pool forwards via `call(client, ...).reply(Returned)`,
//! and the continuation chain replies to the original Submit caller.
//! Slot busy: new Submit replies `Err(HttpClientError::PoolFull)`
//! immediately. No waiter queue. No idle reuse.
//!
//! Capacity is fixed at 1; constructing a pool with any other value
//! panics. Multi-slot pools need multiple `HttpClient` instances and
//! a placement policy — that is a separate slice.

use std::marker::PhantomData;

use tina::prelude::*;
use tina_runtime::{CallOutcome, RuntimeCall, call};

use crate::client::{HttpClientMsg, OutboundCall};
use crate::types::{HttpClientError, HttpResponse, PoolConfig};

/// Messages handled by [`HttpConnectionPool`].
///
/// `Submit` is the user-callable variant. `Returned` is an internal
/// continuation produced by the pool's own `call(client, ...)`; user
/// code never constructs it directly.
#[derive(Debug, Clone)]
pub enum HttpPoolMsg {
    /// User-side: run this outbound call through the pool. The reply
    /// (`Result<HttpResponse, HttpClientError>`) lands via the
    /// runtime's continuation chain.
    Submit(OutboundCall),
    /// Internal: the underlying `HttpClient` returned for the in-flight
    /// Submit.
    Returned(CallOutcome<Result<HttpResponse, HttpClientError>>),
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

    fn handle(&mut self, msg: HttpPoolMsg, _ctx: &mut Context<'_, S>) -> Effect<Self> {
        match msg {
            HttpPoolMsg::Submit(outbound) => {
                if self.in_flight {
                    return reply(Err(HttpClientError::PoolFull));
                }
                self.in_flight = true;
                call(
                    self.client,
                    HttpClientMsg::Call(outbound),
                    self.config.client_call_timeout,
                )
                .reply(HttpPoolMsg::Returned)
            }

            HttpPoolMsg::Returned(outcome) => {
                self.in_flight = false;
                let result = match outcome {
                    CallOutcome::Replied(inner) => inner,
                    CallOutcome::Full => Err(HttpClientError::Busy),
                    CallOutcome::Closed => Err(HttpClientError::Closed),
                    CallOutcome::Timeout => Err(HttpClientError::Timeout),
                };
                reply(result)
            }
        }
    }
}
