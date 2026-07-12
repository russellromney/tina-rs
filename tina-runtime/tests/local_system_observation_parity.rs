use std::convert::Infallible;
use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};

use tina::{
    AddressGeneration, RestartBudget, RestartPolicy, RestartableChildDefinition, prelude::*,
};
use tina_runtime::{
    CallError, DefaultThreadedMailboxFactory, ListenerId, LocalSystem, RuntimeEventKind,
    SuperviseError, ThreadedRuntimeError, ThreadedSendObservedError, TraceSnapshot, WaitError,
    tcp_bind, tcp_close_listener,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug)]
enum BindMsg {
    Start,
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Closed(Result<(), CallError>),
}

struct Binder {
    address: SocketAddr,
}

#[tina_runtime::isolate(message = BindMsg, shard = AppShard)]
impl Binder {
    fn handle(
        &mut self,
        message: BindMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            BindMsg::Start => tcp_bind(self.address).then(BindMsg::Bound),
            BindMsg::Bound(Ok((listener, _))) => tcp_close_listener(listener).then(BindMsg::Closed),
            BindMsg::Bound(Err(_)) => stop(),
            BindMsg::Closed(result) => {
                let _ = result;
                stop()
            }
        }
    }
}

#[derive(Debug)]
enum StopMsg {
    Stop,
}

struct Stopper;

#[tina_runtime::isolate(message = StopMsg, shard = AppShard)]
impl Stopper {
    fn handle(
        &mut self,
        message: StopMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            StopMsg::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ChildMsg {
    Ping,
}

struct Child;

#[tina_runtime::isolate(message = ChildMsg, shard = AppShard)]
impl Child {
    fn handle(
        &mut self,
        message: ChildMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ChildMsg::Ping => noop(),
        }
    }
}

#[derive(Debug)]
enum ParentMsg {
    Spawn,
    Restart,
}

struct Parent;

#[tina_runtime::isolate(
    message = ParentMsg,
    spawn = RestartableChildDefinition<Child>,
    shard = AppShard
)]
impl Parent {
    fn handle(
        &mut self,
        message: ParentMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ParentMsg::Spawn => spawn(
                RestartableChildDefinition::new(|| Child, 4)
                    .with_initial_message(|| ChildMsg::Ping),
            ),
            ParentMsg::Restart => restart_children(),
        }
    }
}

#[derive(Debug)]
enum PressureMsg {
    Block,
    Hit,
}

struct PressureDriver {
    sink: Address<PressureMsg>,
}

#[tina_runtime::isolate(
    message = PressureMsg,
    send = Outbound<PressureMsg>,
    shard = AppShard
)]
impl PressureDriver {
    fn handle(
        &mut self,
        message: PressureMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            PressureMsg::Block => batch([
                send(self.sink, PressureMsg::Hit),
                send(self.sink, PressureMsg::Hit),
            ]),
            PressureMsg::Hit => noop(),
        }
    }
}

struct PressureSink;

#[tina_runtime::isolate(message = PressureMsg, shard = AppShard)]
impl PressureSink {
    fn handle(
        &mut self,
        _message: PressureMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

fn supervisor_config() -> SupervisorConfig {
    SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4))
}

