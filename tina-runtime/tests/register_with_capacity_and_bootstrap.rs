//! Register-and-bootstrap helpers prefill the mailbox with the bootstrap
//! message before inserting the isolate entry. Tests pin: first delivered
//! message is the bootstrap, no host `try_send(Bootstrap)` is needed, sending
//! immediately after the helper returns can see `Full` when capacity is 1,
//! and a custom mailbox that refuses the prefill leaves no registered isolate.

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina::{Mailbox, TrySendError};
use tina_runtime::{
    CallOutcome, DefaultMailboxFactory, DefaultThreadedMailboxFactory, MailboxFactory,
    MultiShardRuntime, RegisterBootstrapError, Runtime, ThreadedMultiShardRuntime,
    ThreadedRegisterBootstrapError, ThreadedRuntime, ThreadedRuntimeConfig, ThreadedRuntimeError,
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

#[derive(Debug)]
enum GateMsg {
    Hold,
}

struct Gate {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

#[derive(Debug)]
struct DropProbe(Arc<AtomicU32>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
enum OwnedBootstrapMsg {
    Bootstrap(DropProbe),
}

struct OwnedBootstrapService {
    drops: Arc<AtomicU32>,
}

impl Drop for OwnedBootstrapService {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::AcqRel);
    }
}

#[tina_runtime::isolate(message = OwnedBootstrapMsg)]
impl OwnedBootstrapService {
    fn handle(
        &mut self,
        message: OwnedBootstrapMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            OwnedBootstrapMsg::Bootstrap(_authority) => noop(),
        }
    }
}

#[derive(Debug, Default)]
struct FailureGate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl FailureGate {
    fn enter_and_wait(&self) {
        let mut state = self.state.lock().expect("failure gate");
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).expect("failure gate wait");
        }
    }

    fn wait_until_entered(&self) {
        let mut state = self.state.lock().expect("failure gate");
        while !state.0 {
            state = self.changed.wait(state).expect("failure gate wait");
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("failure gate");
        state.1 = true;
        self.changed.notify_all();
    }
}

#[derive(Debug, Clone)]
struct TransitionPanicMailboxFactory {
    gate: Arc<FailureGate>,
}

impl MailboxFactory for TransitionPanicMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        if capacity == 13 {
            self.gate.enter_and_wait();
            panic!("intentional worker transition failure");
        }
        DefaultThreadedMailboxFactory.create(capacity)
    }
}

