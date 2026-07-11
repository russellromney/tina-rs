use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    CallError, CallGroup, CallGroupToken, CallJoinSet, CallOutcome, CallSelectSet,
    DefaultThreadedMailboxFactory, FileId, RequestScope, RequestScopeId, ScopeCancelCause,
    SplitServiceHandle, ThreadedRuntime, call, call_cancelable, cancel_call, file_close, sleep,
};

const TIMEOUT: Duration = Duration::from_secs(1);
const POLL_BUDGET: Duration = Duration::from_secs(5);

fn wait_for(total: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

#[derive(Debug)]
enum ProbeMessage {
    Value,
    Hold,
    Held(tina::RequestContext<u32>, Result<(), CallError>),
}

struct Probe;

#[tina_runtime::isolate(message = ProbeMessage, reply = u32)]
impl Probe {
    fn handle(
        &mut self,
        message: ProbeMessage,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ProbeMessage::Held(request, result) => {
                result.expect("hold sleep succeeds");
                reply_to(request, 9)
            }
            ProbeMessage::Value | ProbeMessage::Hold => noop(),
        }
    }

    fn handle_call(&mut self, message: ProbeMessage, call: CallContext<'_, Self>) -> Effect<Self> {
        match message {
            ProbeMessage::Value => call.reply(7),
            ProbeMessage::Hold => call
                .defer(sleep(Duration::from_millis(250)))
                .reply(ProbeMessage::Held),
            ProbeMessage::Held(_, _) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug)]
enum ServiceEvent {
    StartOrdinary,
    StartCancellation,
    Slept(Result<(), CallError>),
    FileClosed(Result<(), CallError>),
    Called(CallOutcome<u32>),
    CancelableCalled(CallOutcome<u32>),
    Cancelled(tina::CancelOutcome),
    DeferredSleep(tina::RequestContext<u32>, Result<(), CallError>),
    DeferredCall(tina::RequestContext<u32>, CallOutcome<u32>),
    FlowCall(tina::RequestContext<u32>, CallOutcome<u32>),
}

#[derive(Debug)]
enum ServiceRequest {
    DeferredSleep,
    DeferredCall,
    FlowCall,
}

struct Service {
    probe: Address<ProbeMessage, u32>,
    observed: Arc<Mutex<Vec<&'static str>>>,
}

#[tina_runtime::isolate(event = ServiceEvent, request = ServiceRequest, reply = u32)]
impl Service {
    fn handle_event(
        &mut self,
        event: ServiceEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            ServiceEvent::StartOrdinary => {
                let slept = sleep(Duration::from_millis(1)).then_service_event(ServiceEvent::Slept);
                let file_closed =
                    file_close(FileId::new(u64::MAX)).then_service_event(ServiceEvent::FileClosed);
                let called = call(self.probe, ProbeMessage::Value, TIMEOUT)
                    .then_service_event(ServiceEvent::Called);
                let (cancelable, handle) =
                    call_cancelable(self.probe, ProbeMessage::Value, TIMEOUT)
                        .then_service_event(ServiceEvent::CancelableCalled);
                drop(handle);
                Effect::Batch(vec![slept, file_closed, called, cancelable])
            }
            ServiceEvent::StartCancellation => {
                let (pending, handle) = call_cancelable(self.probe, ProbeMessage::Hold, TIMEOUT)
                    .then_service_event(ServiceEvent::CancelableCalled);
                let cancel = cancel_call(handle).then_service_event(ServiceEvent::Cancelled);
                Effect::Batch(vec![pending, cancel])
            }
            ServiceEvent::Slept(result) => {
                result.expect("sleep succeeds");
                self.observed.lock().unwrap().push("sleep");
                noop()
            }
            ServiceEvent::FileClosed(result) => {
                assert!(result.is_err(), "unknown file id must stay a typed error");
                self.observed.lock().unwrap().push("typed_call");
                noop()
            }
            ServiceEvent::Called(CallOutcome::Replied(7)) => {
                self.observed.lock().unwrap().push("call");
                noop()
            }
            ServiceEvent::CancelableCalled(CallOutcome::Replied(7)) => {
                self.observed.lock().unwrap().push("cancelable");
                noop()
            }
            ServiceEvent::Cancelled(tina::CancelOutcome::Cancelled) => {
                self.observed.lock().unwrap().push("cancelled");
                noop()
            }
            ServiceEvent::Cancelled(other) => panic!("unexpected cancel outcome: {other:?}"),
            ServiceEvent::Called(other) | ServiceEvent::CancelableCalled(other) => {
                panic!("unexpected call outcome: {other:?}")
            }
            ServiceEvent::DeferredSleep(request, result) => {
                result.expect("deferred sleep succeeds");
                reply_to(request, 11)
            }
            ServiceEvent::DeferredCall(request, CallOutcome::Replied(value)) => {
                reply_to(request, value + 10)
            }
            ServiceEvent::FlowCall(request, CallOutcome::Replied(value)) => {
                reply_to(request, value + 20)
            }
            ServiceEvent::DeferredCall(_, other) | ServiceEvent::FlowCall(_, other) => {
                panic!("unexpected deferred call outcome: {other:?}")
            }
        }
    }

    fn handle_request(
        &mut self,
        request: ServiceRequest,
        request_call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            ServiceRequest::DeferredSleep => request_call
                .defer(sleep(Duration::from_millis(1)))
                .reply_service_event(ServiceEvent::DeferredSleep),
            ServiceRequest::DeferredCall => request_call
                .defer(call(self.probe, ProbeMessage::Value, TIMEOUT))
                .reply_service_event(ServiceEvent::DeferredCall),
            ServiceRequest::FlowCall => request_call.capture(|request| {
                call(self.probe, ProbeMessage::Value, TIMEOUT)
                    .then_service_event_with_request(request, ServiceEvent::FlowCall)
            }),
        }
    }
}

