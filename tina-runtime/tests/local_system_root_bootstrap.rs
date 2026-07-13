//! LocalSystem parity tests for atomic root register-and-bootstrap.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tina::CallRejectedReason;
use tina::Mailbox;
use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, MailboxFactory,
    ThreadedRegisterBootstrapError,
};

#[derive(Debug, Clone, Copy)]
struct TestShard(u32);

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug)]
struct DropProbe(Arc<AtomicU32>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
enum Msg {
    Bootstrap(DropProbe),
    Inspect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BootState(bool);

struct Service {
    booted: bool,
    service_drops: Arc<AtomicU32>,
    deliveries: Arc<AtomicU32>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.service_drops.fetch_add(1, Ordering::AcqRel);
    }
}

#[tina_runtime::isolate(message = Msg, reply = BootState, shard = TestShard)]
impl Service {
    fn handle(
        &mut self,
        message: Msg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            Msg::Bootstrap(_authority) => {
                self.booted = true;
                self.deliveries.fetch_add(1, Ordering::AcqRel);
                noop()
            }
            Msg::Inspect => noop(),
        }
    }

    fn handle_call(&mut self, message: Msg, call: CallContext<'_, Self>) -> Effect<Self> {
        match message {
            Msg::Inspect => call.reply(BootState(self.booted)),
            Msg::Bootstrap(_authority) => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}

fn fresh_service() -> (Service, Arc<AtomicU32>, Arc<AtomicU32>) {
    let service_drops = Arc::new(AtomicU32::new(0));
    let deliveries = Arc::new(AtomicU32::new(0));
    (
        Service {
            booted: false,
            service_drops: Arc::clone(&service_drops),
            deliveries: Arc::clone(&deliveries),
        },
        service_drops,
        deliveries,
    )
}

#[derive(Debug)]
enum SplitEvent {
    Bootstrap(DropProbe),
}

#[derive(Debug)]
enum SplitRequest {
    Inspect,
}

struct SplitService {
    booted: bool,
    deliveries: Arc<AtomicU32>,
}

#[tina_runtime::isolate(
    event = SplitEvent,
    request = SplitRequest,
    reply = BootState,
    shard = TestShard
)]
impl SplitService {
    fn handle_event(
        &mut self,
        event: SplitEvent,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            SplitEvent::Bootstrap(_authority) => {
                self.booted = true;
                self.deliveries.fetch_add(1, Ordering::AcqRel);
                noop()
            }
        }
    }

    fn handle_request(
        &mut self,
        request: SplitRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            SplitRequest::Inspect => call.reply(BootState(self.booted)),
        }
    }
}

#[test]
fn local_system_split_bootstrap_hides_envelope_and_is_first() {
    let app = LocalSystem::single_shard(TestShard(11), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("fallible startup");
    let deliveries = Arc::new(AtomicU32::new(0));
    let message_drops = Arc::new(AtomicU32::new(0));
    let service = app
        .register_split_service_with_bootstrap::<SplitService, _, _, Infallible>(
            SplitService {
                booted: false,
                deliveries: Arc::clone(&deliveries),
            },
            4,
            SplitEvent::Bootstrap(DropProbe(Arc::clone(&message_drops))),
        )
        .expect("typed split bootstrap");

    assert_eq!(
        app.call_blocking_request(
            service.requests,
            SplitRequest::Inspect,
            Duration::from_secs(2),
        )
        .expect("typed host call"),
        CallOutcome::Replied(BootState(true)),
    );
    assert_eq!(deliveries.load(Ordering::Acquire), 1);
    assert_eq!(message_drops.load(Ordering::Acquire), 1);
    app.shutdown().drain().join().expect("clean shutdown");
}

#[test]
fn local_system_split_bootstrap_full_returns_event_authority() {
    let app = LocalSystem::single_shard(TestShard(12), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("fallible startup");
    let deliveries = Arc::new(AtomicU32::new(0));
    let message_drops = Arc::new(AtomicU32::new(0));
    let error = app
        .register_split_service_with_bootstrap::<SplitService, _, _, Infallible>(
            SplitService {
                booted: false,
                deliveries: Arc::clone(&deliveries),
            },
            0,
            SplitEvent::Bootstrap(DropProbe(Arc::clone(&message_drops))),
        )
        .expect_err("zero-capacity split bootstrap");
    assert!(matches!(error, ThreadedRegisterBootstrapError::Full(_)));
    assert_eq!(deliveries.load(Ordering::Acquire), 0);
    assert_eq!(message_drops.load(Ordering::Acquire), 0);
    drop(error);
    assert_eq!(message_drops.load(Ordering::Acquire), 1);
    app.shutdown().drain().join().expect("clean shutdown");
}

#[derive(Debug, Clone, Copy)]
struct ClosedAtCapacityFactory(usize);

impl MailboxFactory for ClosedAtCapacityFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        let mailbox = DefaultThreadedMailboxFactory.create(capacity);
        if capacity == self.0 {
            mailbox.close();
        }
        mailbox
    }
}