#[tina_runtime::isolate(message = GateMsg)]
impl Gate {
    fn handle(
        &mut self,
        _msg: GateMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        self.entered.store(true, Ordering::Release);
        while !self.release.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        noop()
    }
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

#[test]
fn threaded_bootstrap_command_full_returns_message_without_late_registration() {
    let runtime = Arc::new(ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 1,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let gate = runtime
        .register_with_capacity::<Gate, Infallible>(
            Gate {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
            2,
        )
        .expect("register gate");
    runtime
        .try_send(gate, GateMsg::Hold)
        .expect("occupy worker");
    while !entered.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    runtime.try_send(gate, GateMsg::Hold).expect("fill ingress");

    let (service, delivered, _) = fresh_service();
    let runtime_for_register = Arc::clone(&runtime);
    let (tx, rx) = std::sync::mpsc::channel();
    let register = std::thread::spawn(move || {
        let result = runtime_for_register
            .register_with_capacity_and_bootstrap::<_, Infallible>(service, 4, Msg::Bootstrap)
            .map(|_| ());
        tx.send(result).expect("report bootstrap registration");
    });
    let result = rx.recv_timeout(Duration::from_millis(200));
    if result.is_err() {
        release.store(true, Ordering::Release);
        register
            .join()
            .expect("release blocked legacy registration");
        panic!("bootstrap registration blocked instead of returning its message");
    }
    let bootstrap = match result.unwrap() {
        Err(ThreadedRegisterBootstrapError::CommandFull(message)) => message,
        other => panic!("expected recoverable command full, got {other:?}"),
    };
    register.join().expect("registration thread");
    assert_eq!(bootstrap, Msg::Bootstrap);
    assert_eq!(delivered.load(Ordering::Acquire), 0);

    release.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(2);
    let retry_delivered = Arc::new(AtomicU32::new(0));
    let retry_first = Arc::new(AtomicBool::new(false));
    let mut pending = Some(bootstrap);
    loop {
        let message = pending.take().expect("retry authority retained");
        let service = Service {
            delivered: Arc::clone(&retry_delivered),
            first_was_bootstrap: Arc::clone(&retry_first),
        };
        match runtime.register_with_capacity_and_bootstrap::<_, Infallible>(service, 4, message) {
            Ok(_) => break,
            Err(ThreadedRegisterBootstrapError::CommandFull(message))
                if Instant::now() < deadline =>
            {
                pending = Some(message);
                std::thread::yield_now();
            }
            Err(error) => panic!("bootstrap retry after refill failed: {error}"),
        }
    }
    while retry_delivered.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(
        delivered.load(Ordering::Acquire),
        0,
        "full command ran late"
    );
    assert_eq!(retry_delivered.load(Ordering::Acquire), 1);
    Arc::try_unwrap(runtime)
        .unwrap_or_else(|_| panic!("sole runtime owner"))
        .shutdown()
        .expect("clean shutdown");
}

#[test]
fn threaded_bootstrap_closed_command_channel_returns_message() {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    runtime
        .shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("stop worker");
    let (service, delivered, _) = fresh_service();
    match runtime.register_with_capacity_and_bootstrap::<_, Infallible>(service, 4, Msg::Bootstrap)
    {
        Err(ThreadedRegisterBootstrapError::CommandClosed(Msg::Bootstrap)) => {}
        other => panic!("expected recoverable closed command channel, got {other:?}"),
    }
    assert_eq!(delivered.load(Ordering::Acquire), 0);
}

#[test]
fn threaded_bootstrap_accepted_before_worker_failure_does_not_return_message_authority() {
    let gate = Arc::new(FailureGate::default());
    let runtime = Arc::new(ThreadedRuntime::with_config(
        SingleShard,
        TransitionPanicMailboxFactory {
            gate: Arc::clone(&gate),
        },
        ThreadedRuntimeConfig {
            command_capacity: 1,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));

    let failing_runtime = Arc::clone(&runtime);
    let failure = std::thread::spawn(move || {
        let (service, _, _) = fresh_service();
        failing_runtime.register_with_capacity::<Service, Infallible>(service, 13)
    });
    gate.wait_until_entered();

    let service_drops = [Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0))];
    let message_drops = [Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0))];
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let mut registrations = Vec::new();
    for index in 0..2 {
        let runtime = Arc::clone(&runtime);
        let result_tx = result_tx.clone();
        let service_drops = Arc::clone(&service_drops[index]);
        let message_drops = Arc::clone(&message_drops[index]);
        registrations.push(std::thread::spawn(move || {
            let result = runtime.register_with_capacity_and_bootstrap::<_, Infallible>(
                OwnedBootstrapService {
                    drops: service_drops,
                },
                4,
                OwnedBootstrapMsg::Bootstrap(DropProbe(message_drops)),
            );
            result_tx
                .send((index, result.map(|_| ())))
                .expect("report bootstrap registration");
        }));
    }
    drop(result_tx);

    let (rejected_index, rejected) = result_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("one registration must be rejected by the full command queue");
    assert!(matches!(
        rejected,
        Err(ThreadedRegisterBootstrapError::CommandFull(
            OwnedBootstrapMsg::Bootstrap(_)
        ))
    ));
    drop(rejected);
    assert_eq!(service_drops[rejected_index].load(Ordering::Acquire), 1);
    assert_eq!(message_drops[rejected_index].load(Ordering::Acquire), 1);

    let accepted_index = 1 - rejected_index;
    assert_eq!(service_drops[accepted_index].load(Ordering::Acquire), 0);
    assert_eq!(message_drops[accepted_index].load(Ordering::Acquire), 0);

