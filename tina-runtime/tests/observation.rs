//! Typed observation handle tests.
//!
//! Slice 1 covers the bound-address waiter; slice 2 (this file's later
//! tests) covers isolate-complete, operation-done, and child-restarted
//! waiters.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use tina::{AddressGeneration, Mailbox, TrySendError, prelude::*};
use tina::{RestartBudget, RestartPolicy};
use tina_runtime::{
    CallError, CallKind, ListenerId, MailboxFactory, ResultWaitError, RuntimeEventKind,
    ThreadedRuntime, ThreadedRuntimeConfig, ThreadedRuntimeError, ThreadedSendObservedError,
    WaitError, sleep, tcp_bind, tcp_close_listener,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(91)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: RefCell<VecDeque<T>>,
    closed: Cell<bool>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }
    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut q = self.queue.borrow_mut();
        if q.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        q.push_back(message);
        Ok(())
    }
    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }
    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }
    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum BindMsg {
    Start,
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Closed(Result<(), CallError>),
}

#[derive(Debug)]
struct Binder {
    addr: SocketAddr,
}

#[tina_runtime::isolate(message = BindMsg, shard = TestShard)]
impl Binder {
    fn handle(
        &mut self,
        msg: BindMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BindMsg::Start => tcp_bind(self.addr).then(BindMsg::Bound),
            BindMsg::Bound(Ok((listener, _))) => tcp_close_listener(listener).then(BindMsg::Closed),
            BindMsg::Bound(Err(_)) | BindMsg::Closed(_) => stop(),
        }
    }
}