fn wait_for_spawn(mut trace: impl FnMut() -> TraceSnapshot) -> tina::IsolateId {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(child) = trace()
            .events()
            .iter()
            .find_map(|event| match event.kind() {
                RuntimeEventKind::Spawned { child_isolate } => Some(child_isolate),
                _ => None,
            })
        {
            return child;
        }
        assert!(Instant::now() < deadline, "child spawn was not traced");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn local_system_bound_waiter_is_registered_before_bind() {
    let app = LocalSystem::single_shard(AppShard(1), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system");
    let requested: SocketAddr = "127.0.0.1:0".parse().expect("socket address");
    let binder = app
        .register_root::<Binder, Infallible>(Binder { address: requested }, 4)
        .expect("register binder");

    let waiter = app.observe_next_bound().expect("register bind observer");
    app.try_send(binder, BindMsg::Start).expect("trigger bind");
    let bound = waiter.wait(Duration::from_secs(2)).expect("bound address");

    assert_eq!(bound.ip(), requested.ip());
    assert_ne!(bound.port(), 0);
    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean shutdown");
}

#[test]
fn local_system_bound_waiter_reports_runtime_stopped() {
    let app = LocalSystem::single_shard(AppShard(2), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system");
    let waiter = app.observe_next_bound().expect("register bind observer");
    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean shutdown");

    assert_eq!(
        waiter.wait(Duration::from_secs(1)),
        Err(WaitError::RuntimeStopped)
    );
}

#[test]
fn local_system_isolate_complete_preserves_success_and_stopped_worker() {
    let app = LocalSystem::single_shard(AppShard(3), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system");
    let stopper = app
        .register_root::<Stopper, Infallible>(Stopper, 4)
        .expect("register stopper");
    let complete = app
        .observe_isolate_complete(stopper)
        .expect("register completion observer");
    app.try_send(stopper, StopMsg::Stop).expect("stop isolate");
    complete
        .wait(Duration::from_secs(1))
        .expect("isolate completion");

    let live = app
        .register_root::<Stopper, Infallible>(Stopper, 4)
        .expect("register live stopper");
    let report = app
        .shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("runtime shutdown");
    report.ensure_clean().expect("clean shutdown");
    assert!(matches!(
        app.observe_isolate_complete(live),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
    drop(app);
}

#[test]
fn local_system_try_supervise_keeps_unknown_parent_typed_and_worker_alive() {
    let app = LocalSystem::single_shard(AppShard(4), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system");
    let parent = app
        .register_root::<Stopper, Infallible>(Stopper, 4)
        .expect("register parent");
    let unknown: Address<StopMsg> =
        Address::new_in(parent.system(), AppShard(4).id(), tina::IsolateId::new(999));
    assert_eq!(
        app.try_supervise(unknown, supervisor_config()),
        Ok(Err(SuperviseError::UnknownParent))
    );

    let stale_parent = Address::<StopMsg>::new_with_generation_in(
        parent.system(),
        parent.shard(),
        parent.isolate(),
        AddressGeneration::new(parent.generation().get() + 1),
    );
    assert_eq!(
        app.try_supervise(stale_parent, supervisor_config()),
        Ok(Err(SuperviseError::UnknownParent))
    );
    assert_eq!(app.try_supervise(parent, supervisor_config()), Ok(Ok(())));
    app.shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("runtime shutdown")
        .ensure_clean()
        .expect("clean shutdown");
    assert_eq!(
        app.try_supervise(parent, supervisor_config()),
        Err(ThreadedRuntimeError::WorkerStopped)
    );
}

#[test]
fn local_system_child_restart_waiter_reports_replacement_truth() {
    let app = LocalSystem::single_shard(AppShard(5), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system");
    let parent = app
        .register_root::<Parent, Infallible>(Parent, 8)
        .expect("register parent");
    assert!(app.child_lifecycle_report(parent).is_ok());
    app.supervise(parent, supervisor_config())
        .expect("supervise parent");
    app.try_send(parent, ParentMsg::Spawn).expect("spawn child");
    let original = wait_for_spawn(|| app.trace());

    let stale_parent = Address::<ParentMsg>::new_with_generation_in(
        parent.system(),
        parent.shard(),
        parent.isolate(),
        AddressGeneration::new(parent.generation().get() + 1),
    );
    let foreign_parent = Address::<ParentMsg>::new_with_generation_in(
        parent.system(),
        AppShard(500).id(),
        parent.isolate(),
        parent.generation(),
    );
    assert_eq!(
        app.child_lifecycle_report(stale_parent),
        Err(ThreadedRuntimeError::ParentStopped)
    );
    assert_eq!(
        app.child_lifecycle_report(foreign_parent),
        Err(ThreadedRuntimeError::UnknownShard(foreign_parent.shard()))
    );
    let foreign = LocalSystem::single_shard(AppShard(5), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start foreign single-shard owner");
    assert!(matches!(
        foreign.child_lifecycle_report(parent),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected != actual && actual == parent.system()
    ));
    foreign
        .shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("foreign runtime shutdown")
        .ensure_clean()
        .expect("clean foreign shutdown");
    let stale_waiter = app
        .observe_child_restarted(stale_parent)
        .expect("register restart observer");
    let foreign_error = app
        .observe_child_restarted(foreign_parent)
        .expect_err("foreign shard must be rejected eagerly");
    let waiter = app
        .observe_child_restarted(parent)
        .expect("register restart observer");
    app.try_send(parent, ParentMsg::Restart)
        .expect("restart child");
    let restarted = waiter.wait(Duration::from_secs(2)).expect("restart event");

    assert_eq!(restarted.child_ordinal, 0);
    assert_eq!(restarted.new_shard, AppShard(5).id());
    assert_ne!(restarted.new_isolate, original);
    assert_eq!(
        stale_waiter.wait(Duration::from_millis(10)),
        Err(WaitError::AlreadyStopped),
        "stale parent authority must be rejected before claiming a restart"
    );
    assert_eq!(
        foreign_error,
        ThreadedRuntimeError::UnknownShard(foreign_parent.shard()),
        "same-id foreign parent authority must be rejected before claiming a restart"
    );
    assert_eq!(
        app.observe_child_restarted(parent)
            .expect("register restart observer")
            .wait(Duration::from_millis(10)),
        Err(WaitError::Timeout),
        "restart facts are not replayed to late facade waiters"
    );
    app.shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("runtime shutdown")
        .ensure_clean()
        .expect("clean shutdown");
    assert!(matches!(
        app.observe_child_restarted(parent),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
}

#[test]
fn local_multi_shard_child_restart_routes_and_rejects_foreign_owner() {
    let app = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(AppShard(10))
        .shard(AppShard(20))
        .try_build()
        .expect("start multi-shard local system");
    let parent = app
        .register_root_on::<Parent, Infallible>(AppShard(20).id(), Parent, 8)
        .expect("register parent on second shard");
    assert!(app.child_lifecycle_report(parent).is_ok());
    app.supervise(parent, supervisor_config())
        .expect("supervise parent");
    app.try_send(parent, ParentMsg::Spawn).expect("spawn child");
    let original = wait_for_spawn(|| app.trace());
    assert_eq!(
        app.child_lifecycle_report(parent)
            .expect("report after child spawn")
            .children
            .len(),
        1
    );

    let stale_parent = Address::<ParentMsg>::new_with_generation_in(
        parent.system(),
        parent.shard(),
        parent.isolate(),
        AddressGeneration::new(parent.generation().get() + 1),
    );
    let unknown_shard_parent = Address::<ParentMsg>::new_with_generation_in(
        parent.system(),
        AppShard(99).id(),
        parent.isolate(),
        parent.generation(),
    );
    assert_eq!(
        app.child_lifecycle_report(stale_parent),
        Err(ThreadedRuntimeError::ParentStopped)
    );
    assert_eq!(
        app.child_lifecycle_report(unknown_shard_parent),
        Err(ThreadedRuntimeError::UnknownShard(AppShard(99).id()))
    );

    let waiter = app
        .observe_child_restarted(parent)
        .expect("route waiter to parent shard");
    app.try_send(parent, ParentMsg::Restart)
        .expect("restart child");
    let restarted = waiter.wait(Duration::from_secs(2)).expect("restart event");
    assert_eq!(restarted.new_shard, AppShard(20).id());
    assert_ne!(restarted.new_isolate, original);

    let foreign = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(AppShard(30))
        .try_build()
        .expect("start foreign owner");
    assert!(matches!(
        foreign.observe_child_restarted(parent),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected != actual && actual == parent.system()
    ));
    assert!(matches!(
        foreign.child_lifecycle_report(parent),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected != actual && actual == parent.system()
    ));
    foreign
        .shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean foreign shutdown");
    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean owner shutdown");
}

#[test]
fn local_system_pressure_summary_observes_full_then_refill() {
    let app = LocalSystem::single_shard(AppShard(6), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system");
    let sink = app
        .register_root::<PressureSink, Infallible>(PressureSink, 1)
        .expect("register pressure sink");
    let driver = app
        .register_root::<PressureDriver, PressureMsg>(PressureDriver { sink }, 4)
        .expect("register pressure driver");

    app.try_send(driver, PressureMsg::Block)
        .expect("trigger bounded local sends");

    let pressure_deadline = Instant::now() + Duration::from_secs(2);
    let observed_full = loop {
        let summary = app.pressure_summary().expect("pressure summary");
        if summary.send_rejected_full > 0 {
            break summary.send_rejected_full;
        }
        assert!(
            Instant::now() < pressure_deadline,
            "mailbox Full was not observed"
        );
        thread::sleep(Duration::from_millis(1));
    };

    let refill_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match app.send_and_observe(sink, PressureMsg::Hit) {
            Ok(()) => break,
            Err(ThreadedSendObservedError::MailboxFull) => {
                assert!(Instant::now() < refill_deadline, "sink did not refill");
                thread::sleep(Duration::from_millis(1));
            }
            Err(other) => panic!("unexpected refill outcome: {other:?}"),
        }
    }
    assert!(observed_full > 0);
    assert!(app.pressure_summary().expect("final summary").any_full());
    app.shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("runtime shutdown")
        .ensure_clean()
        .expect("clean shutdown");
    assert!(matches!(
        app.pressure_summary(),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
}
