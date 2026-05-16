//! Register-and-bootstrap helpers prefill the mailbox with the bootstrap
//! message before inserting the isolate entry. Tests pin: first delivered
//! message is the bootstrap, no host `try_send(Bootstrap)` is needed, sending
//! immediately after the helper returns can see `Full` when capacity is 1,
//! and a custom mailbox that refuses the prefill leaves no registered isolate.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tina::TrySendError;
use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultMailboxFactory, DefaultThreadedMailboxFactory, MultiShardRuntime,
    RegisterBootstrapError, Runtime, ThreadedMultiShardRuntime, ThreadedRegisterBootstrapError,
    ThreadedRuntime,
};

#[derive(Debug, Clone, Copy)]
struct TestShard(u32);

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Msg {
    Bootstrap,
    Tick,
    Stop,
}

#[derive(Debug)]
struct Service {
    delivered: Arc<AtomicU32>,
    first_was_bootstrap: Arc<std::sync::atomic::AtomicBool>,
}

#[tina_runtime::isolate(message = Msg)]
impl Service {
    fn handle(
        &mut self,
        msg: Msg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        let n = self.delivered.fetch_add(1, Ordering::AcqRel);
        if n == 0 {
            self.first_was_bootstrap
                .store(matches!(msg, Msg::Bootstrap), Ordering::Release);
        }
        match msg {
            Msg::Bootstrap | Msg::Tick => noop(),
            Msg::Stop => stop(),
        }
    }
}

fn fresh_service() -> (Service, Arc<AtomicU32>, Arc<std::sync::atomic::AtomicBool>) {
    let delivered = Arc::new(AtomicU32::new(0));
    let first_was_bootstrap = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let service = Service {
        delivered: Arc::clone(&delivered),
        first_was_bootstrap: Arc::clone(&first_was_bootstrap),
    };
    (service, delivered, first_was_bootstrap)
}

#[test]
fn register_with_capacity_and_bootstrap_makes_first_message_bootstrap() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let (svc, delivered, first_was_bootstrap) = fresh_service();
    let addr = runtime
        .register_with_capacity_and_bootstrap::<_, Infallible>(svc, 4, Msg::Bootstrap)
        .expect("bootstrap admitted");

    // Step the runtime — no host try_send was needed.
    while runtime.step() > 0 {}
    assert_eq!(delivered.load(Ordering::Acquire), 1);
    assert!(first_was_bootstrap.load(Ordering::Acquire));

    runtime.try_send(addr, Msg::Tick).unwrap();
    while runtime.step() > 0 {}
    assert_eq!(delivered.load(Ordering::Acquire), 2);
}

#[test]
fn capacity_one_bootstrap_can_see_full_until_consumed() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let (svc, delivered, _) = fresh_service();
    let addr = runtime
        .register_with_capacity_and_bootstrap::<_, Infallible>(svc, 1, Msg::Bootstrap)
        .expect("bootstrap admitted");

    // Capacity is 1 and Bootstrap is parked in the mailbox; an immediate Tick
    // must see Full. This is honest pressure, documented as such.
    match runtime.try_send(addr, Msg::Tick) {
        Err(TrySendError::Full(_)) => {}
        other => panic!("expected Full while bootstrap parked, got {other:?}"),
    }

    while runtime.step() > 0 {}
    assert_eq!(delivered.load(Ordering::Acquire), 1);

    // After bootstrap is consumed, the next send fits.
    runtime.try_send(addr, Msg::Tick).unwrap();
    while runtime.step() > 0 {}
    assert_eq!(delivered.load(Ordering::Acquire), 2);
}