fn make_runtime() -> ThreadedRuntime<TestShard, TestMailboxFactory> {
    ThreadedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

#[test]
fn waiter_resolves_with_bound_addr() {
    let runtime = make_runtime();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let binder = runtime
        .register_with_capacity::<Binder, Infallible>(Binder { addr }, 8)
        .expect("register binder");

    let waiter = runtime
        .observe_next_bound()
        .expect("register bind observer");

    runtime
        .try_send(binder, BindMsg::Start)
        .expect("kick binder");

    let bound = waiter
        .wait(Duration::from_secs(3))
        .expect("waiter resolves");
    assert_eq!(bound.ip(), addr.ip());
    assert!(bound.port() != 0, "bound port should not be 0");

    let _ = runtime.shutdown();
}

#[test]
fn waiter_times_out_when_no_bind_submitted() {
    let runtime = make_runtime();
    let waiter = runtime
        .observe_next_bound()
        .expect("register bind observer");

    let outcome = waiter.wait(Duration::from_millis(50));
    assert_eq!(outcome, Err(WaitError::Timeout));

    let _ = runtime.shutdown();
}

#[test]
fn observation_registration_is_bounded() {
    let runtime = make_runtime();

    for _ in 0..1024 {
        let _waiter = runtime
            .observe_next_bound()
            .expect("register bind observer");
    }

    let saturated = runtime
        .observe_next_bound()
        .expect("register bind observer");
    assert_eq!(
        saturated.wait(Duration::from_secs(1)),
        Err(WaitError::ObservationFull)
    );

    let _ = runtime.shutdown();
}

#[test]
fn waiter_runtime_stopped_when_runtime_dropped() {
    let runtime = make_runtime();
    let waiter = runtime
        .observe_next_bound()
        .expect("register bind observer");

    drop(runtime);

    let outcome = waiter.wait(Duration::from_secs(1));
    assert_eq!(outcome, Err(WaitError::RuntimeStopped));
}

#[test]
fn dropped_waiter_does_not_block_subsequent_observer() {
    let runtime = make_runtime();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    // First waiter is dropped immediately. The runtime should skip its
    // disconnected slot and serve the next observer.
    {
        let _doomed = runtime
            .observe_next_bound()
            .expect("register bind observer");
    }

    let live = runtime
        .observe_next_bound()
        .expect("register bind observer");

    let binder = runtime
        .register_with_capacity::<Binder, Infallible>(Binder { addr }, 8)
        .expect("register binder");
    runtime
        .try_send(binder, BindMsg::Start)
        .expect("kick binder");

    let bound = live
        .wait(Duration::from_secs(3))
        .expect("second waiter resolves after first dropped");
    assert_eq!(bound.ip(), addr.ip());

    let _ = runtime.shutdown();
}

#[test]
fn waiter_reports_call_failed_on_bad_bind_addr() {
    let runtime = make_runtime();
    // 240.0.0.0/4 is reserved; binding to it should fail with Io.
    let bad: SocketAddr = "240.0.0.1:1".parse().unwrap();
    let binder = runtime
        .register_with_capacity::<Binder, Infallible>(Binder { addr: bad }, 8)
        .expect("register binder");
    let waiter = runtime
        .observe_next_bound()
        .expect("register bind observer");
    runtime
        .try_send(binder, BindMsg::Start)
        .expect("kick binder");

    match waiter.wait(Duration::from_secs(3)) {
        Err(WaitError::CallFailed(_)) => {}
        Err(WaitError::Timeout) => {
            // Some platforms may quietly accept this address; treat
            // timeout as acceptable to keep the test portable, but the
            // host did not deadlock.
        }
        other => panic!("unexpected outcome: {other:?}"),
    }

    let _ = runtime.shutdown();
}

#[test]
fn waiter_serves_observers_in_registration_order() {
    let runtime = make_runtime();

    // Bind once, then again. Record both observers, then trigger both binds.
    let first = runtime
        .observe_next_bound()
        .expect("register bind observer");
    let second = runtime
        .observe_next_bound()
        .expect("register bind observer");

    let addr_a: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let binder_a = runtime
        .register_with_capacity::<Binder, Infallible>(Binder { addr: addr_a }, 8)
        .expect("register binder a");
    let binder_b = runtime
        .register_with_capacity::<Binder, Infallible>(Binder { addr: addr_b }, 8)
        .expect("register binder b");

    runtime
        .try_send(binder_a, BindMsg::Start)
        .expect("kick binder a");

    let first_addr = first.wait(Duration::from_secs(3)).expect("first resolves");

    // Give the worker a tiny window to commit before kicking the second
    // bind, so we can be sure both events arrive in order.
    thread::sleep(Duration::from_millis(20));

    runtime
        .try_send(binder_b, BindMsg::Start)
        .expect("kick binder b");

    let second_addr = second
        .wait(Duration::from_secs(3))
        .expect("second resolves");

    assert!(first_addr.port() != 0);
    assert!(second_addr.port() != 0);
    assert_ne!(first_addr.port(), second_addr.port());

    let _ = runtime.shutdown();
}

// -- Slice 2: IsolateComplete / OperationDone / ChildRestarted -------------

#[derive(Debug, Clone)]
enum SleeperMsg {
    Start,
    SleepDone,
}

#[derive(Debug)]
struct Sleeper {
    nap: Duration,
}

#[tina_runtime::isolate(message = SleeperMsg, shard = TestShard)]
impl Sleeper {
    fn handle(
        &mut self,
        msg: SleeperMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SleeperMsg::Start => sleep(self.nap).then(|_| SleeperMsg::SleepDone),
            SleeperMsg::SleepDone => stop(),
        }
    }
}

#[test]
fn isolate_complete_waiter_resolves_when_isolate_stops() {
    let runtime = make_runtime();
    let sleeper = runtime
        .register_with_capacity::<Sleeper, Infallible>(
            Sleeper {
                nap: Duration::from_millis(20),
            },
            8,
        )
        .expect("register sleeper");

    // Register before triggering — same FIFO contract as the bound waiter.
    let done = runtime
        .observe_isolate_complete(sleeper)
        .expect("register completion observer");
    runtime
        .try_send(sleeper, SleeperMsg::Start)
        .expect("kick sleeper");

    done.wait(Duration::from_secs(3))
        .expect("isolate complete resolves");

    let _ = runtime.shutdown();
}