#[test]
fn service_event_helpers_wrap_envelopes_and_preserve_request_authority() {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let probe = runtime
        .register_with_capacity(Probe, 8)
        .expect("register probe");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let service: SplitServiceHandle<ServiceEvent, ServiceRequest, u32> = runtime
        .register_split_service::<Service, ServiceEvent, ServiceRequest, Infallible>(
            Service {
                probe,
                observed: observed.clone(),
            },
            16,
        )
        .expect("register split service");

    runtime
        .send_event_and_observe(service.events, ServiceEvent::StartOrdinary)
        .expect("start ordinary continuations");
    runtime
        .send_event_and_observe(service.events, ServiceEvent::StartCancellation)
        .expect("start cancellation continuation");

    for (request, expected) in [
        (ServiceRequest::DeferredSleep, 11),
        (ServiceRequest::DeferredCall, 17),
        (ServiceRequest::FlowCall, 27),
    ] {
        assert_eq!(
            runtime
                .call_blocking_request(service.requests, request, TIMEOUT)
                .expect("host call admitted"),
            CallOutcome::Replied(expected),
        );
    }

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let mut values = observed.lock().unwrap().clone();
        values.sort_unstable();
        if values == ["call", "cancelable", "cancelled", "sleep", "typed_call"] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ordinary continuations did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    runtime.shutdown().expect("shutdown");
}

// ---------------------------------------------------------------------------
// Set / scope service-event helpers
//
// These prove the new wrappers land domain events on the split-service event
// lane (not the request lane), keep CallOutcome / CancelOutcome uncollapsed,
// and preserve CallGroup tokens through record_reply. If the helper wrapped
// the wrong envelope variant or dropped the outcome, handle_event would panic
// or observations would stay empty.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct SetObservations {
    group_outcomes: Vec<(u32, CallOutcome<u32>)>,
    join_outcomes: Vec<(u32, CallOutcome<u32>)>,
    select_outcomes: Vec<(u32, CallOutcome<u32>)>,
    scope_cancels: Vec<(&'static str, CancelOutcome)>,
}

