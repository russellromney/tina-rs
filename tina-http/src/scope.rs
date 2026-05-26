//! HTTP rail adapters for [`RequestScope`].
//!
//! A request is a tree. When the HTTP caller goes away, the rails the
//! request opened against the connection — the parked request-body pull,
//! the response-body source, a WebSocket send it owns, a gRPC unary call —
//! should stop waiting. This module wires those HTTP-owned rails into the
//! runtime's [`RequestScope`] cancel vocabulary, plus the honest escape
//! hatch for rails that cannot be scope-cancelled.
//!
//! ## The scope is the operation, never the session
//!
//! A long-lived WebSocket session is **not** a short request scope. These
//! adapters scope *one operation a request owns* — a single send, a single
//! report, a single close, a single unary call. The session keeps living;
//! only the operation's parked wait is registered for scope cancel.
//!
//! ## What is and is not scopeable
//!
//! - **Request-body pull**, **WebSocket send/report/close**, and
//!   **gRPC unary** all reduce to one cancelable call against a connection
//!   isolate. Registering the call's handle into the scope means a scope
//!   cancel (client disconnect, request timeout, owner stop) closes that
//!   parked wait and reclaims the caller's pending-reply slot. See
//!   [`scoped_operation`] and its named wrappers.
//! - **Response-body source**: the source isolate owns downstream
//!   resources. The honest cancel is the protocol's own
//!   [`ResponseChunkMsg::Cancel`]. See [`cancel_response_source`].
//! - **A buffered body already delivered to the handler** has no rail to
//!   cancel — the bytes are in hand. That is not a failure to hide; it is
//!   an [`UnsupportedScopeRow`](tina_runtime::UnsupportedScopeRow) the
//!   service records so the request report stays honest.

use std::time::Duration;

use tina::Address;
use tina::prelude::*;
use tina_runtime::{
    CallOutcome, RequestScope, RuntimeCall, ScopeCancelCause, ScopeRegisterError, call,
    call_cancelable,
};

use crate::streaming::{RequestChunkReply, ResponseChunkMsg, ResponseChunkReply};
use crate::websocket::WebSocketMessage;
use crate::{
    Http2ClientMsg, Http2ClientReply, HttpConnectionMsg, WebSocketCloseCode, WebSocketSessionHandle,
};

/// Why a scoped rail could not be started.
///
/// Both variants leave the caller's authority untouched: the outcome
/// continuation (which usually captures the caller's `RequestContext`) is
/// never built, so the service can answer the caller deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopedRailRejected {
    /// The scope is already cancelled. Answer the caller with the cause
    /// (or, on a disconnect, drop the request — the caller is gone).
    ScopeCancelled {
        /// Reason the scope was cancelled.
        cause: ScopeCancelCause,
    },
    /// The scope's child cap is full: the request opened more child rails
    /// than its scope budgeted. A service-side budgeting bug.
    ScopeFull {
        /// Configured child cap.
        cap: usize,
    },
}

/// Registers one cancelable call against a connection isolate into
/// `scope`, returning the call effect.
///
/// This is the shared core behind the named HTTP-rail adapters. A later
/// scope cancel closes the parked wait; `on_outcome` still delivers the
/// reply on the normal worker-return path until then.
///
/// The scope state is pre-checked before any effect is built, so on
/// rejection the caller authority captured inside `on_outcome` is never
/// consumed. Within one isolate handler nothing else runs between the
/// pre-check and the register, so the register cannot fail for a different
/// reason than the pre-check found.
pub fn scoped_operation<I, M, R, Msg, F>(
    scope: &RequestScope,
    destination: Address<M, R>,
    message: M,
    label: &'static str,
    timeout: Duration,
    on_outcome: F,
) -> Result<Effect<I>, ScopedRailRejected>
where
    I: Isolate<Message = Msg, Call = RuntimeCall<Msg>>,
    M: Send + 'static,
    R: 'static,
    F: FnOnce(CallOutcome<R>) -> Msg + 'static,
    Msg: 'static,
{
    if let Some(cause) = scope.cancel_cause() {
        return Err(ScopedRailRejected::ScopeCancelled { cause });
    }
    if scope.registered() >= scope.child_cap() {
        return Err(ScopedRailRejected::ScopeFull {
            cap: scope.child_cap(),
        });
    }
    let (effect, handle) = call_cancelable(destination, message, timeout).then(on_outcome);
    // The pre-checks above guarantee success on a single-shard handler;
    // map the (unreachable) error paths to the recoverable variant rather
    // than panicking, and without requiring `R: Debug`.
    match scope.register(label, handle) {
        Ok(()) => Ok(effect),
        Err(ScopeRegisterError::Cancelled { cause, .. }) => {
            Err(ScopedRailRejected::ScopeCancelled { cause })
        }
        Err(ScopeRegisterError::Full { cap, .. }) => Err(ScopedRailRejected::ScopeFull { cap }),
    }
}