#[test]
fn threaded_register_and_bootstrap_delivers_bootstrap_first() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let (svc, delivered, first_was_bootstrap) = fresh_service();

    let addr = runtime
        .register_with_capacity_and_bootstrap::<_, Infallible>(svc, 4, Msg::Bootstrap)
        .expect("threaded bootstrap admitted");

    // Poll for first delivery; threaded runtime processes async.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while delivered.load(Ordering::Acquire) == 0 {
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for bootstrap delivery");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(first_was_bootstrap.load(Ordering::Acquire));

    runtime.try_send(addr, Msg::Stop).unwrap();
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
enum MsMsg {
    Bootstrap,
    Tick,
}

#[derive(Debug)]
struct MsService {
    shard: TestShard,
    delivered: Arc<AtomicU32>,
    first_was_bootstrap: Arc<std::sync::atomic::AtomicBool>,
}

#[tina_runtime::isolate(message = MsMsg, shard = TestShard)]
impl MsService {
    fn handle(
        &mut self,
        msg: MsMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        let n = self.delivered.fetch_add(1, Ordering::AcqRel);
        if n == 0 {
            self.first_was_bootstrap
                .store(matches!(msg, MsMsg::Bootstrap), Ordering::Release);
        }
        let _ = self.shard;
        match msg {
            MsMsg::Bootstrap | MsMsg::Tick => noop(),
        }
    }
}

#[test]
fn multi_shard_register_with_capacity_and_bootstrap_on() {
    let mut multi = MultiShardRuntime::new([TestShard(11), TestShard(22)], DefaultMailboxFactory);
    let target_shard = ShardId::new(22);
    let delivered = Arc::new(AtomicU32::new(0));
    let first_was_bootstrap = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let svc = MsService {
        shard: TestShard(22),
        delivered: Arc::clone(&delivered),
        first_was_bootstrap: Arc::clone(&first_was_bootstrap),
    };
    let _addr = multi
        .register_with_capacity_and_bootstrap_on::<_, Infallible>(
            target_shard,
            svc,
            4,
            MsMsg::Bootstrap,
        )
        .expect("bootstrap admitted on shard 22");

    while multi.step() > 0 {}
    assert_eq!(delivered.load(Ordering::Acquire), 1);
    assert!(first_was_bootstrap.load(Ordering::Acquire));
}