    gate.release();
    assert!(matches!(
        failure.join().expect("failure registration joins"),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
    let (reported_index, accepted) = result_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("accepted registration settles after worker failure");
    assert_eq!(reported_index, accepted_index);
    assert!(matches!(
        accepted,
        Err(ThreadedRegisterBootstrapError::WorkerStopped)
    ));
    assert_eq!(service_drops[accepted_index].load(Ordering::Acquire), 1);
    assert_eq!(message_drops[accepted_index].load(Ordering::Acquire), 1);
    assert!(result_rx.recv_timeout(Duration::from_millis(20)).is_err());

    for registration in registrations {
        registration.join().expect("bootstrap registration joins");
    }
    let runtime = Arc::try_unwrap(runtime).unwrap_or_else(|_| panic!("sole runtime owner"));
    assert_eq!(runtime.shutdown(), Err(ThreadedRuntimeError::WorkerStopped));
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

struct MsGate {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

#[tina_runtime::isolate(message = MsMsg, shard = TestShard)]
impl MsGate {
    fn handle(
        &mut self,
        _msg: MsMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        self.entered.store(true, Ordering::Release);
        while !self.release.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        noop()
    }
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
fn threaded_multi_shard_bootstrap_full_returns_message_without_late_registration() {
    let runtime = Arc::new(ThreadedMultiShardRuntime::with_config(
        [TestShard(11), TestShard(22)],
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 1,
            shard_pair_capacity: 4,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let gate = runtime
        .register_with_capacity_on::<MsGate, Infallible>(
            ShardId::new(22),
            MsGate {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
            2,
        )
        .expect("register shard gate");
    runtime.try_send(gate, MsMsg::Tick).expect("occupy shard");
    while !entered.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    runtime
        .try_send(gate, MsMsg::Tick)
        .expect("fill shard ingress");

    let delivered = Arc::new(AtomicU32::new(0));
    let service = MsService {
        shard: TestShard(22),
        delivered: Arc::clone(&delivered),
        first_was_bootstrap: Arc::new(AtomicBool::new(false)),
    };
    let runtime_for_register = Arc::clone(&runtime);
    let (tx, rx) = std::sync::mpsc::channel();
    let register = std::thread::spawn(move || {
        let result = runtime_for_register
            .register_with_capacity_and_bootstrap_on::<_, Infallible>(
                ShardId::new(22),
                service,
                4,
                MsMsg::Bootstrap,
            )
            .map(|_| ());
        tx.send(result)
            .expect("report multi bootstrap registration");
    });
    let result = rx.recv_timeout(Duration::from_millis(200));
    if result.is_err() {
        release.store(true, Ordering::Release);
        register
            .join()
            .expect("release blocked legacy registration");
        panic!("multi bootstrap registration blocked instead of returning authority");
    }
    assert!(matches!(
        result.unwrap(),
        Err(ThreadedRegisterBootstrapError::CommandFull(
            MsMsg::Bootstrap
        ))
    ));
    register.join().expect("registration thread");
    assert_eq!(delivered.load(Ordering::Acquire), 0);
    release.store(true, Ordering::Release);
    runtime
        .shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("bounded multi shutdown");
    assert_eq!(
        delivered.load(Ordering::Acquire),
        0,
        "full command ran late"
    );
}

#[test]
fn threaded_multi_shard_unknown_bootstrap_target_returns_message() {
    let runtime = ThreadedMultiShardRuntime::new(
        [TestShard(11), TestShard(22)],
        DefaultThreadedMailboxFactory,
    );
    let delivered = Arc::new(AtomicU32::new(0));
    let service = MsService {
        shard: TestShard(99),
        delivered: Arc::clone(&delivered),
        first_was_bootstrap: Arc::new(AtomicBool::new(false)),
    };
    match runtime.register_with_capacity_and_bootstrap_on::<_, Infallible>(
        ShardId::new(99),
        service,
        4,
        MsMsg::Bootstrap,
    ) {
        Err(ThreadedRegisterBootstrapError::UnknownShard(shard, MsMsg::Bootstrap)) => {
            assert_eq!(shard, ShardId::new(99))
        }
        other => panic!("expected recoverable unknown shard, got {other:?}"),
    }
    assert_eq!(delivered.load(Ordering::Acquire), 0);
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
fn bootstrap_can_call_back_through_runtime_and_was_delivered_first() {
    // Bootstrap handler can do anything an ordinary handler does. Prove that
    // an isolate registered through the helper saw Bootstrap as its first
    // delivered message AND can be `call`ed afterwards.

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum EchoReply {
        AckBooted { booted: bool, value: u32 },
    }

    #[derive(Debug)]
    enum EchoMsg {
        Bootstrap,
        AskBooted(u32),
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
                EchoMsg::AskBooted(_) => noop(),
            }
        }

        fn handle_call(&mut self, msg: EchoMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
            match msg {
                EchoMsg::Bootstrap => call.reject(tina::CallRejectedReason::UnsupportedMessage),
                EchoMsg::AskBooted(value) => call.reply(EchoReply::AckBooted {
                    booted: self.booted,
                    value,
                }),
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
        .call_blocking(addr, EchoMsg::AskBooted(7), Duration::from_secs(2))
        .unwrap();
    match outcome {
        CallOutcome::Replied(EchoReply::AckBooted { booted, value }) => {
            assert!(
                booted,
                "Bootstrap must have been delivered before AskBooted"
            );
            assert_eq!(value, 7);
        }
        other => panic!("expected AckBooted, got {other:?}"),
    }

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