#[derive(Debug)]
enum SetEvent {
    StartGroup,
    StartJoin,
    StartSelect,
    StartScopeHold,
    CancelScope,
    GroupReturned {
        key: u32,
        token: CallGroupToken,
        outcome: CallOutcome<u32>,
    },
    JoinReturned {
        key: u32,
        token: CallGroupToken,
        outcome: CallOutcome<u32>,
    },
    SelectReturned {
        key: u32,
        token: CallGroupToken,
        outcome: CallOutcome<u32>,
    },
    /// Hold finished before cancel — counted, not a panic, so a flaky
    /// race cannot fail the suite for the wrong reason.
    ScopeHoldSettled(CallOutcome<u32>),
    ScopeChildCancelled(RequestScopeId, &'static str, CancelOutcome),
}

struct SetService {
    probe: Address<ProbeMessage, u32>,
    group: CallGroup<u32, u32>,
    join: CallJoinSet<u32, u32>,
    select: CallSelectSet<u32, u32>,
    scope: RequestScope,
    observed: Arc<Mutex<SetObservations>>,
}

#[tina_runtime::isolate(event = SetEvent)]
impl SetService {
    fn handle_event(
        &mut self,
        event: SetEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            SetEvent::StartGroup => self
                .group
                .start_cancelable_service_event(
                    1u32,
                    call_cancelable(self.probe, ProbeMessage::Value, TIMEOUT),
                    |key, token, outcome| SetEvent::GroupReturned {
                        key,
                        token,
                        outcome,
                    },
                )
                .expect("group accepts one branch"),
            SetEvent::StartJoin => self
                .join
                .start_cancelable_service_event(
                    2u32,
                    call_cancelable(self.probe, ProbeMessage::Value, TIMEOUT),
                    |key, token, outcome| SetEvent::JoinReturned {
                        key,
                        token,
                        outcome,
                    },
                )
                .expect("join set accepts one branch"),
            SetEvent::StartSelect => self
                .select
                .start_cancelable_service_event(
                    3u32,
                    call_cancelable(self.probe, ProbeMessage::Value, TIMEOUT),
                    |key, token, outcome| SetEvent::SelectReturned {
                        key,
                        token,
                        outcome,
                    },
                )
                .expect("select set accepts one branch"),
            SetEvent::StartScopeHold => {
                // Admit the cancelable hold first; cancel runs on a later
                // event so the call slot exists (avoids NotAdmitted).
                let (effect, handle) = call_cancelable(self.probe, ProbeMessage::Hold, TIMEOUT)
                    .then_service_event(SetEvent::ScopeHoldSettled);
                self.scope
                    .register("held", handle)
                    .expect("scope has capacity");
                effect
            }
            SetEvent::CancelScope => {
                let (_report, cancel_effect) = self.scope.cancel_into_service_event_effect(
                    ScopeCancelCause::User,
                    SetEvent::ScopeChildCancelled,
                );
                cancel_effect
            }
            SetEvent::GroupReturned {
                key,
                token,
                outcome,
            } => {
                // Token must still be valid for record_reply; a wrong/missing
                // wrap would never reach this arm as a domain event.
                let _ = self
                    .group
                    .record_reply(key, token, outcome.clone(), |_| true)
                    .expect("group token from start_cancelable_service_event");
                self.observed
                    .lock()
                    .unwrap()
                    .group_outcomes
                    .push((key, outcome));
                noop()
            }
            SetEvent::JoinReturned {
                key,
                token,
                outcome,
            } => {
                let _ = self
                    .join
                    .record_reply(key, token, outcome.clone())
                    .expect("join token from start_cancelable_service_event");
                self.observed
                    .lock()
                    .unwrap()
                    .join_outcomes
                    .push((key, outcome));
                noop()
            }
            SetEvent::SelectReturned {
                key,
                token,
                outcome,
            } => {
                let _ = self
                    .select
                    .record_reply(key, token, outcome.clone())
                    .expect("select token from start_cancelable_service_event");
                self.observed
                    .lock()
                    .unwrap()
                    .select_outcomes
                    .push((key, outcome));
                noop()
            }
            SetEvent::ScopeHoldSettled(_outcome) => {
                // Cancelled waits do not normally re-enter here; ignore late
                // rejected deliveries so the cancel-ack assertion stays primary.
                noop()
            }
            SetEvent::ScopeChildCancelled(_scope, label, outcome) => {
                self.observed
                    .lock()
                    .unwrap()
                    .scope_cancels
                    .push((label, outcome));
                noop()
            }
        }
    }
}

#[test]
fn set_and_scope_service_event_helpers_wrap_envelopes_and_preserve_outcomes() {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let probe = runtime
        .register_with_capacity(Probe, 8)
        .expect("register probe");
    let observed = Arc::new(Mutex::new(SetObservations::default()));
    let events = runtime
        .register_event_service::<SetService, SetEvent, Infallible>(
            SetService {
                probe,
                group: CallGroup::with_capacity(1),
                join: CallJoinSet::with_capacity(1),
                select: CallSelectSet::with_capacity(1),
                scope: RequestScope::with_child_cap(RequestScopeId::alloc(), 2),
                observed: observed.clone(),
            },
            32,
        )
        .expect("register set service");

    for event in [
        SetEvent::StartGroup,
        SetEvent::StartJoin,
        SetEvent::StartSelect,
        SetEvent::StartScopeHold,
    ] {
        runtime
            .send_event_and_observe(events, event)
            .expect("start set/scope continuation");
    }
    // Hold is admitted before cancel so CancelOutcome is Cancelled, not
    // NotAdmitted (same-batch cancel-before-admit pitfall).
    runtime
        .send_event_and_observe(events, SetEvent::CancelScope)
        .expect("cancel scope");

    let observed_for_wait = observed.clone();
    let done = wait_for(POLL_BUDGET, || {
        let obs = observed_for_wait.lock().unwrap();
        !obs.group_outcomes.is_empty()
            && !obs.join_outcomes.is_empty()
            && !obs.select_outcomes.is_empty()
            && !obs.scope_cancels.is_empty()
    });
    assert!(
        done,
        "set/scope service-event helpers did not deliver: {:?}",
        observed.lock().unwrap()
    );

    let obs = observed.lock().unwrap().clone();
    assert_eq!(obs.group_outcomes.len(), 1);
    assert_eq!(obs.group_outcomes[0].0, 1);
    assert_eq!(obs.group_outcomes[0].1, CallOutcome::Replied(7));

    assert_eq!(obs.join_outcomes.len(), 1);
    assert_eq!(obs.join_outcomes[0].0, 2);
    assert_eq!(obs.join_outcomes[0].1, CallOutcome::Replied(7));

    assert_eq!(obs.select_outcomes.len(), 1);
    assert_eq!(obs.select_outcomes[0].0, 3);
    assert_eq!(obs.select_outcomes[0].1, CallOutcome::Replied(7));

    assert_eq!(obs.scope_cancels.len(), 1);
    assert_eq!(obs.scope_cancels[0].0, "held");
    assert_eq!(obs.scope_cancels[0].1, CancelOutcome::Cancelled);

    runtime.shutdown().expect("shutdown");
}