#[test]
fn threaded_multi_shard_register_with_capacity_and_bootstrap_on() {
    let runtime = ThreadedMultiShardRuntime::new(
        [TestShard(11), TestShard(22)],
        DefaultThreadedMailboxFactory,
    );
    let target_shard = ShardId::new(22);
    let delivered = Arc::new(AtomicU32::new(0));
    let first_was_bootstrap = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let svc = MsService {
        shard: TestShard(22),
        delivered: Arc::clone(&delivered),
        first_was_bootstrap: Arc::clone(&first_was_bootstrap),
    };
    let _addr = runtime
        .register_with_capacity_and_bootstrap_on::<_, Infallible>(
            target_shard,
            svc,
            4,
            MsMsg::Bootstrap,
        )
        .expect("bootstrap admitted on threaded shard 22");

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while delivered.load(Ordering::Acquire) == 0 {
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for threaded multi-shard bootstrap delivery");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(first_was_bootstrap.load(Ordering::Acquire));
    let _ = runtime.shutdown();
}

#[test]
fn bootstrap_can_schedule_first_recurring_tick() {
    // The bootstrap handler runs as an ordinary mailbox turn; from that turn
    // an isolate can schedule a sleep and continue work without any host-side
    // try_send.

    #[derive(Debug)]
    #[allow(dead_code)]
    enum SchedMsg {
        Bootstrap,
        Tick(tina_runtime::SleepReply),
    }

    #[derive(Debug)]
    struct Sched {
        delivered: Arc<AtomicU32>,
        tick_count: Arc<AtomicU32>,
    }

    #[tina_runtime::isolate(message = SchedMsg)]
    impl Sched {
        fn handle(
            &mut self,
            msg: SchedMsg,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            self.delivered.fetch_add(1, Ordering::AcqRel);
            match msg {
                SchedMsg::Bootstrap => {
                    tina_runtime::sleep(Duration::from_millis(1)).then(SchedMsg::Tick)
                }
                SchedMsg::Tick(_) => {
                    self.tick_count.fetch_add(1, Ordering::AcqRel);
                    noop()
                }
            }
        }
    }

    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let delivered = Arc::new(AtomicU32::new(0));
    let tick_count = Arc::new(AtomicU32::new(0));
    let svc = Sched {
        delivered: Arc::clone(&delivered),
        tick_count: Arc::clone(&tick_count),
    };
    let _ = runtime
        .register_with_capacity_and_bootstrap::<_, Infallible>(svc, 4, SchedMsg::Bootstrap)
        .expect("bootstrap admitted");

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while tick_count.load(Ordering::Acquire) == 0 {
        if std::time::Instant::now() >= deadline {
            panic!("bootstrap did not schedule a tick");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(delivered.load(Ordering::Acquire) >= 2);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn capacity_zero_mailbox_refuses_prefill_and_leaves_no_address() {
    // Capacity 0 makes prefill impossible. The helper must return a typed
    // error and not insert a registry entry.
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let (svc, delivered, _) = fresh_service();
    match runtime.register_with_capacity_and_bootstrap::<_, Infallible>(svc, 0, Msg::Bootstrap) {
        Err(RegisterBootstrapError::Full(Msg::Bootstrap)) => {}
        Err(RegisterBootstrapError::Closed(_)) => {}
        Ok(_) => panic!("capacity 0 must refuse the prefill"),
        Err(other) => panic!("unexpected error variant: {other:?}"),
    }
    while runtime.step() > 0 {}
    assert_eq!(
        delivered.load(Ordering::Acquire),
        0,
        "no message should be delivered after failed bootstrap prefill"
    );
}

#[test]
fn threaded_bootstrap_error_passes_through() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let (svc, _, _) = fresh_service();
    match runtime.register_with_capacity_and_bootstrap::<_, Infallible>(svc, 0, Msg::Bootstrap) {
        Err(ThreadedRegisterBootstrapError::Full(Msg::Bootstrap))
        | Err(ThreadedRegisterBootstrapError::Closed(_)) => {}
        Ok(_) => panic!("capacity 0 must refuse the threaded prefill"),
        Err(other) => panic!("unexpected threaded error variant: {other:?}"),
    }

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn bootstrap_can_call_back_through_runtime() {
    // Bootstrap handler can do anything an ordinary handler does. Prove that
    // an isolate registered through the helper can be `call`ed and replies.

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum EchoReply {
        Ack(u32),
        Bootstrapped,
    }

    #[derive(Debug)]
    enum EchoMsg {
        Bootstrap,
        Ask(u32),
    }

    #[derive(Debug)]
    struct Echo {
        booted: bool,
    }

    #[tina_runtime::isolate(message = EchoMsg, reply = EchoReply)]
    impl Echo {
        fn handle(
            &mut self,
            msg: EchoMsg,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                EchoMsg::Bootstrap => {
                    self.booted = true;
                    noop()
                }
                EchoMsg::Ask(_) => noop(),
            }
        }

        fn handle_call(&mut self, msg: EchoMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
            match msg {
                EchoMsg::Bootstrap => call.reply(EchoReply::Bootstrapped),
                EchoMsg::Ask(n) => call.reply(EchoReply::Ack(n)),
            }
        }
    }

    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let addr = runtime
        .register_with_capacity_and_bootstrap::<_, Infallible>(
            Echo { booted: false },
            4,
            EchoMsg::Bootstrap,
        )
        .expect("threaded bootstrap admitted");

    let outcome = runtime
        .call_blocking(addr, EchoMsg::Ask(7), Duration::from_secs(2))
        .unwrap();
    assert!(matches!(outcome, CallOutcome::Replied(EchoReply::Ack(7))));

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
