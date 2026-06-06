#![feature(allocator_api)]
//! Proofs for the readiness-driven worker park: the worker blocks on real
//! readiness instead of polling on a timer.
//!
//! - a fully idle worker makes ~0 park wakeups over a window (it blocks until a
//!   real wake source fires, rather than waking every `idle_wait`);
//! - a host command still wakes that blocked worker promptly;
//! - a threaded runtime over the *simulated* backend does not spin and still
//!   wakes for host commands (the simulated park sleeps on a condvar doorbell).
//!
//! The HTTP/socket readiness park is exercised end-to-end by the `tina-http`
//! suite (hundreds of real-socket tests over this worker); these tests pin the
//! wake *policy* with the park-wakeup counter.

use std::alloc::Global;
use std::any::Any;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use betelgeuse::io::simulated::SimulatedIO;
use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, MailboxFactory, ThreadedRuntime,
    ThreadedRuntimeConfig,
};

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

#[derive(Debug)]
enum EchoMsg {
    Ping,
    Notify,
}

struct Echo {
    notify: Option<mpsc::Sender<()>>,
}

#[tina_runtime::isolate(message = EchoMsg, reply = u32, shard = TestShard)]
impl Echo {
    fn handle(&mut self, msg: EchoMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        match msg {
            EchoMsg::Ping => reply(1),
            EchoMsg::Notify => {
                if let Some(tx) = self.notify.take() {
                    let _ = tx.send(());
                }
                noop()
            }
        }
    }

    fn handle_call(&mut self, msg: EchoMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            EchoMsg::Ping => call.reply(1),
            EchoMsg::Notify => call.reply(2),
        }
    }
}

const CAP: usize = 64;

type ErasedAnyMailbox = CapturingMailbox<Box<dyn Any>>;
type CapturedAnyMailbox = Arc<Mutex<Option<ErasedAnyMailbox>>>;

#[derive(Clone, Default)]
struct CapturingMailboxFactory {
    last_any: CapturedAnyMailbox,
}

impl CapturingMailboxFactory {
    fn last_any(&self) -> CapturingMailbox<Box<dyn Any>> {
        self.last_any
            .lock()
            .expect("captured mailbox mutex")
            .clone()
            .expect("runtime created an erased mailbox")
    }
}

impl MailboxFactory for CapturingMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        let mailbox = CapturingMailbox::new(capacity);
        if std::any::TypeId::of::<T>() == std::any::TypeId::of::<Box<dyn Any>>() {
            // Test-only: ThreadedRuntime stores isolate mailboxes erased as
            // `Box<dyn Any>`. Keep a clone of the latest erased mailbox so the
            // test can exercise the direct mailbox ingress seam.
            let erased_inner: Arc<Mutex<CapturingMailboxInner<Box<dyn Any>>>> =
                unsafe { std::mem::transmute(Arc::clone(&mailbox.inner)) };
            let erased = CapturingMailbox {
                capacity: mailbox.capacity,
                inner: erased_inner,
            };
            *self.last_any.lock().expect("captured mailbox mutex") = Some(erased);
        }
        Box::new(mailbox)
    }
}

struct CapturingMailbox<T> {
    capacity: usize,
    inner: Arc<Mutex<CapturingMailboxInner<T>>>,
}

struct CapturingMailboxInner<T> {
    queue: VecDeque<T>,
    closed: bool,
    wake: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
}

impl<T> Clone for CapturingMailbox<T> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            inner: Arc::clone(&self.inner),
        }
    }
}

// Test-only mailbox: the runtime erases messages to `Box<dyn Any>`, and this
// test only sends `EchoMsg` values through that queue. The unsafe impl keeps the
// captured handle movable between the worker-creating factory and the host test.
unsafe impl<T> Send for CapturingMailbox<T> {}
unsafe impl<T> Sync for CapturingMailbox<T> {}

impl<T> CapturingMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Arc::new(Mutex::new(CapturingMailboxInner {
                queue: VecDeque::with_capacity(capacity),
                closed: false,
                wake: None,
            })),
        }
    }
}

impl<T: 'static> Mailbox<T> for CapturingMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        let mut inner = self.inner.lock().expect("capturing mailbox mutex");
        if inner.closed {
            return Err(TrySendError::Closed(message));
        }
        if inner.queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        let was_empty = inner.queue.is_empty();
        inner.queue.push_back(message);
        let wake = was_empty.then(|| inner.wake.clone()).flatten();
        drop(inner);
        if let Some(wake) = wake {
            wake();
        }
        Ok(())
    }

    fn set_wake_hook(&self, wake: Option<Arc<dyn Fn() + Send + Sync + 'static>>) {
        self.inner.lock().expect("capturing mailbox mutex").wake = wake;
    }

    fn recv(&self) -> Option<T> {
        self.inner
            .lock()
            .expect("capturing mailbox mutex")
            .queue
            .pop_front()
    }

    fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .expect("capturing mailbox mutex")
            .queue
            .is_empty()
    }

    fn close(&self) {
        self.inner.lock().expect("capturing mailbox mutex").closed = true;
    }
}