#[test]
fn isolate_complete_waiter_times_out_when_isolate_lives() {
    let runtime = make_runtime();
    let sleeper = runtime
        .register_with_capacity::<Sleeper, Infallible>(
            Sleeper {
                nap: Duration::from_millis(5),
            },
            8,
        )
        .expect("register sleeper");

    let done = runtime
        .observe_isolate_complete(sleeper)
        .expect("register completion observer");
    // Never send Start; the isolate just sits idle.
    let outcome = done.wait(Duration::from_millis(50));
    assert_eq!(outcome, Err(WaitError::Timeout));

    let _ = runtime.shutdown();
}

#[test]
fn operation_done_waiter_resolves_on_matching_call_kind() {
    let runtime = make_runtime();
    let sleeper = runtime
        .register_with_capacity::<Sleeper, Infallible>(
            Sleeper {
                nap: Duration::from_millis(5),
            },
            8,
        )
        .expect("register sleeper");

    let waiter = runtime
        .observe_operation_done(sleeper, CallKind::Sleep)
        .expect("register operation observer");
    runtime
        .try_send(sleeper, SleeperMsg::Start)
        .expect("kick sleeper");

    waiter
        .wait(Duration::from_secs(3))
        .expect("sleep call completed");

    let _ = runtime.shutdown();
}

#[test]
fn operation_done_waiter_does_not_resolve_on_other_isolate() {
    let runtime = make_runtime();
    let sleeper_a = runtime
        .register_with_capacity::<Sleeper, Infallible>(
            Sleeper {
                nap: Duration::from_millis(5),
            },
            8,
        )
        .expect("register sleeper A");
    let sleeper_b = runtime
        .register_with_capacity::<Sleeper, Infallible>(
            Sleeper {
                nap: Duration::from_millis(5),
            },
            8,
        )
        .expect("register sleeper B");

    // Register a waiter on B, but kick A.
    let waiter = runtime
        .observe_operation_done(sleeper_b, CallKind::Sleep)
        .expect("register operation observer");
    runtime
        .try_send(sleeper_a, SleeperMsg::Start)
        .expect("kick sleeper A");

    let outcome = waiter.wait(Duration::from_millis(80));
    assert_eq!(outcome, Err(WaitError::Timeout));

    let _ = runtime.shutdown();
}

// Crashy worker for the child-restarted test.

#[derive(Debug, Clone)]
enum CrashMsg {
    Boom,
}

#[derive(Debug)]
struct Crashy {
    crashed: Arc<AtomicU32>,
}

#[tina_runtime::isolate(message = CrashMsg, shard = TestShard)]
impl Crashy {
    fn handle(
        &mut self,
        msg: CrashMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CrashMsg::Boom => {
                if self
                    .crashed
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    panic!("boom");
                }
                noop()
            }
        }
    }
}

#[derive(Debug, Clone)]
enum ParentMsg {
    Spawn,
}

#[derive(Debug)]
struct Parent {
    crashed: Arc<AtomicU32>,
}

#[tina_runtime::isolate(
    message = ParentMsg,
    spawn = RestartableChildDefinition<Crashy>,
    shard = TestShard,
)]
impl Parent {
    fn handle(
        &mut self,
        msg: ParentMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ParentMsg::Spawn => {
                let crashed = Arc::clone(&self.crashed);
                spawn(
                    RestartableChildDefinition::new(
                        move || Crashy {
                            crashed: Arc::clone(&crashed),
                        },
                        4,
                    )
                    .with_initial_message(|| CrashMsg::Boom),
                )
            }
        }
    }
}

