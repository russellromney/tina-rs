use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina::{Mailbox, TrySendError};
use tina_runtime::{
    DefaultThreadedMailboxFactory, HostBurstOutcomes, LocalSystem, LocalSystemConfig,
    MailboxFactory, SendObservedUntilError, ThreadedSendObservedError, ThreadedTrySendError,
};

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

#[derive(Debug)]
enum TestMsg {
    Count,
    Hold(Arc<(Mutex<bool>, Condvar)>, Arc<(Mutex<bool>, Condvar)>),
    Owned(DropProbe),
    Stop,
}

#[derive(Debug)]
struct DropProbe(Arc<AtomicU32>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

struct TestIsolate {
    counted: Arc<AtomicU32>,
}

#[tina_runtime::isolate(message = TestMsg, shard = TestShard)]
impl TestIsolate {
    fn handle(&mut self, message: TestMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        match message {
            TestMsg::Count => {
                self.counted.fetch_add(1, Ordering::Release);
                noop()
            }
            TestMsg::Hold(entered, release) => {
                let (entered_lock, entered_cv) = &*entered;
                *entered_lock.lock().expect("entered lock") = true;
                entered_cv.notify_all();

                let (release_lock, release_cv) = &*release;
                let mut released = release_lock.lock().expect("release lock");
                while !*released {
                    released = release_cv.wait(released).expect("release wait");
                }
                noop()
            }
            TestMsg::Stop => stop(),
            TestMsg::Owned(_probe) => noop(),
        }
    }
}

struct NonDrainingMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
    draining: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl<T> Mailbox<T> for NonDrainingMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed lock") {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.lock().expect("queue lock");
        if queue.len() == self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.draining
            .as_ref()
            .filter(|draining| draining.load(Ordering::Acquire))
            .and_then(|_| self.queue.lock().expect("queue lock").pop_front())
    }

    fn is_empty(&self) -> bool {
        self.draining
            .as_ref()
            .is_none_or(|draining| !draining.load(Ordering::Acquire))
            || self.queue.lock().expect("queue lock").is_empty()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed lock") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct NonDrainingMailboxFactory;

impl MailboxFactory for NonDrainingMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(NonDrainingMailbox {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
            draining: None,
        })
    }
}

#[derive(Debug, Clone)]
struct ControlledMailboxFactory {
    draining: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone, Copy)]
struct CapacityPanicMailboxFactory;

impl MailboxFactory for CapacityPanicMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        assert_ne!(capacity, 13, "intentional worker failure");
        DefaultThreadedMailboxFactory.create(capacity)
    }
}

impl MailboxFactory for ControlledMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(NonDrainingMailbox {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
            draining: Some(Arc::clone(&self.draining)),
        })
    }
}

fn app(ingress_capacity: usize) -> LocalSystem<TestShard, DefaultThreadedMailboxFactory> {
    LocalSystem::single_shard(TestShard, DefaultThreadedMailboxFactory)
        .config(LocalSystemConfig {
            ingress_capacity,
            idle_wait: Duration::from_millis(1),
            ..LocalSystemConfig::default()
        })
        .try_build()
        .expect("start local system")
}

fn wait_flag(flag: &Arc<(Mutex<bool>, Condvar)>) {
    let (lock, cv) = &**flag;
    let ready = lock.lock().expect("flag lock");
    let (ready, timeout) = cv
        .wait_timeout_while(ready, Duration::from_secs(2), |ready| !*ready)
        .expect("flag wait");
    assert!(*ready && !timeout.timed_out(), "flag was not set");
}

