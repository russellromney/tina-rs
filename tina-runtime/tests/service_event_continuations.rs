use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, DefaultThreadedMailboxFactory, SplitServiceHandle, ThreadedRuntime,
    call, call_cancelable, sleep,
};

const TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
enum ProbeMessage {
    Value,
}

struct Probe;

#[tina_runtime::isolate(message = ProbeMessage, reply = u32)]
impl Probe {
    fn handle(
        &mut self,
        _message: ProbeMessage,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, message: ProbeMessage, call: CallContext<'_, Self>) -> Effect<Self> {
        match message {
            ProbeMessage::Value => call.reply(7),
        }
    }
}

#[derive(Debug)]
enum ServiceEvent {
    StartOrdinary,
    Slept(Result<(), CallError>),
    Called(CallOutcome<u32>),
    CancelableCalled(CallOutcome<u32>),
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
                let called = call(self.probe, ProbeMessage::Value, TIMEOUT)
                    .then_service_event(ServiceEvent::Called);
                let (cancelable, handle) =
                    call_cancelable(self.probe, ProbeMessage::Value, TIMEOUT)
                        .then_service_event(ServiceEvent::CancelableCalled);
                drop(handle);
                Effect::Batch(vec![slept, called, cancelable])
            }
            ServiceEvent::Slept(result) => {
                result.expect("sleep succeeds");
                self.observed.lock().unwrap().push("sleep");
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
        if values == ["call", "cancelable", "sleep"] {
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