/// Starts a request-body pull whose parked wait is owned by `scope`.
///
/// Equivalent to [`scoped_operation`] with the connection's
/// `HttpConnectionMsg::body_next()` message. A later scope cancel closes
/// the parked pull.
pub fn scoped_request_body_pull<I, Msg, F>(
    scope: &RequestScope,
    stream_source: Address<HttpConnectionMsg, RequestChunkReply>,
    label: &'static str,
    timeout: Duration,
    on_chunk: F,
) -> Result<Effect<I>, ScopedRailRejected>
where
    I: Isolate<Message = Msg, Call = RuntimeCall<Msg>>,
    F: FnOnce(CallOutcome<RequestChunkReply>) -> Msg + 'static,
    Msg: 'static,
{
    scoped_operation(
        scope,
        stream_source,
        HttpConnectionMsg::body_next(),
        label,
        timeout,
        on_chunk,
    )
}

/// Scopes a single WebSocket **send** a request owns. The session keeps
/// living; only this send's parked wait is registered for scope cancel.
pub fn scoped_websocket_send<I, Msg, F>(
    scope: &RequestScope,
    session: WebSocketSessionHandle,
    message: WebSocketMessage,
    label: &'static str,
    timeout: Duration,
    on_outcome: F,
) -> Result<Effect<I>, ScopedRailRejected>
where
    I: Isolate<Message = Msg, Call = RuntimeCall<Msg>>,
    F: FnOnce(CallOutcome<RequestChunkReply>) -> Msg + 'static,
    Msg: 'static,
{
    scoped_operation(
        scope,
        session.target(),
        session.send(message),
        label,
        timeout,
        on_outcome,
    )
}

/// Scopes a single WebSocket **report** (point-in-time diagnostic
/// snapshot) a request owns.
pub fn scoped_websocket_report<I, Msg, F>(
    scope: &RequestScope,
    session: WebSocketSessionHandle,
    label: &'static str,
    timeout: Duration,
    on_outcome: F,
) -> Result<Effect<I>, ScopedRailRejected>
where
    I: Isolate<Message = Msg, Call = RuntimeCall<Msg>>,
    F: FnOnce(CallOutcome<RequestChunkReply>) -> Msg + 'static,
    Msg: 'static,
{
    scoped_operation(
        scope,
        session.target(),
        session.report(),
        label,
        timeout,
        on_outcome,
    )
}

/// Scopes a single WebSocket **close** a request owns. Closing the session
/// is an operation the request can abandon; the close-send's wait is the
/// scoped child, not the session itself.
pub fn scoped_websocket_close<I, Msg, F>(
    scope: &RequestScope,
    session: WebSocketSessionHandle,
    code: Option<WebSocketCloseCode>,
    reason: impl Into<Vec<u8>>,
    label: &'static str,
    timeout: Duration,
    on_outcome: F,
) -> Result<Effect<I>, ScopedRailRejected>
where
    I: Isolate<Message = Msg, Call = RuntimeCall<Msg>>,
    F: FnOnce(CallOutcome<RequestChunkReply>) -> Msg + 'static,
    Msg: 'static,
{
    scoped_operation(
        scope,
        session.target(),
        session.close(code, reason),
        label,
        timeout,
        on_outcome,
    )
}