fn release_flag(flag: &Arc<(Mutex<bool>, Condvar)>) {
    let (lock, cv) = &**flag;
    *lock.lock().expect("flag lock") = true;
    cv.notify_all();
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !predicate() {
        assert!(Instant::now() < deadline, "condition was not reached");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn local_system_try_send_outcome_records_admitted_and_closed() {
    let app = app(16);
    let counted = Arc::new(AtomicU32::new(0));
    let address = app
        .register_root::<TestIsolate, Infallible>(
            TestIsolate {
                counted: Arc::clone(&counted),
            },
            4,
        )
        .expect("register root");

    let admitted = HostBurstOutcomes::new();
    app.try_send_outcome(address, TestMsg::Count, &admitted)
        .expect("admit outcome probe");
    admitted
        .wait_complete(Duration::from_secs(2))
        .expect("observe admitted send");
    assert_eq!(admitted.snapshot().admitted, 1);
    wait_until(|| counted.load(Ordering::Acquire) == 1);

    app.send_and_observe(address, TestMsg::Stop)
        .expect("admit stop");
    wait_until(|| {
        app.send_and_observe(address, TestMsg::Count)
            == Err(ThreadedSendObservedError::MailboxClosed)
    });

    let closed = HostBurstOutcomes::new();
    app.try_send_outcome(address, TestMsg::Count, &closed)
        .expect("host ingress accepts closed-target probe");
    closed
        .wait_complete(Duration::from_secs(2))
        .expect("observe closed send");
    assert_eq!(closed.snapshot().mailbox_closed, 1);

    app.shutdown()
        .drain()
        .join()
        .expect("shutdown local system");
}

#[test]
fn local_system_try_send_outcome_records_mailbox_full() {
    let app = LocalSystem::single_shard(TestShard, NonDrainingMailboxFactory)
        .try_build()
        .expect("start local system");
    let address = app
        .register_root::<TestIsolate, Infallible>(
            TestIsolate {
                counted: Arc::new(AtomicU32::new(0)),
            },
            1,
        )
        .expect("register root");
    let outcomes = HostBurstOutcomes::new();

    app.try_send_outcome(address, TestMsg::Count, &outcomes)
        .expect("first command handoff");
    app.try_send_outcome(address, TestMsg::Count, &outcomes)
        .expect("second command handoff");
    outcomes
        .wait_complete(Duration::from_secs(2))
        .expect("observe both sends");
    let snapshot = outcomes.snapshot();
    assert_eq!(snapshot.admitted, 1);
    assert_eq!(snapshot.mailbox_full, 1);

    app.shutdown()
        .drain()
        .join()
        .expect("shutdown local system");
}

#[test]
fn local_system_try_send_outcome_records_ingress_full_and_consumes_message() {
    let app = app(1);
    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let address = app
        .register_root::<TestIsolate, Infallible>(
            TestIsolate {
                counted: Arc::new(AtomicU32::new(0)),
            },
            4,
        )
        .expect("register root");

    app.try_send(
        address,
        TestMsg::Hold(Arc::clone(&entered), Arc::clone(&release)),
    )
    .expect("enter blocking handler");
    wait_flag(&entered);
    app.try_send(address, TestMsg::Count)
        .expect("fill ingress queue");

    let outcomes = HostBurstOutcomes::new();
    let drops = Arc::new(AtomicU32::new(0));
    assert_eq!(
        app.try_send_outcome(
            address,
            TestMsg::Owned(DropProbe(Arc::clone(&drops))),
            &outcomes,
        ),
        Err(ThreadedTrySendError::IngressFull)
    );
    assert_eq!(drops.load(Ordering::Acquire), 1);
    outcomes
        .wait_complete(Duration::from_secs(2))
        .expect("host rejection settles immediately");
    let snapshot = outcomes.snapshot();
    assert_eq!(snapshot.submitted, 1);
    assert_eq!(snapshot.observed, 1);
    assert_eq!(snapshot.ingress_full, 1);

    release_flag(&release);
    app.shutdown()
        .drain()
        .join()
        .expect("shutdown local system");
}

#[test]
fn local_system_send_observed_until_refills_and_rebuilds_owned_message() {
    let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app = LocalSystem::single_shard(
        TestShard,
        ControlledMailboxFactory {
            draining: Arc::clone(&draining),
        },
    )
    .try_build()
    .expect("start local system");
    let counted = Arc::new(AtomicU32::new(0));
    let address = app
        .register_root::<TestIsolate, Infallible>(
            TestIsolate {
                counted: Arc::clone(&counted),
            },
            1,
        )
        .expect("register root");

    app.send_and_observe(address, TestMsg::Count)
        .expect("fill mailbox");

    let factory_calls = Arc::new(AtomicU32::new(0));
    thread::scope(|scope| {
        let sender_calls = Arc::clone(&factory_calls);
        let app = &app;
        let sender = scope.spawn(move || {
            app.send_observed_until(
                address,
                Instant::now() + Duration::from_secs(2),
                Duration::from_millis(2),
                || {
                    sender_calls.fetch_add(1, Ordering::Relaxed);
                    TestMsg::Count
                },
            )
        });
        wait_until(|| factory_calls.load(Ordering::Acquire) >= 1);
        draining.store(true, Ordering::Release);
        sender
            .join()
            .expect("sender thread")
            .expect("refill admits");
    });

    assert!(factory_calls.load(Ordering::Acquire) >= 2);
    wait_until(|| counted.load(Ordering::Acquire) == 2);
    app.shutdown()
        .drain()
        .join()
        .expect("shutdown local system");
}

#[test]
fn local_system_elapsed_deadline_never_builds_or_delivers_message() {
    let app = app(16);
    let counted = Arc::new(AtomicU32::new(0));
    let factory_calls = Arc::new(AtomicU32::new(0));
    let address = app
        .register_root::<TestIsolate, Infallible>(
            TestIsolate {
                counted: Arc::clone(&counted),
            },
            4,
        )
        .expect("register root");

    let result = app.send_observed_until(
        address,
        Instant::now() - Duration::from_millis(1),
        Duration::from_millis(1),
        || {
            factory_calls.fetch_add(1, Ordering::Relaxed);
            TestMsg::Count
        },
    );
    assert_eq!(result, Err(SendObservedUntilError::Timeout));
    thread::sleep(Duration::from_millis(20));
    assert_eq!(factory_calls.load(Ordering::Acquire), 0);
    assert_eq!(counted.load(Ordering::Acquire), 0);

    app.shutdown()
        .drain()
        .join()
        .expect("shutdown local system");
}

#[test]
fn local_system_observed_admission_reports_worker_stopped() {
    let app = LocalSystem::single_shard(TestShard, CapacityPanicMailboxFactory)
        .try_build()
        .expect("start local system");
    let address = app
        .register_root::<TestIsolate, Infallible>(
            TestIsolate {
                counted: Arc::new(AtomicU32::new(0)),
            },
            4,
        )
        .expect("register root");
    assert!(
        app.register_root::<TestIsolate, Infallible>(
            TestIsolate {
                counted: Arc::new(AtomicU32::new(0)),
            },
            13,
        )
        .is_err()
    );
    wait_until(|| {
        app.try_send(address, TestMsg::Count) == Err(ThreadedTrySendError::WorkerStopped)
    });

    let outcomes = HostBurstOutcomes::new();
    assert_eq!(
        app.try_send_outcome(address, TestMsg::Count, &outcomes),
        Err(ThreadedTrySendError::WorkerStopped)
    );
    outcomes
        .wait_complete(Duration::from_secs(2))
        .expect("worker-stop rejection settles immediately");
    assert_eq!(outcomes.snapshot().worker_stopped, 1);
    assert_eq!(
        app.send_observed_until(
            address,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            || TestMsg::Count,
        ),
        Err(SendObservedUntilError::WorkerStopped)
    );

    assert!(matches!(
        app.shutdown().drain().join(),
        Err(tina_runtime::ThreadedRuntimeError::WorkerStopped)
    ));
}
