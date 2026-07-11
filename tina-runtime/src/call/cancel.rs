//! Call cancellation: `CancelCallBuilder`, `cancel_call`,
//! `call_cancelable`, `call_with_handle`, and `call_handle_call_id`.

use std::time::Duration;

use tina::{Address, CallHandle, CallHandleInner, CancelOutcome};

use super::{CallId, CancelableCall, RuntimeCall};

/// Builder returned by [`cancel_call`].
#[doc(hidden)]
pub struct CancelCallBuilder {
    inner: CallHandleInner,
}

impl CancelCallBuilder {
    pub(super) fn new(inner: CallHandleInner) -> Self {
        Self { inner }
    }

    /// Returns the cancel effect; the [`CancelOutcome`] arrives back
    /// as `translator(outcome)`.
    #[deprecated(since = "0.1.0", note = "use `.then(...)` for ordinary continuations")]
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(CancelOutcome) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Returns the cancel effect; the [`CancelOutcome`] arrives back as one
    /// ordinary continuation message.
    pub fn then<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(CancelOutcome) -> M + 'static,
        M: 'static,
    {
        let shared = tina::runtime_internal::call_handle_inner_into_shared(self.inner);
        tina::Effect::Io(RuntimeCall::cancel_call_with_handle(shared, translator))
    }

    /// Returns the cancel effect as a split service's later event.
    ///
    /// The domain event receives the unchanged [`CancelOutcome`], including
    /// `NotAdmitted` and `AlreadyCompleted`; only the routing envelope is
    /// supplied by this helper.
    pub fn then_service_event<I, F, Event, Request>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<
                Message = tina::ServiceMessage<Event, Request>,
                Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
            >,
        F: FnOnce(CancelOutcome) -> Event + 'static,
        Event: 'static,
        Request: 'static,
    {
        self.then(move |outcome| tina::ServiceMessage::Event(translator(outcome)))
    }
}

/// Like [`crate::call`], but `.then(...)` also produces a caller-owned
/// [`tina::CallHandle`]. Pair with [`cancel_call`] to close the wait
/// later. Move-only handle: one cancel per call.
pub fn call_cancelable<T, R>(
    destination: Address<T, R>,
    message: T,
    timeout: Duration,
) -> CancelableCall<T, R>
where
    T: Send + 'static,
    R: 'static,
{
    CancelableCall::new(destination, message, timeout)
}

/// Cancelable-call sibling of [`crate::call_request`] for split service
/// requests.
///
/// This is the request-lane companion to [`call_cancelable`]. Passing an
/// event address here is a compile error, and request payloads are wrapped
/// into [`tina::ServiceMessage::Request`] before dispatch.
pub fn call_cancelable_request<E, Q, R>(
    destination: tina::ServiceRequestAddress<E, Q, R>,
    request: Q,
    timeout: Duration,
) -> CancelableCall<tina::ServiceMessage<E, Q>, R>
where
    E: Send + 'static,
    Q: Send + 'static,
    R: 'static,
{
    CancelableCall::new(
        destination.address().address(),
        tina::ServiceMessage::Request(request),
        timeout,
    )
}

/// Compatibility spelling for [`call_cancelable`].
#[deprecated(since = "0.1.0", note = "use call_cancelable")]
pub fn call_with_handle<T, R>(
    destination: Address<T, R>,
    message: T,
    timeout: Duration,
) -> CancelableCall<T, R>
where
    T: Send + 'static,
    R: 'static,
{
    call_cancelable(destination, message, timeout)
}

/// Closes the caller-side wait of one pending isolate call.
///
/// Reclaims call capacity. Does not cancel external work already
/// accepted by a backend, does not release pool leases, does not retry.
/// Late replies become typed rejected facts (`CallReplyRejected`,
/// `DeferredReplyRejected`) with reason `CallerCancelled`.
pub fn cancel_call<R>(handle: CallHandle<R>) -> CancelCallBuilder
where
    R: 'static,
{
    let inner = tina::runtime_internal::call_handle_into_inner(handle);
    CancelCallBuilder::new(inner)
}

/// Returns the runtime-assigned [`CallId`] for `handle`, or `None` if
/// its effect has not yet been dispatched.
///
/// `tina::CallHandle::call_id` returns a raw `u64` because `tina` is
/// the trait crate and must not depend on this crate's `CallId`. Use
/// this helper from runtime-aware code to keep the typed identity.
pub fn call_handle_call_id<R>(handle: &CallHandle<R>) -> Option<CallId> {
    tina::runtime_internal::call_handle_shared(handle)
        .call_id()
        .map(CallId::new)
}
