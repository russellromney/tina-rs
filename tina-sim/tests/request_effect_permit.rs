//! Deterministic coverage for split-request permit execution.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tina::{RequestContext, SingleShard, noop};
use tina_runtime::{CallOutcome, RuntimeEventKind, call_request, sleep, stable_trace_hash};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug)]
enum Event {
    DeferredDone(RequestContext<Reply>, tina_runtime::SleepReply),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    Immediate,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reply {
    Immediate,
    Deferred,
}

struct Service {
    executions: Rc<RefCell<Vec<Request>>>,
}

#[tina_runtime::isolate(event = Event, request = Request, reply = Reply)]
impl Service {
    fn handle_event(
        &mut self,
        event: Event,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            Event::DeferredDone(request, Ok(())) => tina::reply_to(request, Reply::Deferred),
            Event::DeferredDone(request, Err(_)) => tina::reply_to(request, Reply::Deferred),
        }
    }

    fn handle_request(
        &mut self,
        request: Request,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        self.executions.borrow_mut().push(request);
        match request {
            Request::Immediate => call.reply(Reply::Immediate),
            Request::Deferred => {
                call.defer(sleep(Duration::from_millis(5)))
                    .reply(|request, result| {
                        tina::ServiceMessage::Event(Event::DeferredDone(request, result))
                    })
            }
        }
    }
}

#[derive(Debug)]
enum ClientMessage {
    Start(tina::ServiceRequestAddress<Event, Request, Reply>, Request),
    Returned(CallOutcome<Reply>),
}

struct Client {
    outcomes: Rc<RefCell<Vec<CallOutcome<Reply>>>>,
}

#[tina_runtime::isolate(message = ClientMessage)]
impl Client {
    fn handle(
        &mut self,
        message: ClientMessage,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ClientMessage::Start(service, request) => {
                call_request(service, request, Duration::from_millis(50))
                    .then(ClientMessage::Returned)
            }
            ClientMessage::Returned(outcome) => {
                self.outcomes.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Run {
    fingerprint: u64,
    executions: Vec<Request>,
    outcomes: Vec<CallOutcome<Reply>>,
    deferred_replies: usize,
    abandoned: usize,
}

fn run_once() -> Run {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let executions = Rc::new(RefCell::new(Vec::new()));
    let raw_service = sim.register(Service {
        executions: Rc::clone(&executions),
    });
    let service = tina::ServiceRequestAddress::from_call_address(raw_service.callable());
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(Client {
        outcomes: Rc::clone(&outcomes),
    });

    sim.try_send(client, ClientMessage::Start(service, Request::Immediate))
        .unwrap();
    sim.try_send(client, ClientMessage::Start(service, Request::Deferred))
        .unwrap();
    while sim.step() > 0 {}
    sim.advance_time(Duration::from_millis(5));
    while sim.step() > 0 {}

    Run {
        fingerprint: stable_trace_hash(sim.trace()),
        executions: executions.borrow().clone(),
        outcomes: outcomes.borrow().clone(),
        deferred_replies: sim
            .trace()
            .iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::DeferredReplySent { .. }))
            .count(),
        abandoned: sim
            .trace()
            .iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::CallReplyAbandoned { .. }))
            .count(),
    }
}

#[test]
fn request_permit_paths_execute_and_complete_once_under_replay() {
    let first = run_once();
    let second = run_once();

    assert_eq!(first, second, "same simulator schedule must replay exactly");
    assert_eq!(first.executions, [Request::Immediate, Request::Deferred]);
    assert_eq!(
        first.outcomes,
        [
            CallOutcome::Replied(Reply::Immediate),
            CallOutcome::Replied(Reply::Deferred),
        ]
    );
    assert_eq!(first.deferred_replies, 1);
    assert_eq!(first.abandoned, 0);
}
