use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tina::TrySendError;
use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, DefaultMailboxFactory, DefaultThreadedMailboxFactory,
    EventServiceHandle, LocalSystem, MultiShardRuntime, RequestServiceHandle, SplitServiceHandle,
    ThreadedMultiShardRuntime, ThreadedRuntimeError, sleep,
};

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
    Stop,
    Delayed(RequestContext<u32>, Result<(), CallError>),
}

#[derive(Debug)]
enum SplitRequest {
    Read,
    Hold,
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
            SplitEvent::Stop => stop(),
            SplitEvent::Delayed(request, Ok(())) | SplitEvent::Delayed(request, Err(_)) => {
                reply_to(request, 8)
            }
        }
    }

    fn handle_request(
        &mut self,
        request: SplitRequest,
        caller: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            SplitRequest::Read => caller.reply(7),
            SplitRequest::Hold => {
                caller
                    .defer(sleep(Duration::from_millis(200)))
                    .reply(|request, outcome| {
                        tina::ServiceMessage::Event(SplitEvent::Delayed(request, outcome))
                    })
            }
        }
    }
}

#[derive(Debug)]
enum RootMessage {
    Read,
}

struct RootService(u32);

#[tina_runtime::isolate(message = RootMessage, reply = u32, shard = ServiceShard)]
impl RootService {
    fn handle(
        &mut self,
        message: RootMessage,
        _ctx: &mut Context<'_, ServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            RootMessage::Read => reply(self.0),
        }
    }

    fn handle_call(&mut self, message: RootMessage, caller: CallContext<'_, Self>) -> Effect<Self> {
        match message {
            RootMessage::Read => caller.reply(self.0),
        }
    }
}

#[test]
fn explicit_multi_owner_preserves_event_pressure_and_shard_choice() {
    let mut runtime =
        MultiShardRuntime::new([ServiceShard(20), ServiceShard(10)], DefaultMailboxFactory);
    let seen = Arc::new(AtomicU32::new(0));

    let events = runtime.register_event_service_on(
        ShardId::new(20),
        EventService {
            seen: Arc::clone(&seen),
        },
        1,
    );
    let _: EventServiceHandle<Event> = events;
    assert_eq!(events.address().shard(), ShardId::new(20));

    runtime
        .try_send_event(events, Event::Record(41))
        .expect("first event accepted");
    assert_eq!(
        runtime.try_send_event(events, Event::Record(42)),
        Err(TrySendError::Full(Event::Record(42)))
    );
    runtime.step();
    assert_eq!(seen.load(Ordering::Acquire), 41);

    runtime
        .try_send_event(events, Event::Stop)
        .expect("stop accepted");
    runtime.step();
    assert_eq!(
        runtime.try_send_event(events, Event::Record(43)),
        Err(TrySendError::Closed(Event::Record(43)))
    );

    let requests = runtime.register_request_service_on(ShardId::new(10), RequestService(9), 2);
    let _: RequestServiceHandle<Request, u32> = requests;
    assert_eq!(requests.address().shard(), ShardId::new(10));

    let split = runtime.register_split_service_on(ShardId::new(10), SplitService, 2);
    let _: SplitServiceHandle<SplitEvent, SplitRequest, u32> = split;
    assert_eq!(split.events.address().shard(), ShardId::new(10));
}

#[test]
#[should_panic(expected = "unknown shard 99")]
fn explicit_multi_registration_panics_on_unknown_shard() {
    let mut runtime =
        MultiShardRuntime::new([ServiceShard(10), ServiceShard(20)], DefaultMailboxFactory);
    let _ = runtime.register_event_service_on(
        ShardId::new(99),
        EventService {
            seen: Arc::new(AtomicU32::new(0)),
        },
        4,
    );
}

#[test]
fn threaded_multi_owner_registers_all_service_shapes() {
    let runtime = ThreadedMultiShardRuntime::new(
        [ServiceShard(30), ServiceShard(40)],
        DefaultThreadedMailboxFactory,
    );
    let events = runtime
        .register_event_service_on(
            ShardId::new(40),
            EventService {
                seen: Arc::new(AtomicU32::new(0)),
            },
            4,
        )
        .expect("event service registered");
    let requests = runtime
        .register_request_service_on(ShardId::new(30), RequestService(11), 4)
        .expect("request service registered");
    let split = runtime
        .register_split_service_on(ShardId::new(30), SplitService, 4)
        .expect("split service registered");

    let _: EventServiceHandle<Event> = events;
    let _: RequestServiceHandle<Request, u32> = requests;
    let _: SplitServiceHandle<SplitEvent, SplitRequest, u32> = split;
    assert_eq!(events.address().shard(), ShardId::new(40));
    assert_eq!(requests.address().shard(), ShardId::new(30));
    runtime
        .try_send_event(events, Event::Record(50))
        .expect("event admitted to worker");
    assert_eq!(
        runtime
            .call_blocking_request(requests, Request::Read, Duration::from_secs(1))
            .expect("request call driven"),
        CallOutcome::Replied(11)
    );
    runtime
        .try_send_event(split.events, SplitEvent::Reset)
        .expect("split event admitted");
    assert_eq!(
        runtime
            .call_blocking_request(split.requests, SplitRequest::Read, Duration::from_secs(1))
            .expect("split request call driven"),
        CallOutcome::Replied(7)
    );

    runtime.shutdown().expect("threaded multi owner shuts down");
}

