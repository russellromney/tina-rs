//! Typed service delivery for HTTP listeners.
//!
//! Application code installs a capability-typed handle:
//!
//! - request-only / split request lane → [`HttpListener::for_requests`]
//! - event-only → [`HttpListener::for_events`]
//! - split-service handle → [`HttpListener::for_split_service`]
//!
//! The private [`tina::ServiceMessage`] envelope stays inside this crate.
//! Callers never extract a raw address or name the envelope at the install
//! site.
//!
//! # Delivery semantics
//!
//! | Lane | Admission | Completion |
//! |------|-----------|------------|
//! | Request / split request | `call` into request capability | typed reply or exact terminal |
//! | Event-only | observed send into event capability | `202` when admitted; no processing claim |
//!
//! Terminal mapping (all lanes):
//!
//! - mailbox `Full` → `429 Too Many Requests`
//! - `Closed` / shutdown / foreign system → `503 Service Unavailable`
//! - call `Timeout` → `504 Gateway Timeout`
//! - call `Rejected` → `500 Internal Server Error`
//! - invalid wire input → `400` (parse path)
//! - transport failure remains a transport error

use std::convert::Infallible;
use std::net::SocketAddr;

use http::StatusCode;
use tina::prelude::*;
use tina_runtime::{CallError, CallOutcome, SendOutcome};

use crate::listener::HttpListener;
use crate::types::{FromHttpRequest, HttpRequest, HttpResponse, HttpServerConfig};

/// How a connection delivers one parsed request into the service mailbox.
///
/// Call mode waits for an [`HttpResponse`]. Admit mode reports the send
/// outcome only — used for event-only HTTP.
///
/// Manually `Copy`/`Clone`: both arms only store an [`Address`] and a
/// function pointer, neither of which needs `M: Copy`.
pub(crate) enum ServiceDelivery<M> {
    /// Call the service and wait for a typed HTTP response.
    Call {
        address: Address<M, HttpResponse>,
        to_message: fn(HttpRequest) -> M,
    },
    /// Observed event admission; response is derived from [`SendOutcome`].
    Admit {
        address: Address<M>,
        to_message: fn(HttpRequest) -> M,
    },
}

impl<M> Copy for ServiceDelivery<M> {}

impl<M> Clone for ServiceDelivery<M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M> ServiceDelivery<M>
where
    M: FromHttpRequest,
{
    /// Call-lane delivery that uses [`FromHttpRequest`] for the payload.
    pub(crate) fn call_from_http(address: Address<M, HttpResponse>) -> Self {
        Self::Call {
            address,
            to_message: M::from_http_request,
        }
    }
}

impl<M> ServiceDelivery<M> {
    /// Call-lane delivery with an explicit payload converter.
    pub(crate) fn call(
        address: Address<M, HttpResponse>,
        to_message: fn(HttpRequest) -> M,
    ) -> Self {
        Self::Call {
            address,
            to_message,
        }
    }

    /// Event-admit delivery with an explicit payload converter.
    pub(crate) fn admit(address: Address<M>, to_message: fn(HttpRequest) -> M) -> Self {
        Self::Admit {
            address,
            to_message,
        }
    }

    pub(crate) fn to_message(self, request: HttpRequest) -> M {
        match self {
            Self::Call { to_message, .. } | Self::Admit { to_message, .. } => to_message(request),
        }
    }
}

