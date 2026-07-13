use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::IngressSendError;
use tina_runtime::sharded::{ShardPlacement, ShardRequestServiceTable};
use tina_runtime::{
    CallError, CallOutcome, DefaultMailboxFactory, DefaultThreadedMailboxFactory,
    EventServiceHandle, LocalSystem, LocalSystemConfig, MultiShardRuntime, RequestServiceHandle,
    ResultWaitError, SplitServiceHandle, ThreadedMultiShardRuntime, ThreadedRuntime,
    ThreadedRuntimeConfig, ThreadedRuntimeError, sleep,
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
    Reject,
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
            SplitRequest::Reject => caller.reject(tina::CallRejectedReason::UnsupportedMessage),
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

#[derive(Debug)]
struct DropMessage(Arc<AtomicU32>);

impl Drop for DropMessage {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

struct DropService;

#[tina_runtime::isolate(message = DropMessage, reply = (), shard = ServiceShard)]
impl DropService {
    fn handle(
        &mut self,
        _message: DropMessage,
        _ctx: &mut Context<'_, ServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _message: DropMessage, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(())
    }
}

#[derive(Debug)]
struct DropRequest(Arc<AtomicU32>);

impl Drop for DropRequest {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

struct DropRequestService;

#[tina_runtime::isolate(request = DropRequest, reply = (), shard = ServiceShard)]
impl DropRequestService {
    fn handle_request(
        &mut self,
        _request: DropRequest,
        caller: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        caller.reply(())
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
        Err(IngressSendError::Full(Event::Record(42)))
    );
    runtime.step();
    assert_eq!(seen.load(Ordering::Acquire), 41);

    runtime
        .try_send_event(events, Event::Stop)
        .expect("stop accepted");
    runtime.step();
    assert_eq!(
        runtime.try_send_event(events, Event::Record(43)),
        Err(IngressSendError::Closed(Event::Record(43)))
    );

    let requests = runtime.register_request_service_on(ShardId::new(10), RequestService(9), 2);
    let _: RequestServiceHandle<Request, u32> = requests;
    assert_eq!(requests.address().shard(), ShardId::new(10));

    let split = runtime.register_split_service_on(ShardId::new(10), SplitService, 2);
    let _: SplitServiceHandle<SplitEvent, SplitRequest, u32> = split;
    assert_eq!(split.events.address().shard(), ShardId::new(10));
}

#[test]
fn request_service_table_registers_and_routes_explicit_multi_owner_services() {
    let placement = ShardPlacement::new(
        "explicit requests",
        vec![ShardId::new(20), ShardId::new(10)],
    )
    .unwrap();
    let mut runtime =
        MultiShardRuntime::new([ServiceShard(20), ServiceShard(10)], DefaultMailboxFactory);
    let table = ShardRequestServiceTable::from_placement(placement.clone(), |shard| {
        runtime.register_request_service_on(shard, RequestService(shard.get()), 2)
    })
    .unwrap();

    let _: RequestServiceHandle<Request, u32> = table.address_for(ShardId::new(10)).unwrap();
    assert_eq!(table.addresses().len(), 2);
    assert_eq!(
        table
            .address_for_str("owned key")
            .address()
            .address()
            .shard(),
        placement.owner_for_str("owned key")
    );
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
fn request_service_table_registers_and_calls_threaded_multi_owner_services() {
    let placement = ShardPlacement::new(
        "threaded requests",
        vec![ShardId::new(30), ShardId::new(40)],
    )
    .unwrap();
    let runtime = ThreadedMultiShardRuntime::new(
        [ServiceShard(30), ServiceShard(40)],
        DefaultThreadedMailboxFactory,
    );
    let table = ShardRequestServiceTable::try_from_placement(placement, |shard| {
        runtime.register_request_service_on(shard, RequestService(shard.get()), 4)
    })
    .unwrap();

    for shard in [ShardId::new(30), ShardId::new(40)] {
        assert_eq!(
            runtime
                .call_blocking_request(
                    table.address_for(shard).unwrap(),
                    Request::Read,
                    Duration::from_secs(1),
                )
                .expect("request call routed"),
            CallOutcome::Replied(shard.get())
        );
    }
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
fn threaded_single_host_operations_reject_routing_before_admission() {
    let local_system = tina::SystemIncarnation::new(0x5050);
    let foreign_system = tina::SystemIncarnation::new(0x5051);
    let config_for = |system_incarnation| ThreadedRuntimeConfig {
        system_incarnation: Some(system_incarnation),
        ..ThreadedRuntimeConfig::default()
    };
    let local = ThreadedRuntime::with_config(
        ServiceShard(50),
        DefaultThreadedMailboxFactory,
        config_for(local_system),
    );
    let unowned = ThreadedRuntime::with_config(
        ServiceShard(99),
        DefaultThreadedMailboxFactory,
        config_for(local_system),
    );
    let foreign = ThreadedRuntime::with_config(
        ServiceShard(99),
        DefaultThreadedMailboxFactory,
        config_for(foreign_system),
    );
    let local_drop = local
        .register_with_capacity::<DropService, std::convert::Infallible>(DropService, 4)
        .expect("local drop service registered");
    let unowned_drop = unowned
        .register_with_capacity::<DropService, std::convert::Infallible>(DropService, 4)
        .expect("same-system unowned drop service registered");
    let unowned_request = unowned
        .register_request_service(DropRequestService, 4)
        .expect("same-system unowned request service registered");
    let foreign_drop = foreign
        .register_with_capacity::<DropService, std::convert::Infallible>(DropService, 4)
        .expect("foreign drop service registered");
    let foreign_request = foreign
        .register_request_service(DropRequestService, 4)
        .expect("foreign request service registered");
    let drops = Arc::new(AtomicU32::new(0));

    assert!(matches!(
        local.call_blocking(
            unowned_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert!(matches!(
        local.call_blocking_with_host_timeout(
            unowned_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_secs(1),
            Duration::ZERO,
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert!(matches!(
        local.call_blocking_typed(
            unowned_drop.callable(),
            DropMessage(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert!(matches!(
        local.call_blocking_request(
            unowned_request,
            DropRequest(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));

    // Exceed the registry's 1024-slot bound. Routing failures must never reach
    // the worker command queue or consume an observation slot.
    for _ in 0..=1024 {
        assert_eq!(
            local.observe_result::<u32, _, _>(unowned_drop).err(),
            Some(ResultWaitError::UnknownShard(ShardId::new(99)))
        );
    }
    let local_waiter = local
        .observe_result::<u32, _, _>(local_drop)
        .expect("unowned-shard rejections did not exhaust observation capacity");
    drop(local_waiter);

    assert!(matches!(
        local.call_blocking(
            foreign_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == local_system && actual == foreign_system
    ));
    assert!(matches!(
        local.call_blocking_with_host_timeout(
            foreign_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_secs(1),
            Duration::ZERO,
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == local_system && actual == foreign_system
    ));
    assert!(matches!(
        local.call_blocking_typed(
            foreign_drop.callable(),
            DropMessage(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == local_system && actual == foreign_system
    ));
    assert!(matches!(
        local.call_blocking_request(
            foreign_request,
            DropRequest(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == local_system && actual == foreign_system
    ));
    assert!(matches!(
        local.observe_result::<u32, _, _>(foreign_drop),
        Err(ResultWaitError::ForeignSystem { expected, actual })
            if expected == local_system && actual == foreign_system
    ));
    assert_eq!(drops.load(Ordering::Acquire), 8);

    let report = local
        .shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("local worker stops");
    assert!(report.error().is_none());
    assert!(matches!(
        local.call_blocking(
            unowned_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::ZERO,
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert!(matches!(
        local.call_blocking(
            foreign_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::ZERO,
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == local_system && actual == foreign_system
    ));
    assert_eq!(
        local.observe_result::<u32, _, _>(unowned_drop).err(),
        Some(ResultWaitError::UnknownShard(ShardId::new(99)))
    );
    assert!(matches!(
        local.observe_result::<u32, _, _>(foreign_drop),
        Err(ResultWaitError::ForeignSystem { expected, actual })
            if expected == local_system && actual == foreign_system
    ));
    assert_eq!(drops.load(Ordering::Acquire), 10);

    drop(local);
    unowned.shutdown().expect("unowned runtime shuts down");
    foreign.shutdown().expect("foreign runtime shuts down");
}

#[test]
fn threaded_multi_host_operations_reject_same_system_unowned_shard_before_admission() {
    let system = tina::SystemIncarnation::new(0x5100);
    let config = ThreadedRuntimeConfig {
        system_incarnation: Some(system),
        ..ThreadedRuntimeConfig::default()
    };
    let local = ThreadedMultiShardRuntime::with_config(
        [ServiceShard(30), ServiceShard(40)],
        DefaultThreadedMailboxFactory,
        config,
    );
    let other = ThreadedMultiShardRuntime::with_config(
        [ServiceShard(99)],
        DefaultThreadedMailboxFactory,
        config,
    );
    let foreign_system = tina::SystemIncarnation::new(0x5101);
    let foreign = ThreadedMultiShardRuntime::with_config(
        [ServiceShard(99)],
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            system_incarnation: Some(foreign_system),
            ..ThreadedRuntimeConfig::default()
        },
    );
    let local_drop = local
        .register_with_capacity_on::<DropService, std::convert::Infallible>(
            ShardId::new(30),
            DropService,
            4,
        )
        .expect("local drop service registered");
    let other_drop = other
        .register_with_capacity_on::<DropService, std::convert::Infallible>(
            ShardId::new(99),
            DropService,
            4,
        )
        .expect("other drop service registered");
    let other_request = other
        .register_request_service_on(ShardId::new(99), DropRequestService, 4)
        .expect("other request service registered");
    let foreign_drop = foreign
        .register_with_capacity_on::<DropService, std::convert::Infallible>(
            ShardId::new(99),
            DropService,
            4,
        )
        .expect("foreign drop service registered");
    let foreign_request = foreign
        .register_request_service_on(ShardId::new(99), DropRequestService, 4)
        .expect("foreign request service registered");
    let drops = Arc::new(AtomicU32::new(0));

    assert!(matches!(
        local.call_blocking(
            other_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert!(matches!(
        local.call_blocking_with_host_timeout(
            other_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_secs(1),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert!(matches!(
        local.call_blocking_request(
            other_request,
            DropRequest(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert_eq!(drops.load(Ordering::Acquire), 3);
    for _ in 0..=1024 {
        assert_eq!(
            local.observe_result::<u32, _, _>(other_drop).err(),
            Some(ResultWaitError::UnknownShard(ShardId::new(99)))
        );
    }
    let local_waiter = local
        .observe_result::<u32, _, _>(local_drop)
        .expect("unknown-shard rejection did not claim observation capacity");
    drop(local_waiter);

    assert!(matches!(
        local.call_blocking(
            foreign_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert!(matches!(
        local.call_blocking_with_host_timeout(
            foreign_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_secs(1),
            Duration::ZERO,
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert!(matches!(
        local.call_blocking_request(
            foreign_request,
            DropRequest(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert!(matches!(
        local.observe_result::<u32, _, _>(foreign_drop),
        Err(ResultWaitError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert_eq!(drops.load(Ordering::Acquire), 6);

    let report = local
        .shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("local workers stop");
    assert!(report.error().is_none());
    assert!(matches!(
        local.call_blocking(
            other_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::ZERO,
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert!(matches!(
        local.call_blocking(
            foreign_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::ZERO,
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert_eq!(
        local.observe_result::<u32, _, _>(other_drop).err(),
        Some(ResultWaitError::UnknownShard(ShardId::new(99)))
    );
    assert!(matches!(
        local.observe_result::<u32, _, _>(foreign_drop),
        Err(ResultWaitError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert_eq!(drops.load(Ordering::Acquire), 8);

    drop(local);
    other.shutdown().expect("other runtime shuts down");
    foreign.shutdown().expect("foreign runtime shuts down");
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
            .call_blocking_request(
                single_split.requests,
                SplitRequest::Reject,
                Duration::from_secs(1),
            )
            .expect("single rejected request call driven"),
        CallOutcome::Rejected(tina::CallRejectedReason::UnsupportedMessage)
    );
    assert_eq!(
        single.call_blocking_with_host_timeout(
            single_split.requests.address().address(),
            tina::ServiceMessage::Request(SplitRequest::Hold),
            Duration::from_secs(1),
            Duration::from_millis(1),
        ),
        Err(ThreadedRuntimeError::HostWaitTimeout)
    );
    assert_eq!(
        single
            .call_blocking_with_host_timeout(
                single_root,
                RootMessage::Read,
                Duration::from_secs(1),
                Duration::from_secs(2),
            )
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
    assert_eq!(
        multi.call_blocking_with_host_timeout(
            multi_split.requests.address().address(),
            tina::ServiceMessage::Request(SplitRequest::Hold),
            Duration::from_secs(1),
            Duration::from_millis(1),
        ),
        Err(ThreadedRuntimeError::HostWaitTimeout)
    );
    assert_eq!(multi_root.shard(), ShardId::new(70));
    assert_eq!(
        multi
            .call_blocking_with_host_timeout(
                multi_root,
                RootMessage::Read,
                Duration::from_secs(1),
                Duration::from_secs(2),
            )
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
fn local_single_host_operations_reject_same_system_unowned_shard_before_admission() {
    let system = tina::SystemIncarnation::new(0x5150);
    let config = LocalSystemConfig {
        system_incarnation: Some(system),
        ..LocalSystemConfig::default()
    };
    let local = LocalSystem::single_shard(ServiceShard(50), DefaultThreadedMailboxFactory)
        .config(config)
        .build();
    let other = LocalSystem::single_shard(ServiceShard(99), DefaultThreadedMailboxFactory)
        .config(config)
        .build();
    let foreign_system = tina::SystemIncarnation::new(0x5151);
    let foreign = LocalSystem::single_shard(ServiceShard(99), DefaultThreadedMailboxFactory)
        .config(LocalSystemConfig {
            system_incarnation: Some(foreign_system),
            ..LocalSystemConfig::default()
        })
        .build();
    let local_drop = local
        .register_root::<DropService, std::convert::Infallible>(DropService, 4)
        .expect("local drop service registered");
    let other_drop = other
        .register_root::<DropService, std::convert::Infallible>(DropService, 4)
        .expect("other drop service registered");
    let other_request = other
        .register_request_service(DropRequestService, 4)
        .expect("other request service registered");
    let foreign_drop = foreign
        .register_root::<DropService, std::convert::Infallible>(DropService, 4)
        .expect("foreign drop service registered");
    let foreign_request = foreign
        .register_request_service(DropRequestService, 4)
        .expect("foreign request service registered");
    let drops = Arc::new(AtomicU32::new(0));

    assert!(matches!(
        local.call_blocking(
            other_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert!(matches!(
        local.call_blocking_with_host_timeout(
            other_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_secs(1),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert!(matches!(
        local.call_blocking_request(
            other_request,
            DropRequest(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert_eq!(drops.load(Ordering::Acquire), 3);
    for _ in 0..=1024 {
        assert_eq!(
            local.observe_result::<u32, _, _>(other_drop).err(),
            Some(ResultWaitError::UnknownShard(ShardId::new(99)))
        );
    }
    let local_waiter = local
        .observe_result::<u32, _, _>(local_drop)
        .expect("unknown-shard rejection did not claim observation capacity");
    drop(local_waiter);

    assert!(matches!(
        local.call_blocking(
            foreign_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert!(matches!(
        local.call_blocking_with_host_timeout(
            foreign_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_secs(1),
            Duration::ZERO,
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert!(matches!(
        local.call_blocking_request(
            foreign_request,
            DropRequest(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert!(matches!(
        local.observe_result::<u32, _, _>(foreign_drop),
        Err(ResultWaitError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert_eq!(drops.load(Ordering::Acquire), 6);

    local.shutdown().join().expect("local system shuts down");
    other.shutdown().join().expect("other system shuts down");
    foreign
        .shutdown()
        .join()
        .expect("foreign system shuts down");
}

#[test]
fn request_service_table_registers_and_calls_local_multi_services() {
    let placement =
        ShardPlacement::new("local requests", vec![ShardId::new(60), ShardId::new(70)]).unwrap();
    let multi = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(ServiceShard(60))
        .shard(ServiceShard(70))
        .build();
    let table = ShardRequestServiceTable::try_from_placement(placement, |shard| {
        multi.register_request_service_on(shard, RequestService(shard.get()), 4)
    })
    .unwrap();

    for shard in [ShardId::new(60), ShardId::new(70)] {
        assert_eq!(
            multi
                .call_blocking_request(
                    table.address_for(shard).unwrap(),
                    Request::Read,
                    Duration::from_secs(1),
                )
                .expect("request call routed"),
            CallOutcome::Replied(shard.get())
        );
    }
    multi.shutdown().join().expect("local multi shuts down");
}

#[test]
fn local_multi_request_call_rejects_foreign_system_before_unknown_shard() {
    let local = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(ServiceShard(80))
        .build();
    let foreign = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(ServiceShard(99))
        .build();
    let foreign_service = foreign
        .register_split_service_on(ShardId::new(99), SplitService, 4)
        .expect("foreign split service registered");

    assert!(matches!(
        local.call_blocking_request(
            foreign_service.requests,
            SplitRequest::Read,
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == local.system_incarnation()
                && actual == foreign.system_incarnation()
    ));

    local
        .shutdown()
        .join()
        .expect("local system shuts down after rejected call");
    foreign
        .shutdown()
        .join()
        .expect("foreign local system shuts down");
}

#[test]
fn local_multi_host_operations_reject_same_system_unowned_shard_before_admission() {
    let system = tina::SystemIncarnation::new(0x5200);
    let config = LocalSystemConfig {
        system_incarnation: Some(system),
        ..LocalSystemConfig::default()
    };
    let local = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(ServiceShard(80))
        .config(config)
        .build();
    let other = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(ServiceShard(99))
        .config(config)
        .build();
    let foreign_system = tina::SystemIncarnation::new(0x5201);
    let foreign = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(ServiceShard(99))
        .config(LocalSystemConfig {
            system_incarnation: Some(foreign_system),
            ..LocalSystemConfig::default()
        })
        .build();
    let local_drop = local
        .register_root_on::<DropService, std::convert::Infallible>(ShardId::new(80), DropService, 4)
        .expect("local drop service registered");
    let other_drop = other
        .register_root_on::<DropService, std::convert::Infallible>(ShardId::new(99), DropService, 4)
        .expect("other drop service registered");
    let other_request = other
        .register_request_service_on(ShardId::new(99), DropRequestService, 4)
        .expect("other request service registered");
    let foreign_drop = foreign
        .register_root_on::<DropService, std::convert::Infallible>(ShardId::new(99), DropService, 4)
        .expect("foreign drop service registered");
    let foreign_request = foreign
        .register_request_service_on(ShardId::new(99), DropRequestService, 4)
        .expect("foreign request service registered");
    let drops = Arc::new(AtomicU32::new(0));

    assert!(matches!(
        local.call_blocking(
            other_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert!(matches!(
        local.call_blocking_with_host_timeout(
            other_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_secs(1),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert!(matches!(
        local.call_blocking_request(
            other_request,
            DropRequest(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert_eq!(drops.load(Ordering::Acquire), 3);
    for _ in 0..=1024 {
        assert_eq!(
            local.observe_result::<u32, _, _>(other_drop).err(),
            Some(ResultWaitError::UnknownShard(ShardId::new(99)))
        );
    }
    let local_waiter = local
        .observe_result::<u32, _, _>(local_drop)
        .expect("unknown-shard rejection did not claim observation capacity");
    drop(local_waiter);

    assert!(matches!(
        local.call_blocking(
            foreign_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert!(matches!(
        local.call_blocking_with_host_timeout(
            foreign_drop,
            DropMessage(Arc::clone(&drops)),
            Duration::from_secs(1),
            Duration::ZERO,
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert!(matches!(
        local.call_blocking_request(
            foreign_request,
            DropRequest(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert!(matches!(
        local.observe_result::<u32, _, _>(foreign_drop),
        Err(ResultWaitError::ForeignSystem { expected, actual })
            if expected == system && actual == foreign_system
    ));
    assert_eq!(drops.load(Ordering::Acquire), 6);

    local.shutdown().join().expect("local system shuts down");
    other.shutdown().join().expect("other system shuts down");
    foreign
        .shutdown()
        .join()
        .expect("foreign system shuts down");
}