/// Scopes a single gRPC **unary** call a request owns. Build `submit` with
/// `GrpcClient::unary_request(...)`; this registers the outbound call's
/// parked wait so a scope cancel closes it. The upstream may still finish
/// late — that is a visible rejected trace fact, not a delivered success.
pub fn scoped_grpc_unary<I, Msg, F>(
    scope: &RequestScope,
    connection: Address<Http2ClientMsg, Http2ClientReply>,
    submit: Http2ClientMsg,
    label: &'static str,
    timeout: Duration,
    on_outcome: F,
) -> Result<Effect<I>, ScopedRailRejected>
where
    I: Isolate<Message = Msg, Call = RuntimeCall<Msg>>,
    F: FnOnce(CallOutcome<Http2ClientReply>) -> Msg + 'static,
    Msg: 'static,
{
    scoped_operation(scope, connection, submit, label, timeout, on_outcome)
}

/// Tells a response-body source to stop producing and release its
/// resources, the protocol-honest cancel for the response side.
///
/// This is the same [`ResponseChunkMsg::Cancel`] the connection sends when
/// the client disconnects mid-response. A service that owns the source
/// address (for example, because it is cancelling its own request scope)
/// uses this to release the source's downstream work — there is no late
/// "ghost" chunk because the source observes the cancel and stops.
pub fn cancel_response_source<I, F, M>(
    source: Address<ResponseChunkMsg, ResponseChunkReply>,
    timeout: Duration,
    translator: F,
) -> Effect<I>
where
    I: Isolate<Message = M, Call = RuntimeCall<M>>,
    F: FnOnce(CallOutcome<ResponseChunkReply>) -> M + 'static,
    M: 'static,
{
    call(source, ResponseChunkMsg::Cancel, timeout).then(translator)
}

#[cfg(test)]
mod tests {
    //! In-crate proofs for the session-operation wrappers. The
    //! `WebSocketSessionHandle` constructor is `pub(crate)`, so these live
    //! here rather than in an integration test. They prove the wrappers
    //! delegate the scope-admission decision to `scoped_operation`: a fresh
    //! scope admits, a full scope refuses, and a cancelled scope refuses —
    //! all without dispatching the effect (no runtime stepping needed).

    use super::*;
    use std::convert::Infallible;
    use tina_runtime::{
        DefaultThreadedMailboxFactory, RequestScopeId, RequestScopeSetCapacityReport,
        ScopedRequestReport, ThreadedRuntime,
    };

    use crate::websocket::WebSocketSessionId;

    // The connection isolate the adapters call: its message *is*
    // `HttpConnectionMsg` (like the real connection / WebSocket owner). We
    // never dispatch the built effect, so its handlers do nothing.
    #[derive(Default)]
    struct FakeConn;

    impl Isolate for FakeConn {
        tina::isolate_types! {
            message: HttpConnectionMsg,
            reply: RequestChunkReply,
            send: tina::Outbound<Infallible>,
            spawn: Infallible,
            call: RuntimeCall<HttpConnectionMsg>,
            shard: SingleShard,
        }

        fn handle(
            &mut self,
            _msg: HttpConnectionMsg,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            noop()
        }

        fn handle_call(
            &mut self,
            _msg: HttpConnectionMsg,
            call: tina::CallContext<'_, Self>,
        ) -> Effect<Self> {
            call.reject(tina::CallRejectedReason::UnsupportedMessage)
        }
    }

    // The service isolate that *owns* the request scope. It is the `I`
    // type param: its message holds the operation outcome. We only need
    // the type; no instance is registered, so the payloads are never read.
    #[allow(dead_code)]
    enum SvcMsg {
        WsOutcome(CallOutcome<RequestChunkReply>),
        GrpcOutcome(CallOutcome<Http2ClientReply>),
    }

    struct Svc;

    impl Isolate for Svc {
        tina::isolate_types! {
            message: SvcMsg,
            reply: (),
            send: tina::Outbound<Infallible>,
            spawn: Infallible,
            call: RuntimeCall<SvcMsg>,
            shard: SingleShard,
        }

        fn handle(
            &mut self,
            _msg: SvcMsg,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            noop()
        }
    }

