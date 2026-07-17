//! Typed WebSocket app delivery and lane classification.
//!
//! Application code accepts an upgrade with a capability-typed handle:
//!
//! - request-only → [`WebSocketUpgradeRequest::accept_request_service`]
//! - split-service → [`WebSocketUpgradeRequest::accept_split_service`]
//! - raw address remains via [`WebSocketUpgradeRequest::accept`]
//!
//! The private [`tina::ServiceMessage`] envelope stays inside this crate.
//!
//! # Lane contract
//!
//! | Message class | Delivery (Call / Split) | Authority |
//! |---------------|-------------------------|-----------|
//! | Reply-needed session work (`SessionOpen`, `SessionText`, …) | `call` into request capability | request/reply |
//! | Notifications (`SendOutcome`, `SessionClosed`, …) | observed `send` into event capability | event |
//!
//! On Call and Split installs, connection write / send-admission completions
//! do not enter the application's request lane. RequestOnly has no event
//! capability, so every session message (including notifications) is a
//! request-lane `call` by design — use Split for room fanout that needs
//! [`WebSocketSessionMsg::SendOutcome`] on an event lane. A closed or full app
//! mailbox yields the same exact terminal pressure the call path already used
//! (`AppMailboxFull` / begin close).

use std::convert::Infallible;

use tina::prelude::*;
use tina::ServiceMessage;

use crate::websocket::{
    WebSocketAccept, WebSocketError, WebSocketLimits, WebSocketSessionMsg, WebSocketSessionOutcome,
    WebSocketUpgradeRequest,
};

/// Which service lane a session message occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSocketSessionLane {
    /// Needs a typed [`WebSocketSessionOutcome`] reply before the connection
    /// continues (join ack, echo text, close response, …).
    Request,
    /// Fire-and-forget notification. Must not occupy the request lane.
    Event,
}

/// Classifies a session message for lane-correct delivery.
///
/// Notifications from the connection owner — especially
/// [`WebSocketSessionMsg::SendOutcome`] after a room fanout offer — are always
/// event-lane. Reply-needed wire work stays request-lane so the connection can
/// apply the app outcome to the stream.
pub fn websocket_session_lane(msg: &WebSocketSessionMsg) -> WebSocketSessionLane {
    match msg {
        WebSocketSessionMsg::SendOutcome(_)
        | WebSocketSessionMsg::SessionReport(_)
        | WebSocketSessionMsg::SessionAccepted { .. }
        | WebSocketSessionMsg::SessionPressure { .. }
        | WebSocketSessionMsg::SessionClosed { .. }
        | WebSocketSessionMsg::Pressure(_)
        | WebSocketSessionMsg::Closed(_)
        | WebSocketSessionMsg::AppControl(_)
        | WebSocketSessionMsg::Shutdown { .. } => WebSocketSessionLane::Event,

        WebSocketSessionMsg::Open
        | WebSocketSessionMsg::SessionOpen { .. }
        | WebSocketSessionMsg::SessionText { .. }
        | WebSocketSessionMsg::SessionBinary { .. }
        | WebSocketSessionMsg::SessionClose { .. }
        | WebSocketSessionMsg::Text(_)
        | WebSocketSessionMsg::Binary(_)
        | WebSocketSessionMsg::Ping(_)
        | WebSocketSessionMsg::Pong(_)
        | WebSocketSessionMsg::Close(_, _) => WebSocketSessionLane::Request,
    }
}

/// How the connection owner delivers one session message to the app.
///
/// Manually `Copy`/`Clone`: both arms only store an [`Address`].
#[derive(Debug)]
pub(crate) enum WebSocketAppDelivery {
    /// Every message uses the raw session address.
    ///
    /// Request-lane messages still `call`; event-lane messages use observed
    /// send so write completions never wait on a request reply.
    Call {
        address: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
    },
    /// Split-service envelope with `WebSocketSessionMsg` on both lanes.
    ///
    /// Request-lane → `ServiceMessage::Request` via `call`.
    /// Event-lane → `ServiceMessage::Event` via observed send.
    Split {
        address: Address<
            ServiceMessage<WebSocketSessionMsg, WebSocketSessionMsg>,
            WebSocketSessionOutcome,
        >,
    },
    /// Request-only service: every message is a request-lane `call`.
    ///
    /// Use this for echo-style apps. Room fanout that needs
    /// [`WebSocketSessionMsg::SendOutcome`] on an event lane should install a
    /// split-service handle instead.
    RequestOnly {
        address: Address<ServiceMessage<Infallible, WebSocketSessionMsg>, WebSocketSessionOutcome>,
    },
}

impl Copy for WebSocketAppDelivery {}

impl Clone for WebSocketAppDelivery {
    fn clone(&self) -> Self {
        *self
    }
}

impl WebSocketAppDelivery {
    pub(crate) fn call(address: Address<WebSocketSessionMsg, WebSocketSessionOutcome>) -> Self {
        Self::Call { address }
    }

    pub(crate) fn split(
        address: Address<
            ServiceMessage<WebSocketSessionMsg, WebSocketSessionMsg>,
            WebSocketSessionOutcome,
        >,
    ) -> Self {
        Self::Split { address }
    }

    pub(crate) fn request_only(
        address: Address<ServiceMessage<Infallible, WebSocketSessionMsg>, WebSocketSessionOutcome>,
    ) -> Self {
        Self::RequestOnly { address }
    }