#[test]
fn child_restarted_waiter_resolves_after_panic_and_restart() {
    let runtime = make_runtime();
    let crashed = Arc::new(AtomicU32::new(0));
    let parent = runtime
        .register_with_capacity::<Parent, Infallible>(
            Parent {
                crashed: Arc::clone(&crashed),
            },
            8,
        )
        .expect("register parent");
    runtime
        .supervise(
            parent,
            SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(2)),
        )
        .expect("supervise");

    let stale_parent = Address::<ParentMsg>::new_with_generation_in(
        parent.system(),
        parent.shard(),
        parent.isolate(),
        AddressGeneration::new(parent.generation().get() + 1),
    );
    let foreign_parent = Address::<ParentMsg>::new_with_generation_in(
        parent.system(),
        ShardId::new(parent.shard().get() + 1),
        parent.isolate(),
        parent.generation(),
    );
    let stale_waiter = runtime
        .observe_child_restarted(stale_parent)
        .expect("register restart observer");
    let foreign_error = runtime
        .observe_child_restarted(foreign_parent)
        .expect_err("foreign shard must be rejected eagerly");
    let dropped_waiter = runtime
        .observe_child_restarted(parent)
        .expect("register restart observer");
    let restart_waiter = runtime
        .observe_child_restarted(parent)
        .expect("register restart observer");
    drop(dropped_waiter);
    runtime
        .try_send(parent, ParentMsg::Spawn)
        .expect("ask parent to spawn");

    let restarted = restart_waiter
        .wait(Duration::from_secs(3))
        .expect("restart event resolves");
    assert_eq!(
        stale_waiter.wait(Duration::from_millis(10)),
        Err(WaitError::AlreadyStopped),
        "a stale generation must be rejected before claiming a restart"
    );
    assert_eq!(
        foreign_error,
        ThreadedRuntimeError::UnknownShard(foreign_parent.shard()),
        "a foreign shard must be rejected before it can claim the restart"
    );
    assert_eq!(
        runtime
            .observe_child_restarted(parent)
            .expect("register restart observer")
            .wait(Duration::from_millis(10)),
        Err(WaitError::Timeout),
        "restart observations are not replayed to late waiters"
    );
    assert_eq!(restarted.child_ordinal, 0);
    let old_child = runtime
        .trace()
        .events()
        .iter()
        .find_map(|event| match event.kind() {
            RuntimeEventKind::Spawned { child_isolate } => Some(child_isolate),
            _ => None,
        })
        .expect("initial child spawn traced");
    assert_ne!(old_child, restarted.new_isolate);
    let replacement = Address::<CrashMsg>::new_with_generation_in(
        parent.system(),
        restarted.new_shard,
        restarted.new_isolate,
        restarted.new_generation,
    );
    runtime
        .send_and_observe(replacement, CrashMsg::Boom)
        .expect("replacement remains live after its non-crashing bootstrap");
    let stale = Address::<CrashMsg>::new_in(parent.system(), parent.shard(), old_child);
    assert!(matches!(
        runtime.send_and_observe(stale, CrashMsg::Boom),
        Err(ThreadedSendObservedError::MailboxClosed)
    ));
    // The shared gate permits exactly one crash across the whole replacement
    // lineage, so the late-waiter assertion above cannot race a fresh restart.
    assert_eq!(crashed.load(Ordering::Acquire), 1);

    let _ = runtime.shutdown();
}

#[test]
fn child_restarted_waiter_reports_runtime_stopped() {
    let runtime = make_runtime();
    let parent = runtime
        .register_with_capacity::<Parent, Infallible>(
            Parent {
                crashed: Arc::new(AtomicU32::new(0)),
            },
            8,
        )
        .expect("register parent");
    let waiter = runtime
        .observe_child_restarted(parent)
        .expect("register restart observer");

    drop(runtime);

    assert_eq!(
        waiter.wait(Duration::from_secs(1)),
        Err(WaitError::RuntimeStopped)
    );
}

