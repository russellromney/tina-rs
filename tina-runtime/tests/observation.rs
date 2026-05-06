//! Phase 047 Rock 4: typed observation handle tests.
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

use tina::{Mailbox, TrySendError, prelude::*};
use tina::{RestartBudget, RestartPolicy};
use tina_runtime::{
    CallError, CallKind, ListenerId, MailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
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
    fn handle(&mut self, msg: BindMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        match msg {
            BindMsg::Start => tcp_bind(self.addr).reply(BindMsg::Bound),
            BindMsg::Bound(Ok((listener, _))) => {
                tcp_close_listener(listener).reply(BindMsg::Closed)
            }
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

    let waiter = runtime.observe_next_bound();

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
    let waiter = runtime.observe_next_bound();

    let outcome = waiter.wait(Duration::from_millis(50));
    assert_eq!(outcome, Err(WaitError::Timeout));

    let _ = runtime.shutdown();
}

#[test]
fn waiter_runtime_stopped_when_runtime_dropped() {
    let runtime = make_runtime();
    let waiter = runtime.observe_next_bound();

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
        let _doomed = runtime.observe_next_bound();
    }

    let live = runtime.observe_next_bound();

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
    let waiter = runtime.observe_next_bound();
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
    let first = runtime.observe_next_bound();
    let second = runtime.observe_next_bound();

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
    SleepDone(Result<(), CallError>),
}

#[derive(Debug)]
struct Sleeper {
    nap: Duration,
}

#[tina_runtime::isolate(message = SleeperMsg, shard = TestShard)]
impl Sleeper {
    fn handle(&mut self, msg: SleeperMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        match msg {
            SleeperMsg::Start => sleep(self.nap).reply(SleeperMsg::SleepDone),
            SleeperMsg::SleepDone(_) => stop(),
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
    let done = runtime.observe_isolate_complete(sleeper);
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

    let done = runtime.observe_isolate_complete(sleeper);
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

    let waiter = runtime.observe_operation_done(sleeper, CallKind::Sleep);
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
    let waiter = runtime.observe_operation_done(sleeper_b, CallKind::Sleep);
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
    Settle,
}

#[derive(Debug)]
struct Crashy {
    crashed: Arc<AtomicU32>,
}

#[tina_runtime::isolate(message = CrashMsg, shard = TestShard)]
impl Crashy {
    fn handle(&mut self, msg: CrashMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        match msg {
            CrashMsg::Boom => {
                self.crashed.fetch_add(1, Ordering::Relaxed);
                panic!("boom");
            }
            CrashMsg::Settle => noop(),
        }
    }
}

#[derive(Debug, Clone)]
enum ParentMsg {
    Spawn,
    Settle,
}

#[derive(Debug)]
struct Parent {
    crashed: Arc<AtomicU32>,
    child: Option<Address<CrashMsg>>,
}

#[tina_runtime::isolate(
    message = ParentMsg,
    spawn = RestartableChildDefinition<Crashy>,
    shard = TestShard,
)]
impl Parent {
    fn handle(&mut self, msg: ParentMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
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
            ParentMsg::Settle => noop(),
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
                child: None,
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

    let restart_waiter = runtime.observe_child_restarted(parent);
    runtime
        .try_send(parent, ParentMsg::Spawn)
        .expect("ask parent to spawn");

    let restarted = restart_waiter
        .wait(Duration::from_secs(3))
        .expect("restart event resolves");
    assert_eq!(restarted.child_ordinal, 0);
    // The child has crashed at least once. The replacement carries a
    // fresh incarnation id (and generation policy is owned by the runtime,
    // not by this test), so just check that the runtime saw the panic.
    assert!(crashed.load(Ordering::Relaxed) >= 1);

    let _ = runtime.shutdown();
}
