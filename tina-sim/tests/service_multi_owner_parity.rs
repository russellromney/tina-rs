use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tina::TrySendError;
use tina::prelude::*;
use tina_runtime::{EventServiceHandle, RequestServiceHandle, SplitServiceHandle};
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
        Err(TrySendError::Full(Event::Record(32)))
    );
    simulator.step();
    assert_eq!(seen.load(Ordering::Acquire), 31);

    simulator
        .try_send_event(events, Event::Stop)
        .expect("stop accepted");
    simulator.step();
    assert_eq!(
        simulator.try_send_event(events, Event::Record(33)),
        Err(TrySendError::Closed(Event::Record(33)))
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
    let _ = (Request::Read, SplitRequest::Read);
}