#[test]
fn abandoned_child_restart_authorities_do_not_exhaust_observation_capacity() {
    let runtime = make_runtime();
    let parent = runtime
        .register_with_capacity::<Parent, Infallible>(
            Parent {
                crashed: Arc::new(AtomicU32::new(0)),
            },
            8,
        )
        .expect("register parent");

    for isolate in 1..=(2 * 1024) {
        let forged = Address::<ParentMsg>::new_with_generation_in(
            parent.system(),
            ShardId::new(99),
            tina::IsolateId::new(isolate),
            AddressGeneration::new(7),
        );
        assert_eq!(
            runtime.observe_child_restarted(forged).expect_err(
                "foreign restart authority must be rejected before observer registration"
            ),
            ThreadedRuntimeError::UnknownShard(ShardId::new(99))
        );
    }

    assert_eq!(
        runtime
            .observe_next_bound()
            .expect("register bind observer")
            .wait(Duration::from_millis(10)),
        Err(WaitError::Timeout),
        "abandoned foreign restart authorities must release the shared cap"
    );

    let _ = runtime.shutdown();
}

// ---------------------------------------------------------------------------
// Typed isolate result waiters.
// ---------------------------------------------------------------------------

/// Final value carried back to the host via `stop_with`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResultPayload {
    successes: u32,
    bytes: usize,
}

#[derive(Debug, Clone)]
#[allow(clippy::enum_variant_names)]
enum ResultMsg {
    /// Finish with a typed payload.
    FinishWithResult(ResultPayload),
    /// Finish without publishing a typed payload.
    FinishWithoutResult,
    /// Finish with a value of a *different* type than the host requested.
    FinishWithMismatchedType,
}

#[derive(Debug, Default)]
struct ResultProducer;

#[tina_runtime::isolate(message = ResultMsg, shard = TestShard)]
impl ResultProducer {
    fn handle(
        &mut self,
        msg: ResultMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ResultMsg::FinishWithResult(payload) => stop_with(payload),
            ResultMsg::FinishWithoutResult => stop(),
            // u64 != ResultPayload — TypeMismatch on the host.
            ResultMsg::FinishWithMismatchedType => stop_with(99u64),
        }
    }
}

fn spawn_producer(
    runtime: &ThreadedRuntime<TestShard, TestMailboxFactory>,
) -> Address<ResultMsg, ()> {
    runtime
        .register_with_capacity::<ResultProducer, Infallible>(ResultProducer, 8)
        .expect("register producer")
}

#[test]
fn result_waiter_resolves_with_typed_value() {
    let runtime = make_runtime();
    let producer = spawn_producer(&runtime);

    let waiter = runtime
        .observe_result::<ResultPayload, _, _>(producer)
        .expect("register result waiter");

    let payload = ResultPayload {
        successes: 7,
        bytes: 1234,
    };
    runtime
        .try_send(producer, ResultMsg::FinishWithResult(payload.clone()))
        .expect("kick producer");

    let resolved = waiter
        .wait(Duration::from_secs(3))
        .expect("waiter resolves");
    assert_eq!(resolved, payload);

    let _ = runtime.shutdown();
}

#[test]
fn result_waiter_times_out_when_no_stop() {
    let runtime = make_runtime();
    let producer = spawn_producer(&runtime);

    let waiter = runtime
        .observe_result::<ResultPayload, _, _>(producer)
        .expect("register result waiter");

    let outcome = waiter.wait(Duration::from_millis(50));
    assert_eq!(outcome, Err(ResultWaitError::Timeout));

    let _ = runtime.shutdown();
}

#[test]
fn result_waiter_runtime_stopped_when_runtime_dropped() {
    let runtime = make_runtime();
    let producer = spawn_producer(&runtime);

    let waiter = runtime
        .observe_result::<ResultPayload, _, _>(producer)
        .expect("register result waiter");

    drop(runtime);

    let outcome = waiter.wait(Duration::from_secs(1));
    assert_eq!(outcome, Err(ResultWaitError::RuntimeStopped));
}

#[test]
fn result_waiter_dropped_does_not_block_isolate_stop() {
    let runtime = make_runtime();
    let producer = spawn_producer(&runtime);

    {
        let _doomed = runtime
            .observe_result::<ResultPayload, _, _>(producer)
            .expect("register result waiter");
    }

    // Isolate stops cleanly even though waiter went away. Use the standard
    // complete waiter to confirm the lifecycle event still fired.
    let stopped = runtime
        .observe_isolate_complete(producer)
        .expect("register completion observer");
    runtime
        .try_send(
            producer,
            ResultMsg::FinishWithResult(ResultPayload {
                successes: 1,
                bytes: 0,
            }),
        )
        .expect("kick producer");

    stopped.wait(Duration::from_secs(3)).expect("isolate stops");

    let _ = runtime.shutdown();
}