    /// Address used only for diagnostics / legacy `WebSocketAccept::app`.
    ///
    /// Split and request-only deliveries do not expose a raw
    /// `Address<WebSocketSessionMsg, _>`; callers that need the app should
    /// hold the typed handle they installed.
    pub(crate) fn legacy_app_address(
        self,
    ) -> Option<Address<WebSocketSessionMsg, WebSocketSessionOutcome>> {
        match self {
            Self::Call { address } => Some(address),
            Self::Split { .. } | Self::RequestOnly { .. } => None,
        }
    }
}

impl WebSocketUpgradeRequest {
    /// Accept with a typed request-only service handle.
    ///
    /// The private service envelope is never named at the call site.
    ///
    /// ```compile_fail
    /// use tina_http::{WebSocketLimits, WebSocketSessionMsg, websocket_upgrade};
    /// use tina_runtime::EventServiceHandle;
    ///
    /// fn bad(events: EventServiceHandle<WebSocketSessionMsg>, limits: WebSocketLimits) {
    ///     // Event handles have no request lane for reply-needed session work.
    ///     let _ = events;
    ///     let _ = limits;
    ///     let upgrade: tina_http::WebSocketUpgradeRequest = unreachable!();
    ///     let _ = upgrade.accept_request_service(events, limits);
    /// }
    /// ```
    pub fn accept_request_service(
        self,
        requests: tina_runtime::RequestServiceHandle<WebSocketSessionMsg, WebSocketSessionOutcome>,
        limits: WebSocketLimits,
    ) -> WebSocketAccept {
        WebSocketAccept::from_parts(
            self.accept_key,
            None,
            WebSocketAppDelivery::request_only(requests.address().address()),
            limits,
        )
    }

    /// Accept with a typed split-service handle.
    ///
    /// Reply-needed session messages use the request lane; notifications use
    /// the event lane. The private envelope is not named at the call site.
    ///
    /// ```compile_fail
    /// use tina_http::{WebSocketLimits, WebSocketSessionMsg, WebSocketSessionOutcome};
    /// use tina_runtime::RequestServiceHandle;
    ///
    /// fn bad(
    ///     requests: RequestServiceHandle<WebSocketSessionMsg, WebSocketSessionOutcome>,
    ///     limits: WebSocketLimits,
    /// ) {
    ///     // accept_split_service requires a SplitServiceHandle, not request-only.
    ///     let upgrade: tina_http::WebSocketUpgradeRequest = unreachable!();
    ///     let _ = upgrade.accept_split_service(requests, limits);
    /// }
    /// ```
    pub fn accept_split_service(
        self,
        handle: tina_runtime::SplitServiceHandle<
            WebSocketSessionMsg,
            WebSocketSessionMsg,
            WebSocketSessionOutcome,
        >,
        limits: WebSocketLimits,
    ) -> WebSocketAccept {
        WebSocketAccept::from_parts(
            self.accept_key,
            None,
            WebSocketAppDelivery::split(handle.address()),
            limits,
        )
    }

    /// Accept with a selected subprotocol and a typed split-service handle.
    pub fn accept_split_service_subprotocol(
        self,
        handle: tina_runtime::SplitServiceHandle<
            WebSocketSessionMsg,
            WebSocketSessionMsg,
            WebSocketSessionOutcome,
        >,
        limits: WebSocketLimits,
        subprotocol: impl Into<String>,
    ) -> Result<WebSocketAccept, WebSocketError> {
        let subprotocol = subprotocol.into();
        self.ensure_subprotocol(&subprotocol)?;
        Ok(WebSocketAccept::from_parts(
            self.accept_key,
            Some(subprotocol),
            WebSocketAppDelivery::split(handle.address()),
            limits,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket::{WebSocketCloseCode, WebSocketSendError, WebSocketSendOutcome, WebSocketSessionId};

    #[test]
    fn send_outcome_is_event_lane() {
        let msg = WebSocketSessionMsg::SendOutcome(WebSocketSendOutcome {
            session: WebSocketSessionId::new(0),
            result: Ok(()),
        });
        assert_eq!(websocket_session_lane(&msg), WebSocketSessionLane::Event);
    }

    #[test]
    fn session_closed_is_event_lane() {
        let msg = WebSocketSessionMsg::SessionClosed {
            session_id: WebSocketSessionId::new(0),
            error: crate::websocket::WebSocketError::PeerClosed,
        };
        assert_eq!(websocket_session_lane(&msg), WebSocketSessionLane::Event);
    }

    #[test]
    fn session_text_is_request_lane() {
        let msg = WebSocketSessionMsg::SessionText {
            session_id: WebSocketSessionId::new(0),
            text: "hi".into(),
        };
        assert_eq!(websocket_session_lane(&msg), WebSocketSessionLane::Request);
    }

    #[test]
    fn session_open_is_request_lane() {
        // Handle construction needs a dummy address — only the variant matters.
        let msg = WebSocketSessionMsg::Open;
        assert_eq!(websocket_session_lane(&msg), WebSocketSessionLane::Request);
    }

    #[test]
    fn send_error_variants_stay_event_lane() {
        for result in [
            Err(WebSocketSendError::Closed),
            Err(WebSocketSendError::Stale),
            Err(WebSocketSendError::Timeout),
            Err(WebSocketSendError::OutboundQueueFull),
        ] {
            let msg = WebSocketSessionMsg::SendOutcome(WebSocketSendOutcome {
                session: WebSocketSessionId::new(1),
                result,
            });
            assert_eq!(websocket_session_lane(&msg), WebSocketSessionLane::Event);
        }
    }

    #[test]
    fn close_code_request_lane() {
        let msg = WebSocketSessionMsg::SessionClose {
            session_id: WebSocketSessionId::new(0),
            code: Some(WebSocketCloseCode(1000)),
            reason: Vec::new(),
        };
        assert_eq!(websocket_session_lane(&msg), WebSocketSessionLane::Request);
    }
}
