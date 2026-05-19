//! Capability-typed service handles returned by `Runtime::register_*`.
//!
//! Splits the raw `Address<M, R>` into separate `send` / `call` /
//! event / request capabilities so "called a send-only handle" / "sent
//! through a call-only handle" become compile errors rather than
//! runtime [`tina::CallRejectedReason::UnsupportedMessage`] rejections.

/// Capability-typed handles for one registered callable service.
///
/// Returned by [`crate::Runtime::register_service`]. The `send` lane is a
/// [`SendAddress<M>`](tina::SendAddress) for ordinary send/continuation
/// traffic; the `call` lane is a [`CallAddress<M, R>`](tina::CallAddress) for
/// callable traffic. Splitting the address into capabilities at the boundary
/// turns "called a send-only handle" / "sent through a call-only handle" into
/// compile errors rather than runtime
/// [`tina::CallRejectedReason::UnsupportedMessage`] rejections.
///
/// Both lanes point at the same underlying isolate. The split is purely a
/// type-level capability check at the caller; the runtime continues to route
/// based on whether the inbound message arrived with a reply slot.
#[derive(Debug)]
pub struct ServiceHandle<M, R> {
    /// Send-only capability for the service's mailbox.
    pub send: tina::SendAddress<M>,
    /// Callable capability for the service's mailbox.
    pub call: tina::CallAddress<M, R>,
}

impl<M, R> Copy for ServiceHandle<M, R> {}

impl<M, R> Clone for ServiceHandle<M, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M, R> ServiceHandle<M, R> {
    /// Wraps a raw [`Address<M, R>`](tina::Address) as both capabilities.
    pub const fn from_address(address: tina::Address<M, R>) -> Self {
        Self {
            send: address.send_only(),
            call: address.callable(),
        }
    }

    /// Returns the underlying raw [`Address<M, R>`](tina::Address).
    pub const fn address(self) -> tina::Address<M, R> {
        self.call.address()
    }
}

/// Capability-typed handle for one registered send-only service.
///
/// Returned by [`crate::Runtime::register_service_send_only`]. Exposes only a
/// [`SendAddress`](tina::SendAddress): no callable lane is constructed.
#[derive(Debug)]
pub struct SendOnlyServiceHandle<M> {
    /// Send-only capability for the service's mailbox.
    pub send: tina::SendAddress<M>,
}

impl<M> Copy for SendOnlyServiceHandle<M> {}

impl<M> Clone for SendOnlyServiceHandle<M> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Capability-typed handles for one split event/request service.
///
/// Returned by [`crate::Runtime::register_split_service`]. The `events` lane accepts
/// only public fire-and-forget events through [`tina::send_event`]. The
/// `requests` lane accepts only callable requests through [`crate::call_request`].
#[derive(Debug)]
pub struct SplitServiceHandle<Event, Request, Reply> {
    /// Send capability for service events.
    pub events: tina::ServiceEventAddress<Event, Request>,
    /// Call capability for service requests.
    pub requests: tina::ServiceRequestAddress<Event, Request, Reply>,
}

impl<Event, Request, Reply> Copy for SplitServiceHandle<Event, Request, Reply> {}

impl<Event, Request, Reply> Clone for SplitServiceHandle<Event, Request, Reply> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Event, Request, Reply> SplitServiceHandle<Event, Request, Reply> {
    /// Wraps the raw service envelope address as split capabilities.
    pub const fn from_address(
        address: tina::Address<tina::ServiceMessage<Event, Request>, Reply>,
    ) -> Self {
        Self {
            events: tina::ServiceEventAddress::from_send_address(address.send_only()),
            requests: tina::ServiceRequestAddress::from_call_address(address.callable()),
        }
    }

    /// Returns the underlying raw envelope address.
    pub const fn address(self) -> tina::Address<tina::ServiceMessage<Event, Request>, Reply> {
        self.requests.address().address()
    }
}