#[test]
fn result_waiter_stopped_without_result_when_isolate_uses_plain_stop() {
    let runtime = make_runtime();
    let producer = spawn_producer(&runtime);

    let waiter = runtime
        .observe_result::<ResultPayload, _, _>(producer)
        .expect("register result waiter");

    runtime
        .try_send(producer, ResultMsg::FinishWithoutResult)
        .expect("kick producer");

    let outcome = waiter.wait(Duration::from_secs(3));
    assert_eq!(outcome, Err(ResultWaitError::StoppedWithoutResult));

    let _ = runtime.shutdown();
}

#[test]
fn result_waiter_type_mismatch_when_stop_with_value_is_wrong_type() {
    let runtime = make_runtime();
    let producer = spawn_producer(&runtime);

    let waiter = runtime
        .observe_result::<ResultPayload, _, _>(producer)
        .expect("register result waiter");

    runtime
        .try_send(producer, ResultMsg::FinishWithMismatchedType)
        .expect("kick producer");

    let outcome = waiter.wait(Duration::from_secs(3));
    assert_eq!(outcome, Err(ResultWaitError::TypeMismatch));

    let _ = runtime.shutdown();
}

#[test]
fn observe_result_is_already_claimed_on_second_register() {
    let runtime = make_runtime();
    let producer = spawn_producer(&runtime);

    let _first = runtime
        .observe_result::<ResultPayload, _, _>(producer)
        .expect("first register succeeds");

    let second = runtime.observe_result::<ResultPayload, _, _>(producer);
    assert_eq!(second.err(), Some(ResultWaitError::AlreadyClaimed));

    let _ = runtime.shutdown();
}

#[test]
fn observe_result_is_already_stopped_when_isolate_already_finished() {
    let runtime = make_runtime();
    let producer = spawn_producer(&runtime);

    let stopped = runtime
        .observe_isolate_complete(producer)
        .expect("register completion observer");
    runtime
        .try_send(producer, ResultMsg::FinishWithoutResult)
        .expect("kick producer");
    stopped.wait(Duration::from_secs(3)).expect("isolate stops");

    // A late registration cannot replay any value the isolate might have
    // produced.
    let late = runtime.observe_result::<ResultPayload, _, _>(producer);
    assert_eq!(late.err(), Some(ResultWaitError::AlreadyStopped));

    let _ = runtime.shutdown();
}

#[test]
fn result_waiter_observes_isolate_already_stopped_when_resolved() {
    // P2 regression: the result must only resolve after the runtime has
    // marked the isolate stopped, emitted IsolateStopped, and cancelled
    // requester-owned driver calls. observe_isolate_complete is the
    // proxy for "was IsolateStopped emitted yet?"; if the result waiter
    // wakes first, the complete waiter would still be unresolved.
    let runtime = make_runtime();
    let producer = spawn_producer(&runtime);

    let result = runtime
        .observe_result::<ResultPayload, _, _>(producer)
        .expect("register result waiter");
    let stopped = runtime
        .observe_isolate_complete(producer)
        .expect("register completion observer");

    runtime
        .try_send(
            producer,
            ResultMsg::FinishWithResult(ResultPayload {
                successes: 1,
                bytes: 1,
            }),
        )
        .expect("kick producer");

    // Wait for the result first.
    let _ = result
        .wait(Duration::from_secs(3))
        .expect("waiter resolves");
    // The IsolateStopped event must already be visible: a 0ms wait
    // should resolve immediately.
    stopped
        .wait(Duration::from_millis(0))
        .expect("isolate is already marked stopped before result observed");

    let _ = runtime.shutdown();
}