#[test]
fn threaded_multi_registration_returns_typed_unknown_shard() {
    let runtime = ThreadedMultiShardRuntime::new(
        [ServiceShard(30), ServiceShard(40)],
        DefaultThreadedMailboxFactory,
    );
    let result = runtime.register_event_service_on(
        ShardId::new(99),
        EventService {
            seen: Arc::new(AtomicU32::new(0)),
        },
        4,
    );

    assert!(matches!(
        result,
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    runtime.shutdown().expect("threaded multi owner shuts down");
}

#[test]
fn canonical_local_facades_delegate_service_registration_and_events() {
    let single = LocalSystem::single_shard(ServiceShard(50), DefaultThreadedMailboxFactory).build();
    let single_seen = Arc::new(AtomicU32::new(0));
    let single_events = single
        .register_event_service(
            EventService {
                seen: Arc::clone(&single_seen),
            },
            4,
        )
        .expect("single event service registered");
    let single_requests = single
        .register_request_service(RequestService(13), 4)
        .expect("single request service registered");
    let single_split = single
        .register_split_service(SplitService, 4)
        .expect("single split service registered");
    let single_root = single
        .register_root::<RootService, std::convert::Infallible>(RootService(19), 4)
        .expect("single root registered");
    let _: EventServiceHandle<Event> = single_events;
    let _: RequestServiceHandle<Request, u32> = single_requests;
    let _: SplitServiceHandle<SplitEvent, SplitRequest, u32> = single_split;
    single
        .try_send_event(single_events, Event::Record(60))
        .expect("single facade event admitted");
    assert_eq!(
        single
            .call_blocking_request(single_requests, Request::Read, Duration::from_secs(1),)
            .expect("single request call driven"),
        CallOutcome::Replied(13)
    );
    assert_eq!(
        single
            .call_blocking_request(
                single_split.requests,
                SplitRequest::Hold,
                Duration::from_millis(10),
            )
            .expect("single delayed request call driven"),
        CallOutcome::Timeout
    );
    assert_eq!(
        single
            .call_blocking(single_root, RootMessage::Read, Duration::from_secs(1))
            .expect("single root call driven"),
        CallOutcome::Replied(19)
    );
    single
        .try_send_event(single_split.events, SplitEvent::Stop)
        .expect("single split stop admitted");
    assert_eq!(
        single
            .call_blocking_request(
                single_split.requests,
                SplitRequest::Read,
                Duration::from_secs(1),
            )
            .expect("single closed request call driven"),
        CallOutcome::Closed
    );
    single
        .shutdown()
        .join()
        .expect("single local system shuts down");
    assert_eq!(single_seen.load(Ordering::Acquire), 60);

    let multi = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(ServiceShard(60))
        .shard(ServiceShard(70))
        .build();
    let multi_seen = Arc::new(AtomicU32::new(0));
    let multi_events = multi
        .register_event_service_on(
            ShardId::new(70),
            EventService {
                seen: Arc::clone(&multi_seen),
            },
            4,
        )
        .expect("multi event service registered");
    let multi_requests = multi
        .register_request_service_on(ShardId::new(60), RequestService(17), 4)
        .expect("multi request service registered");
    let multi_split = multi
        .register_split_service_on(ShardId::new(60), SplitService, 4)
        .expect("multi split service registered");
    let multi_root = multi
        .register_root_on::<RootService, std::convert::Infallible>(
            ShardId::new(70),
            RootService(23),
            4,
        )
        .expect("multi root registered");
    let _: EventServiceHandle<Event> = multi_events;
    let _: RequestServiceHandle<Request, u32> = multi_requests;
    let _: SplitServiceHandle<SplitEvent, SplitRequest, u32> = multi_split;
    assert_eq!(multi_events.address().shard(), ShardId::new(70));
    multi
        .try_send_event(multi_events, Event::Record(70))
        .expect("multi facade event admitted");
    assert_eq!(
        multi
            .call_blocking_request(multi_requests, Request::Read, Duration::from_secs(1),)
            .expect("multi request call driven"),
        CallOutcome::Replied(17)
    );
    assert_eq!(
        multi
            .call_blocking_request(
                multi_split.requests,
                SplitRequest::Read,
                Duration::from_secs(1),
            )
            .expect("multi split request call driven"),
        CallOutcome::Replied(7)
    );
    assert_eq!(multi_root.shard(), ShardId::new(70));
    assert_eq!(
        multi
            .call_blocking(multi_root, RootMessage::Read, Duration::from_secs(1))
            .expect("multi root call driven"),
        CallOutcome::Replied(23)
    );
    assert!(matches!(
        multi.register_event_service_on(
            ShardId::new(99),
            EventService {
                seen: Arc::new(AtomicU32::new(0)),
            },
            4,
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    multi
        .shutdown()
        .join()
        .expect("multi local system shuts down");
    assert_eq!(multi_seen.load(Ordering::Acquire), 70);
}

#[test]
fn local_multi_request_call_panics_on_an_address_from_an_unknown_shard() {
    let local = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(ServiceShard(80))
        .build();
    let foreign = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(ServiceShard(99))
        .build();
    let foreign_service = foreign
        .register_split_service_on(ShardId::new(99), SplitService, 4)
        .expect("foreign split service registered");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = local.call_blocking_request(
            foreign_service.requests,
            SplitRequest::Read,
            Duration::from_millis(50),
        );
    }));
    assert!(result.is_err(), "unknown shard must remain a loud misuse");

    local
        .shutdown()
        .join()
        .expect("local system shuts down after rejected call");
    foreign
        .shutdown()
        .join()
        .expect("foreign local system shuts down");
}