/// Maps a service-call error into the settled HTTP status table.
///
/// | `CallError` | Status |
/// |-------------|--------|
/// | `TargetFull` | `429 Too Many Requests` |
/// | `TargetClosed` | `503 Service Unavailable` |
/// | `Timeout` | `504 Gateway Timeout` |
/// | `Rejected` / other runtime faults | `500 Internal Server Error` |
pub fn response_for_call_error(error: &CallError) -> HttpResponse {
    let status = match error {
        CallError::TargetFull => StatusCode::TOO_MANY_REQUESTS,
        CallError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        CallError::TargetClosed => StatusCode::SERVICE_UNAVAILABLE,
        CallError::InvariantViolation
        | CallError::InvalidResource
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
        | CallError::TlsAlpnMismatch
        | CallError::SignalFull
        | CallError::SignalClosed
        | CallError::ProcessFull
        | CallError::ProcessClosed
        | CallError::KillUncertain
        | CallError::TimerFull
        | CallError::Rejected(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    HttpResponse::with_status(status)
}

/// Projects a non-reply [`CallOutcome`] into the settled HTTP status table.
pub fn response_for_call_outcome(outcome: &CallOutcome<HttpResponse>) -> Option<HttpResponse> {
    match outcome {
        CallOutcome::Replied(_) => None,
        CallOutcome::Full => Some(HttpResponse::with_status(StatusCode::TOO_MANY_REQUESTS)),
        CallOutcome::Closed => Some(HttpResponse::with_status(StatusCode::SERVICE_UNAVAILABLE)),
        CallOutcome::Timeout => Some(HttpResponse::with_status(StatusCode::GATEWAY_TIMEOUT)),
        CallOutcome::Rejected(_) => {
            Some(HttpResponse::with_status(StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}

/// Maps an observed event-admission outcome into an HTTP response.
///
/// Accepted input yields `202 Accepted` with an empty body — the wire
/// response only claims the event entered the service mailbox, not that the
/// actor processed it.
pub fn response_for_send_outcome(outcome: SendOutcome) -> HttpResponse {
    match outcome {
        SendOutcome::Accepted => HttpResponse::with_status(StatusCode::ACCEPTED),
        SendOutcome::Full => HttpResponse::with_status(StatusCode::TOO_MANY_REQUESTS),
        SendOutcome::Closed | SendOutcome::ForeignSystem { .. } => {
            HttpResponse::with_status(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

fn wrap_service_request<Event, Request>(
    request: HttpRequest,
) -> tina::ServiceMessage<Event, Request>
where
    Request: FromHttpRequest,
{
    tina::ServiceMessage::Request(Request::from_http_request(request))
}

fn wrap_service_event<Event, Request>(request: HttpRequest) -> tina::ServiceMessage<Event, Request>
where
    Event: FromHttpRequest,
{
    tina::ServiceMessage::Event(Event::from_http_request(request))
}

impl<S: Shard + 'static, Event, Request> HttpListener<S, tina::ServiceMessage<Event, Request>>
where
    Event: Send + 'static,
    Request: FromHttpRequest + Send + 'static,
{
    /// Install request-lane delivery from a typed request capability.
    ///
    /// Accepts a request-only handle or the `.requests` half of a split
    /// handle. The private service envelope is never named at the call site.
    ///
    /// ```compile_fail
    /// use std::net::SocketAddr;
    /// use tina::prelude::*;
    /// use tina_http::{HttpListener, HttpRequest, HttpServerConfig};
    /// use tina_runtime::EventServiceHandle;
    ///
    /// fn bad(
    ///     events: EventServiceHandle<HttpRequest>,
    ///     bind: SocketAddr,
    ///     config: HttpServerConfig,
    /// ) {
    ///     // Event handles have no request lane — this must not compile.
    ///     let _ = HttpListener::<SingleShard, _>::for_requests(bind, events, config);
    /// }
    /// ```
    pub fn for_requests(
        bind_addr: SocketAddr,
        requests: tina::ServiceRequestAddress<Event, Request, HttpResponse>,
        config: HttpServerConfig,
    ) -> Self {
        Self::from_delivery(
            bind_addr,
            ServiceDelivery::call(
                requests.address().address(),
                wrap_service_request::<Event, Request>,
            ),
            config,
        )
    }

    /// Install request-lane delivery from a split-service handle.
    ///
    /// Consumes only the request capability. The event half is not required
    /// for HTTP wire delivery and is not held by the listener.
    pub fn for_split_service(
        bind_addr: SocketAddr,
        handle: tina_runtime::SplitServiceHandle<Event, Request, HttpResponse>,
        config: HttpServerConfig,
    ) -> Self {
        Self::for_requests(bind_addr, handle.requests, config)
    }
}

impl<S: Shard + 'static, Request> HttpListener<S, tina::ServiceMessage<Infallible, Request>>
where
    Request: FromHttpRequest + Send + 'static,
{
    /// Install request-lane delivery from a request-only service handle.
    pub fn for_request_service(
        bind_addr: SocketAddr,
        requests: tina_runtime::RequestServiceHandle<Request, HttpResponse>,
        config: HttpServerConfig,
    ) -> Self {
        Self::for_requests(bind_addr, requests, config)
    }
}

impl<S: Shard + 'static, Event, Request> HttpListener<S, tina::ServiceMessage<Event, Request>>
where
    Event: FromHttpRequest + Send + 'static,
    Request: Send + 'static,
{
    /// Install event-only admission delivery from a typed event capability.
    ///
    /// Valid input that is accepted into the service mailbox completes with
    /// `202 Accepted` and an empty body. That response does **not** claim the
    /// actor processed the event.
    ///
    /// ```compile_fail
    /// use std::net::SocketAddr;
    /// use tina::prelude::*;
    /// use tina_http::{HttpListener, HttpRequest, HttpResponse, HttpServerConfig};
    /// use tina_runtime::RequestServiceHandle;
    ///
    /// fn bad(
    ///     requests: RequestServiceHandle<HttpRequest, HttpResponse>,
    ///     bind: SocketAddr,
    ///     config: HttpServerConfig,
    /// ) {
    ///     // Request handles have no event lane — this must not compile.
    ///     let _ = HttpListener::<SingleShard, _>::for_events(bind, requests, config);
    /// }
    /// ```
    pub fn for_events(
        bind_addr: SocketAddr,
        events: tina::ServiceEventAddress<Event, Request>,
        config: HttpServerConfig,
    ) -> Self {
        Self::from_delivery(
            bind_addr,
            ServiceDelivery::admit(
                events.address().address(),
                wrap_service_event::<Event, Request>,
            ),
            config,
        )
    }
}

impl<S: Shard + 'static, Event> HttpListener<S, tina::ServiceMessage<Event, Infallible>>
where
    Event: FromHttpRequest + Send + 'static,
{
    /// Install event-only admission from an event-only service handle.
    pub fn for_event_service(
        bind_addr: SocketAddr,
        events: tina_runtime::EventServiceHandle<Event>,
        config: HttpServerConfig,
    ) -> Self {
        Self::for_events(bind_addr, events, config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_call_error_maps_to_429() {
        assert_eq!(
            response_for_call_error(&CallError::TargetFull).status,
            StatusCode::TOO_MANY_REQUESTS,
        );
    }

    #[test]
    fn closed_call_error_maps_to_503() {
        assert_eq!(
            response_for_call_error(&CallError::TargetClosed).status,
            StatusCode::SERVICE_UNAVAILABLE,
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
    fn full_outcome_projects_to_429() {
        let response = response_for_call_outcome(&CallOutcome::<HttpResponse>::Full)
            .expect("Full projects to a response");
        assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn closed_outcome_projects_to_503() {
        let response = response_for_call_outcome(&CallOutcome::<HttpResponse>::Closed)
            .expect("Closed projects to a response");
        assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
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

    #[test]
    fn send_accepted_maps_to_202() {
        assert_eq!(
            response_for_send_outcome(SendOutcome::Accepted).status,
            StatusCode::ACCEPTED,
        );
    }

    #[test]
    fn send_full_maps_to_429() {
        assert_eq!(
            response_for_send_outcome(SendOutcome::Full).status,
            StatusCode::TOO_MANY_REQUESTS,
        );
    }

    #[test]
    fn send_closed_maps_to_503() {
        assert_eq!(
            response_for_send_outcome(SendOutcome::Closed).status,
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
}
