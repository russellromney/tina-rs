use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::sharded::{ShardPlacement, ShardRequestServiceTable};
use tina_runtime::{
    CallOutcome, EventServiceHandle, RequestServiceHandle, SplitServiceHandle, call_request,
};
use tina_sim::{MultiShardSimulator, SimulatorConfig};

#[derive(Debug, Clone, Copy)]
struct ServiceShard(u32);

impl Shard for ServiceShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Record(u32),
    Stop,
}

struct EventService {
    seen: Arc<AtomicU32>,
}

#[tina_runtime::isolate(event = Event, shard = ServiceShard)]
impl EventService {
    fn handle_event(
        &mut self,
        event: Event,
        _ctx: &mut Context<'_, ServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            Event::Record(value) => {
                self.seen.store(value, Ordering::Release);
                noop()
            }
            Event::Stop => stop(),
        }
    }
}

#[derive(Debug)]
enum Request {
    Read,
}

struct RequestService(u32);

#[tina_runtime::isolate(request = Request, reply = u32, shard = ServiceShard)]
impl RequestService {
    fn handle_request(
        &mut self,
        request: Request,
        caller: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            Request::Read => caller.reply(self.0),
        }
    }
}

#[derive(Debug)]
enum SplitEvent {
    Reset,
}

#[derive(Debug)]
enum SplitRequest {
    Read,
}

struct SplitService;

#[tina_runtime::isolate(
    event = SplitEvent,
    request = SplitRequest,
    reply = u32,
    shard = ServiceShard
)]
impl SplitService {
    fn handle_event(
        &mut self,
        event: SplitEvent,
        _ctx: &mut Context<'_, ServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            SplitEvent::Reset => noop(),
        }
    }

    fn handle_request(
        &mut self,
        request: SplitRequest,
        caller: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            SplitRequest::Read => caller.reply(23),
        }
    }
}

#[derive(Debug)]
enum ClientMessage {
    Start(RequestServiceHandle<Request, u32>),
    RequestReturned(CallOutcome<u32>),
    SplitReturned(CallOutcome<u32>),
}

struct Client {
    split: SplitServiceHandle<SplitEvent, SplitRequest, u32>,
    observed: Arc<AtomicU32>,
}

#[tina_runtime::isolate(
    message = ClientMessage,
    send = tina::ServiceOutbound<SplitEvent, SplitRequest>,
    shard = ServiceShard
)]
impl Client {
    fn handle(
        &mut self,
        message: ClientMessage,
        _ctx: &mut Context<'_, ServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ClientMessage::Start(requests) => {
                call_request(requests, Request::Read, Duration::from_millis(10))
                    .then(ClientMessage::RequestReturned)
            }
            ClientMessage::RequestReturned(CallOutcome::Replied(value)) => {
                self.observed.store(value, Ordering::Release);
                batch([
                    tina::send_event(self.split.events, SplitEvent::Reset),
                    call_request(
                        self.split.requests,
                        SplitRequest::Read,
                        Duration::from_millis(10),
                    )
                    .then(ClientMessage::SplitReturned),
                ])
            }
            ClientMessage::SplitReturned(CallOutcome::Replied(value)) => {
                assert_eq!(self.observed.load(Ordering::Acquire), 37);
                self.observed.store(value, Ordering::Release);
                stop()
            }
            ClientMessage::RequestReturned(other) | ClientMessage::SplitReturned(other) => {
                panic!("service request did not reply: {other:?}")
            }
        }
    }
}

#[test]
fn simulator_multi_owner_preserves_service_capabilities_and_domain_errors() {
    let mut simulator = MultiShardSimulator::new(
        [ServiceShard(2), ServiceShard(1)],
        SimulatorConfig::default(),
    );
    let seen = Arc::new(AtomicU32::new(0));

    let events = simulator.register_event_service_on(
        ShardId::new(2),
        EventService {
            seen: Arc::clone(&seen),
        },
        1,
    );
    let _: EventServiceHandle<Event> = events;
    assert_eq!(events.address().shard(), ShardId::new(2));

    simulator
        .try_send_event(events, Event::Record(31))
        .expect("first event accepted");
    assert_eq!(
        simulator.try_send_event(events, Event::Record(32)),
        Err(tina_runtime::IngressSendError::Full(Event::Record(32)))
    );
    simulator.step();
    assert_eq!(seen.load(Ordering::Acquire), 31);

    simulator
        .try_send_event(events, Event::Stop)
        .expect("stop accepted");
    simulator.step();
    assert_eq!(
        simulator.try_send_event(events, Event::Record(33)),
        Err(tina_runtime::IngressSendError::Closed(Event::Record(33)))
    );

    let requests = simulator.register_request_service_on(ShardId::new(1), RequestService(37), 2);
    let _: RequestServiceHandle<Request, u32> = requests;
    assert_eq!(requests.address().shard(), ShardId::new(1));

    let split = simulator.register_split_service_on(ShardId::new(1), SplitService, 2);
    let _: SplitServiceHandle<SplitEvent, SplitRequest, u32> = split;
    assert_eq!(split.events.address().shard(), ShardId::new(1));
    simulator
        .try_send_event(split.events, SplitEvent::Reset)
        .expect("split event accepted");

    let request_seen = Arc::new(AtomicU32::new(0));
    let client = simulator.register_with_capacity_on::<Client, ClientMessage, _>(
        ShardId::new(2),
        Client {
            split,
            observed: Arc::clone(&request_seen),
        },
        4,
    );
    simulator
        .try_send(client, ClientMessage::Start(requests))
        .expect("client start accepted");
    simulator.run_until_quiescent();
    assert_eq!(request_seen.load(Ordering::Acquire), 23);
}

#[test]
fn request_service_table_registers_and_routes_simulated_multi_owner_services() {
    let placement =
        ShardPlacement::new("sim requests", vec![ShardId::new(2), ShardId::new(1)]).unwrap();
    let mut simulator = MultiShardSimulator::new(
        [ServiceShard(2), ServiceShard(1)],
        SimulatorConfig::default(),
    );
    let table = ShardRequestServiceTable::from_placement(placement.clone(), |shard| {
        simulator.register_request_service_on(shard, RequestService(shard.get()), 2)
    })
    .unwrap();

    let _: RequestServiceHandle<Request, u32> = table.address_for(ShardId::new(1)).unwrap();
    assert_eq!(table.addresses().len(), 2);
    assert_eq!(
        table
            .address_for_bytes(b"owned key")
            .address()
            .address()
            .shard(),
        placement.owner_for_bytes(b"owned key")
    );
}

#[test]
#[should_panic(expected = "unknown shard 99")]
fn simulator_multi_registration_panics_on_unknown_shard() {
    let mut simulator = MultiShardSimulator::new(
        [ServiceShard(1), ServiceShard(2)],
        SimulatorConfig::default(),
    );
    let _ = simulator.register_event_service_on(
        ShardId::new(99),
        EventService {
            seen: Arc::new(AtomicU32::new(0)),
        },
        4,
    );
}