/// A fully idle worker blocks on the kernel and makes ~0 wakeups over a window.
///
/// With the old timer park the worker woke every `idle_wait` (~1ms), so a 250ms
/// idle window would show ~250 wakeups. The readiness park blocks until a real
/// wake source fires, so an idle worker with no timers, no I/O, and no commands
/// stays flat.
#[test]
fn idle_worker_makes_near_zero_wakeups() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::new(TestShard, DefaultThreadedMailboxFactory);
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo { notify: None }, CAP)
        .expect("register echo");

    // Warm: one call so the worker has parked at least once after real work.
    assert_eq!(
        runtime
            .call_blocking(echo, EchoMsg::Ping, Duration::from_secs(1))
            .expect("call"),
        CallOutcome::Replied(1)
    );

    // Let it settle into a quiet park, then sample across an idle window.
    std::thread::sleep(Duration::from_millis(20));
    let before = runtime.park_wakeups();
    std::thread::sleep(Duration::from_millis(250));
    let delta = runtime.park_wakeups() - before;

    assert!(
        delta <= 2,
        "idle worker woke {delta} times over 250ms; a timer park would wake ~250 times"
    );

    runtime.shutdown().expect("shutdown");
}

/// The blocked idle worker still wakes promptly for a host command.
#[test]
fn idle_blocked_worker_wakes_for_command() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::new(TestShard, DefaultThreadedMailboxFactory);
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo { notify: None }, CAP)
        .expect("register echo");

    // Reach a quiet, block-forever park.
    std::thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    let outcome = runtime
        .call_blocking(echo, EchoMsg::Ping, Duration::from_secs(1))
        .expect("call");
    let elapsed = started.elapsed();

    assert_eq!(outcome, CallOutcome::Replied(1));
    assert!(
        elapsed < Duration::from_millis(200),
        "blocked worker did not wake promptly for a command: {elapsed:?}"
    );

    runtime.shutdown().expect("shutdown");
}

/// A pre-wake race: a command admitted right as the worker decides to park must
/// not sleep forever. Hammer commands at a freshly-settling worker many times;
/// every one must be observed under a tight bound (the doorbell is coalescing,
/// so a wake landing just before the park is still seen).
#[test]
fn command_admitted_around_park_is_never_missed() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::new(TestShard, DefaultThreadedMailboxFactory);
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo { notify: None }, CAP)
        .expect("register echo");

    for i in 0..200 {
        // A short idle gap most iterations lands the call right around the park
        // boundary; the worker must still wake and reply.
        if i % 2 == 0 {
            std::thread::sleep(Duration::from_micros(50));
        }
        let outcome = runtime
            .call_blocking(echo, EchoMsg::Ping, Duration::from_secs(2))
            .expect("call");
        assert_eq!(outcome, CallOutcome::Replied(1), "missed wake on iter {i}");
    }

    runtime.shutdown().expect("shutdown");
}

/// A direct mailbox push (outside `ThreadedRuntime::try_send`) must wake an
/// idle worker too. This is the seam custom mailbox factories expose: the
/// mailbox, not only the runtime command queue, owns empty -> non-empty
/// readiness truth.
#[test]
fn direct_mailbox_push_wakes_idle_worker() {
    let factory = CapturingMailboxFactory::default();
    let runtime: ThreadedRuntime<TestShard, CapturingMailboxFactory> =
        ThreadedRuntime::new(TestShard, factory.clone());
    let (tx, rx) = mpsc::channel();
    let _echo = runtime
        .register_with_capacity::<_, Infallible>(Echo { notify: Some(tx) }, CAP)
        .expect("register echo");
    let mailbox = factory.last_any();

    std::thread::sleep(Duration::from_millis(50));
    mailbox
        .try_send(Box::new(EchoMsg::Notify) as Box<dyn Any>)
        .expect("direct mailbox send accepted");

    rx.recv_timeout(Duration::from_secs(1))
        .expect("direct mailbox push woke worker and delivered message");

    runtime.shutdown().expect("shutdown");
}

/// A threaded runtime over the *simulated* backend must not spin and must still
/// wake for host commands. The simulated park sleeps on a condvar doorbell
/// (bounded cap), so an idle worker stays low-wakeup and a call still completes.
///
/// `tina-sim` deterministic replay is unaffected: it drives the simulated
/// backend with explicit `step()` calls and never uses this live blocking park.
#[test]
fn simulated_threaded_backend_no_spin_and_command_wake() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::with_config_and_io_loop_factory(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig::default(),
            || SimulatedIO::new().loop_handle(Global),
        );
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo { notify: None }, CAP)
        .expect("register echo");

    // Commands wake the simulated park.
    for _ in 0..10 {
        assert_eq!(
            runtime
                .call_blocking(echo, EchoMsg::Ping, Duration::from_secs(1))
                .expect("call"),
            CallOutcome::Replied(1)
        );
    }

    // No spin: the simulated park sleeps (its cap is ~1ms), so an idle window
    // shows far fewer wakeups than a busy-spin (which would be thousands).
    std::thread::sleep(Duration::from_millis(20));
    let before = runtime.park_wakeups();
    std::thread::sleep(Duration::from_millis(200));
    let delta = runtime.park_wakeups() - before;
    assert!(
        delta < 1000,
        "simulated worker appears to spin: {delta} wakeups in 200ms"
    );

    runtime.shutdown().expect("shutdown");
}
