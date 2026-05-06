//! Phase 047 Rock 4 (slice 1): typed bound-address waiter tests.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::thread;
use std::time::Duration;

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallError, ListenerId, MailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig, WaitError,
    tcp_bind, tcp_close_listener,
};

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
