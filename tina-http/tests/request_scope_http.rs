//! Proofs for the HTTP rail adapters in `tina_http::scope`.
//!
//! Three rails, three honesty checks:
//!
//! - A scoped request-body pull is a real parked wait. A scope cancel
//!   closes it (`CancelOutcome::Cancelled`) and the chunk continuation
//!   never fires — the parked pull authority is released.
//! - A response-body source receives `ResponseChunkMsg::Cancel` and stops
//!   producing — the protocol-honest response-side cancel.
//! - A rail with no cancel handle (a buffered body already in hand) is
//!   reported as an `UnsupportedScopeRow`, and the request report fails
//!   closed (`is_clean() == false`) rather than pretending it was
//!   cancelled.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_http::HttpConnectionMsg;
use tina_http::scope::{cancel_response_source, scoped_request_body_pull};
use tina_http::streaming::{RequestChunkReply, ResponseChunkMsg, ResponseChunkReply};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RequestScope, RequestScopeId,
    RequestScopeSetCapacityReport, RuntimeEventKind, ScopeCancelCause, ScopedRequestReport,
    ThreadedRuntime,
};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_BUDGET: Duration = Duration::from_secs(5);

fn wait_for(total: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + total;
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

// --- Fake connection source: parks the body_next pull like the real
// connection isolate does when no chunk is ready yet. ---

#[derive(Debug, Default)]
struct FakeBodySource {
    held: Option<DeferredReply<RequestChunkReply>>,
}

#[tina_runtime::isolate(message = HttpConnectionMsg, reply = RequestChunkReply)]
impl FakeBodySource {
    fn handle(
        &mut self,
        _msg: HttpConnectionMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(
        &mut self,
        msg: HttpConnectionMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            HttpConnectionMsg::RequestBodyNext => {
                // Park the pull: hold the deferred slot, never reply. A
                // scope cancel must be what closes this wait.
                self.held = Some(call.into_request_context().into_deferred());
                noop()
            }
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

// --- Driver: starts a scoped body pull, then cancels the scope. ---

#[derive(Debug)]
enum PullDriverMsg {
    Begin,
    Cancel,
    Chunk(CallOutcome<RequestChunkReply>),
    ChildCancelled(RequestScopeId, &'static str, CancelOutcome),
}

#[derive(Debug, Default, Clone)]
struct PullObservations {
    chunks: Vec<CallOutcome<RequestChunkReply>>,
    cancel_acks: Vec<(RequestScopeId, &'static str, CancelOutcome)>,
}

struct PullDriver {
    source: Address<HttpConnectionMsg, RequestChunkReply>,
    scope: RequestScope,
    obs: Arc<Mutex<PullObservations>>,
}

#[tina_runtime::isolate(message = PullDriverMsg)]
impl PullDriver {
    fn handle(
        &mut self,
        msg: PullDriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PullDriverMsg::Begin => scoped_request_body_pull(
                &self.scope,
                self.source,
                "http.request_body_pull",
                CALL_TIMEOUT,
                PullDriverMsg::Chunk,
            )
            .expect("scope is fresh, pull admits"),
            PullDriverMsg::Cancel => {
                let (_report, effects) = self.scope.cancel_into_effects::<Self, _, _>(
                    ScopeCancelCause::ClientDisconnect,
                    PullDriverMsg::ChildCancelled,
                );
                if effects.is_empty() {
                    noop()
                } else {
                    Effect::Batch(effects)
                }
            }
            PullDriverMsg::Chunk(outcome) => {
                self.obs.lock().expect("obs").chunks.push(outcome);
                noop()
            }
            PullDriverMsg::ChildCancelled(id, label, outcome) => {
                self.obs
                    .lock()
                    .expect("obs")
                    .cancel_acks
                    .push((id, label, outcome));
                noop()
            }
        }
    }
}

#[test]
fn scoped_request_body_pull_is_closed_by_scope_cancel() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let source = runtime
        .register_with_capacity::<_, Infallible>(FakeBodySource::default(), 8)
        .expect("source");
    let obs = Arc::new(Mutex::new(PullObservations::default()));
    let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            PullDriver {
                source,
                scope: scope.clone(),
                obs: obs.clone(),
            },
            32,
        )
        .expect("driver");

    runtime
        .try_send(driver, PullDriverMsg::Begin)
        .expect("begin");
    runtime
        .try_send(driver, PullDriverMsg::Cancel)
        .expect("cancel");

    let obs_for_wait = obs.clone();
    assert!(
        wait_for(POLL_BUDGET, || {
            !obs_for_wait.lock().expect("obs").cancel_acks.is_empty()
        }),
        "driver never observed the cancel ack",
    );

    let snapshot = obs.lock().expect("obs").clone();
    let events: Vec<RuntimeEventKind> = runtime
        .complete_trace()
        .expect("trace")
        .into_iter()
        .map(|e| e.kind())
        .collect();
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }

    assert_eq!(snapshot.cancel_acks.len(), 1, "one ack for the body pull");
    let (id, label, outcome) = snapshot.cancel_acks[0];
    assert_eq!(id, scope.id());
    assert_eq!(label, "http.request_body_pull");
    assert_eq!(
        outcome,
        CancelOutcome::Cancelled,
        "the parked body pull wait must be closed by the scope cancel",
    );
    assert!(
        snapshot.chunks.is_empty(),
        "a cancelled body pull must not deliver a chunk; got {snapshot:?}",
    );
    let cancelled = events.iter().any(|kind| {
        matches!(
            kind,
            RuntimeEventKind::CallCancelled {
                cause: tina::CancelCause::CallerCancelled,
                ..
            }
        )
    });
    assert!(cancelled, "trace must record the CallCancelled fact");
}

#[test]
fn scoped_request_body_pull_rejects_when_scope_already_cancelled() {
    // A pre-cancelled scope rejects a new pull without consuming the chunk
    // continuation, so the caller can answer the gone client deliberately.
    // The helper short-circuits before building any effect; the source is
    // registered only so we have a real address to pass.
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let source = runtime
        .register_with_capacity::<_, Infallible>(FakeBodySource::default(), 4)
        .expect("source");
    let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 2);
    let _ = scope.cancel_synchronously(ScopeCancelCause::Timeout);

    let result = scoped_request_body_pull::<PullDriver, _, _>(
        &scope,
        source,
        "http.request_body_pull",
        CALL_TIMEOUT,
        PullDriverMsg::Chunk,
    );
    match result {
        Err(tina_http::ScopedRailRejected::ScopeCancelled { cause }) => {
            assert_eq!(cause, ScopeCancelCause::Timeout);
        }
        Err(other) => panic!("expected ScopeCancelled, got {other:?}"),
        Ok(_) => panic!("expected ScopeCancelled, got an admitted pull effect"),
    }
    let _ = runtime.shutdown();
}

// --- Response-source cancel ---

#[derive(Debug, Default)]
struct FakeResponseSource {
    cancelled: Arc<Mutex<bool>>,
}

#[tina_runtime::isolate(message = ResponseChunkMsg, reply = ResponseChunkReply)]
impl FakeResponseSource {
    fn handle(
        &mut self,
        _msg: ResponseChunkMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(
        &mut self,
        msg: ResponseChunkMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            ResponseChunkMsg::Cancel => {
                *self.cancelled.lock().expect("cancelled") = true;
                // Acknowledge the cancel; duplicate cancels are harmless.
                call.reply(ResponseChunkReply::Eof)
            }
            ResponseChunkMsg::Next => call.reply(ResponseChunkReply::Chunk(vec![1, 2, 3])),
            ResponseChunkMsg::Http2RequestChunk(_) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[derive(Debug)]
enum CancelDriverMsg {
    Begin,
    Acked(CallOutcome<ResponseChunkReply>),
}

struct CancelDriver {
    source: Address<ResponseChunkMsg, ResponseChunkReply>,
    acked: Arc<Mutex<bool>>,
}

#[tina_runtime::isolate(message = CancelDriverMsg)]
impl CancelDriver {
    fn handle(
        &mut self,
        msg: CancelDriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CancelDriverMsg::Begin => {
                cancel_response_source(self.source, CALL_TIMEOUT, CancelDriverMsg::Acked)
            }
            CancelDriverMsg::Acked(outcome) => {
                // The source acknowledged the cancel (no late ghost chunk).
                if matches!(outcome, CallOutcome::Replied(ResponseChunkReply::Eof)) {
                    *self.acked.lock().expect("acked") = true;
                }
                noop()
            }
        }
    }
}

#[test]
fn cancel_response_source_reaches_the_source() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let cancelled = Arc::new(Mutex::new(false));
    let source = runtime
        .register_with_capacity::<_, Infallible>(
            FakeResponseSource {
                cancelled: cancelled.clone(),
            },
            8,
        )
        .expect("source");
    let acked = Arc::new(Mutex::new(false));
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            CancelDriver {
                source,
                acked: acked.clone(),
            },
            8,
        )
        .expect("driver");

    runtime
        .try_send(driver, CancelDriverMsg::Begin)
        .expect("begin");

    let cancelled_for_wait = cancelled.clone();
    assert!(
        wait_for(POLL_BUDGET, || *cancelled_for_wait.lock().expect("c")),
        "the response source never observed ResponseChunkMsg::Cancel",
    );
    assert!(
        wait_for(POLL_BUDGET, || *acked.lock().expect("a")),
        "the driver never saw the cancel ack",
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn buffered_body_rail_fails_closed_as_unsupported() {
    // A buffered body already delivered to the handler has no rail to
    // cancel. The request report must say so out loud, not pretend.
    let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 2);
    let cancel = scope.cancel_synchronously(ScopeCancelCause::ClientDisconnect);
    let capacity = RequestScopeSetCapacityReport {
        in_use: 0,
        capacity: 1,
    };
    let report = ScopedRequestReport::new(cancel, capacity).with_unsupported(
        "buffered_request_body",
        "body bytes already delivered to the handler; no cancel handle",
    );
    assert!(
        !report.is_clean(),
        "a request that used an uncancelable rail must not report clean",
    );
    assert_eq!(report.unsupported.len(), 1);
    assert_eq!(report.unsupported[0].rail, "buffered_request_body");
}