    #[test]
    fn scoped_websocket_send_admits_then_refuses_when_full_or_cancelled() {
        let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
        let target = runtime
            .register_with_capacity::<FakeConn, Infallible>(FakeConn, 4)
            .expect("register fake conn");
        let session = WebSocketSessionHandle::new(WebSocketSessionId::new(1), target);

        // child_cap 1: the first scoped send admits and fills the scope.
        let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 1);
        let first = scoped_websocket_send::<Svc, _, _>(
            &scope,
            session,
            WebSocketMessage::Text("hi".to_owned()),
            "ws_send",
            Duration::from_secs(1),
            SvcMsg::WsOutcome,
        );
        assert!(first.is_ok(), "fresh scope must admit the send");
        assert_eq!(scope.registered(), 1, "the send registered as a child");

        // Second send overflows the child cap; the wrapper refuses.
        let second = scoped_websocket_send::<Svc, _, _>(
            &scope,
            session,
            WebSocketMessage::Text("again".to_owned()),
            "ws_send",
            Duration::from_secs(1),
            SvcMsg::WsOutcome,
        );
        assert!(
            matches!(second, Err(ScopedRailRejected::ScopeFull { cap }) if cap == 1),
            "a full child cap must refuse the second send",
        );

        // A cancelled scope refuses with the cause.
        let cancelled = RequestScope::with_child_cap(RequestScopeId::alloc(), 2);
        let report = cancelled.cancel_synchronously(ScopeCancelCause::ClientDisconnect);
        let refused = scoped_websocket_report::<Svc, _, _>(
            &cancelled,
            session,
            "ws_report",
            Duration::from_secs(1),
            SvcMsg::WsOutcome,
        );
        assert!(
            matches!(
                refused,
                Err(ScopedRailRejected::ScopeCancelled {
                    cause: ScopeCancelCause::ClientDisconnect
                })
            ),
            "a cancelled scope must refuse with the cancel cause",
        );
        // The close wrapper takes the same path.
        let close_refused = scoped_websocket_close::<Svc, _, _>(
            &cancelled,
            session,
            None,
            Vec::new(),
            "ws_close",
            Duration::from_secs(1),
            SvcMsg::WsOutcome,
        );
        assert!(matches!(
            close_refused,
            Err(ScopedRailRejected::ScopeCancelled { .. })
        ));
        // No children were registered on the cancelled scope — a clean,
        // honest teardown.
        assert_eq!(report.children.len(), 0);

        let _ = runtime.shutdown();
    }

    #[derive(Default)]
    struct FakeH2;

    impl Isolate for FakeH2 {
        tina::isolate_types! {
            message: Http2ClientMsg,
            reply: Http2ClientReply,
            send: tina::Outbound<Infallible>,
            spawn: Infallible,
            call: RuntimeCall<Http2ClientMsg>,
            shard: SingleShard,
        }

        fn handle(
            &mut self,
            _msg: Http2ClientMsg,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            noop()
        }

        fn handle_call(
            &mut self,
            _msg: Http2ClientMsg,
            call: tina::CallContext<'_, Self>,
        ) -> Effect<Self> {
            call.reject(tina::CallRejectedReason::UnsupportedMessage)
        }
    }

    #[test]
    fn scoped_grpc_unary_admits_into_a_fresh_scope() {
        // Proves the gRPC wrapper drives `scoped_operation` over a
        // non-connection address type. The message content is irrelevant
        // to scope admission; `Begin` is a cheap stand-in.
        let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
        let connection = runtime
            .register_with_capacity::<FakeH2, Infallible>(FakeH2, 4)
            .expect("register fake h2");
        let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 1);
        let admitted = scoped_grpc_unary::<Svc, _, _>(
            &scope,
            connection,
            Http2ClientMsg::Begin,
            "grpc_unary",
            Duration::from_secs(1),
            SvcMsg::GrpcOutcome,
        );
        assert!(admitted.is_ok(), "fresh scope admits the unary call");
        assert_eq!(scope.registered(), 1, "the call registered as a child");

        // A request that used only this rail tears down clean.
        let report = ScopedRequestReport::new(
            scope.cancel_synchronously(ScopeCancelCause::Timeout),
            RequestScopeSetCapacityReport {
                in_use: 0,
                capacity: 1,
            },
        );
        assert!(report.is_clean());

        let _ = runtime.shutdown();
    }
}