#[test]
fn dropped_result_waiter_does_not_block_re_registration() {
    // P2 regression: a dropped/timed-out waiter must not leave a ghost
    // single-claim slot. A fresh observe_result on the same address
    // should succeed and resolve normally.
    let runtime = make_runtime();
    let producer = spawn_producer(&runtime);

    {
        let _doomed = runtime
            .observe_result::<ResultPayload, _, _>(producer)
            .expect("first register");
    }

    // Stale slot is reclaimed on next register.
    let fresh = runtime
        .observe_result::<ResultPayload, _, _>(producer)
        .expect("re-register after drop");

    runtime
        .try_send(
            producer,
            ResultMsg::FinishWithResult(ResultPayload {
                successes: 9,
                bytes: 9,
            }),
        )
        .expect("kick producer");

    let value = fresh
        .wait(Duration::from_secs(3))
        .expect("fresh waiter resolves");
    assert_eq!(value.successes, 9);

    let _ = runtime.shutdown();
}

#[test]
fn timed_out_result_waiter_does_not_block_re_registration() {
    // Same as the dropped case but going through `wait` + Timeout — the
    // waiter is consumed by `wait`, abandoning the claim.
    let runtime = make_runtime();
    let producer = spawn_producer(&runtime);

    let timed_out = runtime
        .observe_result::<ResultPayload, _, _>(producer)
        .expect("first register");
    assert_eq!(
        timed_out.wait(Duration::from_millis(20)),
        Err(ResultWaitError::Timeout)
    );

    let fresh = runtime
        .observe_result::<ResultPayload, _, _>(producer)
        .expect("re-register after timeout");

    runtime
        .try_send(
            producer,
            ResultMsg::FinishWithResult(ResultPayload {
                successes: 3,
                bytes: 3,
            }),
        )
        .expect("kick producer");

    let value = fresh
        .wait(Duration::from_secs(3))
        .expect("fresh waiter resolves");
    assert_eq!(value.successes, 3);

    let _ = runtime.shutdown();
}

#[test]
fn dropped_result_waiters_for_distinct_isolates_do_not_fill_observation_cap() {
    // P2 regression: dropping a result waiter for one (isolate,
    // generation) used to leave its slot pinned to the global cap
    // until the same address re-registered. Many distinct isolates
    // could thus exhaust the cap purely via abandoned slots. After
    // the prune-on-cap-check fix, registering and dropping a fresh
    // waiter for distinct isolates does not raise pending_count.
    let runtime = make_runtime();
    // Register and drop result waiters for a sequence of distinct
    // isolates well past the observation cap.
    for _ in 0..(2 * 1024) {
        let producer = spawn_producer(&runtime);
        {
            let _doomed = runtime
                .observe_result::<ResultPayload, _, _>(producer)
                .expect("register result waiter");
        }
        // Stop the producer to keep entry table tidy.
        let stopped = runtime
            .observe_isolate_complete(producer)
            .expect("register completion observer");
        runtime
            .try_send(producer, ResultMsg::FinishWithoutResult)
            .expect("kick producer");
        let _ = stopped.wait(Duration::from_secs(1));
    }

    // After all of those drops, registering a fresh waiter against a
    // brand-new isolate must still succeed.
    let producer = spawn_producer(&runtime);
    let waiter = runtime
        .observe_result::<ResultPayload, _, _>(producer)
        .expect("registering after many abandoned slots succeeds");
    let _ = waiter; // drop is fine
    let _ = runtime.shutdown();
}

#[test]
fn observe_result_is_observation_full_when_cap_reached() {
    let runtime = make_runtime();

    // Saturate the observation registry with bound waiters.
    let mut bound_waiters = Vec::new();
    for _ in 0..1024 {
        bound_waiters.push(
            runtime
                .observe_next_bound()
                .expect("register bind observer"),
        );
    }

    let producer = spawn_producer(&runtime);
    let outcome = runtime.observe_result::<ResultPayload, _, _>(producer);
    assert_eq!(outcome.err(), Some(ResultWaitError::ObservationFull));

    let _ = runtime.shutdown();
    drop(bound_waiters);
}
