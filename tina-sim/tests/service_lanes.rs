use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{CallOutcome, EventServiceHandle, RequestServiceHandle, call_request};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug)]
enum Event {
    Record(u32),
}

struct EventService {
    seen: Arc<AtomicU32>,
}

#[tina_runtime::isolate(event = Event)]
impl EventService {
    fn handle_event(
        &mut self,
        event: Event,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            Event::Record(value) => {
                self.seen.store(value, Ordering::Release);
                noop()
            }
        }
    }
}

#[derive(Debug)]
enum Request {
    Read,
}

struct RequestService {
    value: u32,
}

#[tina_runtime::isolate(request = Request, reply = u32)]
impl RequestService {
    fn handle_request(
        &mut self,
        request: Request,
        caller: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            Request::Read => caller.reply(self.value),
        }
    }
}

#[derive(Debug)]
enum ClientMessage {
    Start(RequestServiceHandle<Request, u32>),
    Returned(CallOutcome<u32>),
}

struct Client {
    observed: Arc<AtomicU32>,
}

#[tina_runtime::isolate(message = ClientMessage)]
impl Client {
    fn handle(
        &mut self,
        message: ClientMessage,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ClientMessage::Start(target) => {
                call_request(target, Request::Read, Duration::from_millis(10))
                    .then(ClientMessage::Returned)
            }
            ClientMessage::Returned(CallOutcome::Replied(value)) => {
                self.observed.store(value, Ordering::Release);
                stop()
            }
            ClientMessage::Returned(other) => panic!("request did not reply: {other:?}"),
        }
    }
}

#[test]
fn simulator_registers_and_routes_single_lane_services() {
    let mut simulator = Simulator::new(SingleShard, SimulatorConfig::default());
    let event_seen = Arc::new(AtomicU32::new(0));
    let request_seen = Arc::new(AtomicU32::new(0));

    let events = simulator.register_event_service(
        EventService {
            seen: Arc::clone(&event_seen),
        },
        1,
    );
    let _: EventServiceHandle<Event> = events;
    let requests = simulator.register_request_service(RequestService { value: 73 }, 4);
    let _: RequestServiceHandle<Request, u32> = requests;
    let client = simulator.register_with_mailbox_capacity::<Client, ClientMessage, Infallible>(
        Client {
            observed: Arc::clone(&request_seen),
        },
        4,
    );

    let sent: Result<(), tina_runtime::IngressSendError<Event>> =
        simulator.try_send_event(events, Event::Record(51));
    sent.expect("event accepted");
    assert!(matches!(
        simulator.try_send_event(events, Event::Record(52)),
        Err(tina_runtime::IngressSendError::Full(Event::Record(52)))
    ));
    simulator
        .try_send(client, ClientMessage::Start(requests))
        .expect("client start accepted");
    simulator.run_until_quiescent();

    assert_eq!(event_seen.load(Ordering::Acquire), 51);
    assert_eq!(request_seen.load(Ordering::Acquire), 73);
}