#[derive(Debug)]
enum GateMsg {
    Hold,
}

struct Gate {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

#[tina_runtime::isolate(message = GateMsg, shard = TestShard)]
impl Gate {
    fn handle(
        &mut self,
        _message: GateMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        self.entered.store(true, Ordering::Release);
        while !self.release.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        noop()
    }
}

#[test]
fn local_system_bootstrap_is_first_and_supports_typed_host_calls() {
    let app = LocalSystem::single_shard(TestShard(1), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("fallible startup");
    let (service, service_drops, deliveries) = fresh_service();
    let message_drops = Arc::new(AtomicU32::new(0));
    let address = app
        .register_root_with_bootstrap::<Service, Infallible>(
            service,
            4,
            Msg::Bootstrap(DropProbe(Arc::clone(&message_drops))),
        )
        .expect("atomic root bootstrap");
    assert_eq!(address.system(), app.system_incarnation());

    let outcome = app
        .call_blocking(address, Msg::Inspect, Duration::from_secs(2))
        .expect("host call admitted");
    assert_eq!(outcome, CallOutcome::Replied(BootState(true)));
    assert_eq!(deliveries.load(Ordering::Acquire), 1);
    assert_eq!(message_drops.load(Ordering::Acquire), 1);

    app.shutdown().drain().join().expect("clean shutdown");
    assert_eq!(service_drops.load(Ordering::Acquire), 1);
}

#[test]
fn local_system_full_prefill_rolls_back_and_returns_authority_exactly_once() {
    let app = LocalSystem::single_shard(TestShard(2), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("fallible startup");
    let (service, service_drops, deliveries) = fresh_service();
    let message_drops = Arc::new(AtomicU32::new(0));
    let error = app
        .register_root_with_bootstrap::<Service, Infallible>(
            service,
            0,
            Msg::Bootstrap(DropProbe(Arc::clone(&message_drops))),
        )
        .expect_err("zero-capacity mailbox refuses bootstrap");
    assert!(matches!(error, ThreadedRegisterBootstrapError::Full(_)));
    assert_eq!(service_drops.load(Ordering::Acquire), 1);
    assert_eq!(deliveries.load(Ordering::Acquire), 0);
    assert_eq!(message_drops.load(Ordering::Acquire), 0);
    drop(error);
    assert_eq!(message_drops.load(Ordering::Acquire), 1);

    app.shutdown().drain().join().expect("clean shutdown");
}

#[test]
fn local_system_closed_prefill_rolls_back_and_returns_authority() {
    const REFUSED_CAPACITY: usize = 17;
    let app = LocalSystem::single_shard(TestShard(3), ClosedAtCapacityFactory(REFUSED_CAPACITY))
        .try_build()
        .expect("fallible startup");
    let (service, service_drops, deliveries) = fresh_service();
    let message_drops = Arc::new(AtomicU32::new(0));
    let error = app
        .register_root_with_bootstrap::<Service, Infallible>(
            service,
            REFUSED_CAPACITY,
            Msg::Bootstrap(DropProbe(Arc::clone(&message_drops))),
        )
        .expect_err("closed mailbox refuses bootstrap");
    assert!(matches!(error, ThreadedRegisterBootstrapError::Closed(_)));
    assert_eq!(service_drops.load(Ordering::Acquire), 1);
    assert_eq!(deliveries.load(Ordering::Acquire), 0);
    drop(error);
    assert_eq!(message_drops.load(Ordering::Acquire), 1);

    app.shutdown().drain().join().expect("clean shutdown");
}

#[test]
fn local_system_closed_command_returns_unadmitted_bootstrap() {
    let app = LocalSystem::single_shard(TestShard(4), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("fallible startup");
    app.shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("stop worker");
    let (service, service_drops, deliveries) = fresh_service();
    let message_drops = Arc::new(AtomicU32::new(0));
    let error = app
        .register_root_with_bootstrap::<Service, Infallible>(
            service,
            4,
            Msg::Bootstrap(DropProbe(Arc::clone(&message_drops))),
        )
        .expect_err("closed command lane");
    assert!(matches!(
        error,
        ThreadedRegisterBootstrapError::CommandClosed(_)
    ));
    assert_eq!(service_drops.load(Ordering::Acquire), 1);
    assert_eq!(deliveries.load(Ordering::Acquire), 0);
    drop(error);
    assert_eq!(message_drops.load(Ordering::Acquire), 1);
    let _ = app.shutdown().join();
}

#[test]
fn local_system_full_command_returns_authority_without_late_registration() {
    let app = Arc::new(
        LocalSystem::single_shard(TestShard(5), DefaultThreadedMailboxFactory)
            .ingress_capacity(1)
            .try_build()
            .expect("fallible startup"),
    );
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let gate = app
        .register_root::<Gate, Infallible>(
            Gate {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
            2,
        )
        .expect("register gate");
    app.try_send(gate, GateMsg::Hold).expect("occupy worker");
    while !entered.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    app.try_send(gate, GateMsg::Hold)
        .expect("fill command lane");

    let (service, service_drops, deliveries) = fresh_service();
    let message_drops = Arc::new(AtomicU32::new(0));
    let error = app
        .register_root_with_bootstrap::<Service, Infallible>(
            service,
            4,
            Msg::Bootstrap(DropProbe(Arc::clone(&message_drops))),
        )
        .expect_err("bounded command lane is full");
    assert!(matches!(
        error,
        ThreadedRegisterBootstrapError::CommandFull(_)
    ));
    assert_eq!(service_drops.load(Ordering::Acquire), 1);
    assert_eq!(deliveries.load(Ordering::Acquire), 0);
    drop(error);
    assert_eq!(message_drops.load(Ordering::Acquire), 1);

    release.store(true, Ordering::Release);
    Arc::try_unwrap(app)
        .unwrap_or_else(|_| panic!("sole local-system owner"))
        .shutdown()
        .drain()
        .join()
        .expect("clean shutdown");
    assert_eq!(
        deliveries.load(Ordering::Acquire),
        0,
        "no late registration"
    );
}

#[test]
fn local_multi_shard_bootstrap_routes_and_unknown_shard_returns_authority() {
    let app = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(TestShard(10))
        .shard(TestShard(20))
        .try_build()
        .expect("fallible startup");
    let (service, _, deliveries) = fresh_service();
    let successful_drops = Arc::new(AtomicU32::new(0));
    let address = app
        .register_root_with_bootstrap_on::<Service, Infallible>(
            ShardId::new(20),
            service,
            4,
            Msg::Bootstrap(DropProbe(Arc::clone(&successful_drops))),
        )
        .expect("bootstrap on owned shard");
    assert_eq!(address.system(), app.system_incarnation());
    assert_eq!(address.shard(), ShardId::new(20));
    let deadline = Instant::now() + Duration::from_secs(2);
    while deliveries.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(deliveries.load(Ordering::Acquire), 1);

    let (foreign_service, foreign_service_drops, foreign_deliveries) = fresh_service();
    let foreign_message_drops = Arc::new(AtomicU32::new(0));
    let error = app
        .register_root_with_bootstrap_on::<Service, Infallible>(
            ShardId::new(99),
            foreign_service,
            4,
            Msg::Bootstrap(DropProbe(Arc::clone(&foreign_message_drops))),
        )
        .expect_err("unknown shard");
    assert!(matches!(
        error,
        ThreadedRegisterBootstrapError::UnknownShard(shard, _) if shard == ShardId::new(99)
    ));
    assert_eq!(foreign_service_drops.load(Ordering::Acquire), 1);
    assert_eq!(foreign_deliveries.load(Ordering::Acquire), 0);
    drop(error);
    assert_eq!(foreign_message_drops.load(Ordering::Acquire), 1);

    app.shutdown().drain().join().expect("clean shutdown");
    assert_eq!(successful_drops.load(Ordering::Acquire), 1);
}

#[test]
fn local_multi_split_bootstrap_routes_and_returns_domain_event() {
    let app = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(TestShard(30))
        .try_build()
        .expect("fallible startup");
    let deliveries = Arc::new(AtomicU32::new(0));
    let successful_drops = Arc::new(AtomicU32::new(0));
    let service = app
        .register_split_service_with_bootstrap_on::<SplitService, _, _, Infallible>(
            ShardId::new(30),
            SplitService {
                booted: false,
                deliveries: Arc::clone(&deliveries),
            },
            4,
            SplitEvent::Bootstrap(DropProbe(Arc::clone(&successful_drops))),
        )
        .expect("typed split bootstrap on owned shard");
    assert_eq!(service.requests.address().shard(), ShardId::new(30));
    assert_eq!(
        app.call_blocking_request(
            service.requests,
            SplitRequest::Inspect,
            Duration::from_secs(2),
        )
        .expect("typed host call"),
        CallOutcome::Replied(BootState(true)),
    );

    let refused_drops = Arc::new(AtomicU32::new(0));
    let error = app
        .register_split_service_with_bootstrap_on::<SplitService, _, _, Infallible>(
            ShardId::new(99),
            SplitService {
                booted: false,
                deliveries: Arc::new(AtomicU32::new(0)),
            },
            4,
            SplitEvent::Bootstrap(DropProbe(Arc::clone(&refused_drops))),
        )
        .expect_err("unknown shard returns domain event");
    assert!(matches!(
        error,
        ThreadedRegisterBootstrapError::UnknownShard(shard, _) if shard == ShardId::new(99)
    ));
    assert_eq!(refused_drops.load(Ordering::Acquire), 0);
    drop(error);
    assert_eq!(refused_drops.load(Ordering::Acquire), 1);
    app.shutdown().drain().join().expect("clean shutdown");
    assert_eq!(successful_drops.load(Ordering::Acquire), 1);
}
