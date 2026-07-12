use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina::{Mailbox, TrySendError};
use tina_runtime::{
    DefaultThreadedMailboxFactory, HostBurstOutcomes, MailboxFactory, SendObservedUntilError,
    ThreadedMultiShardRuntime, ThreadedRuntimeConfig, ThreadedRuntimeError,
    ThreadedSendObservedError, ThreadedTrySendError,
};

#[derive(Debug, Clone, Copy)]
struct TestShard(u32);

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Gate {
    fn enter_and_wait(&self) {
        let mut state = self.state.lock().expect("gate lock");
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).expect("gate wait");
        }
    }

    fn wait_until_entered(&self) {
        let mut state = self.state.lock().expect("gate lock");
        while !state.0 {
            state = self.changed.wait(state).expect("gate wait");
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("gate lock");
        state.1 = true;
        self.changed.notify_all();
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum TestMsg {
    Count,
    Owned(DropProbe),
    Hold(Arc<Gate>),
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
    processed: Arc<AtomicU32>,
}

#[tina_runtime::isolate(message = TestMsg, shard = TestShard)]
impl TestIsolate {
    fn handle(&mut self, message: TestMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        match message {
            TestMsg::Count | TestMsg::Owned(_) => {
                self.processed.fetch_add(1, Ordering::Release);
                noop()
            }
            TestMsg::Hold(gate) => {
                gate.enter_and_wait();
                noop()
            }
            TestMsg::Stop => stop(),
        }
    }
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !predicate() {
        assert!(Instant::now() < deadline, "condition was not reached");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn runtime(
    command_capacity: usize,
) -> ThreadedMultiShardRuntime<TestShard, DefaultThreadedMailboxFactory> {
    ThreadedMultiShardRuntime::with_config(
        [TestShard(11), TestShard(22)],
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

#[test]
fn multi_observed_admission_routes_exact_outcomes_to_owning_shard() {
    let runtime = runtime(16);
    let processed = Arc::new(AtomicU32::new(0));
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::clone(&processed),
            },
            4,
        )
        .expect("register on second shard");

    let admitted = HostBurstOutcomes::new();
    runtime
        .try_send_outcome(address, TestMsg::Count, &admitted)
        .expect("accept observed command");
    admitted
        .wait_complete(Duration::from_secs(1))
        .expect("observe admitted message");
    assert_eq!(admitted.snapshot().admitted, 1);
    wait_until(|| processed.load(Ordering::Acquire) == 1);

    runtime
        .send_and_observe(address, TestMsg::Stop)
        .expect("admit stop");
    wait_until(|| {
        runtime.send_and_observe(address, TestMsg::Count)
            == Err(ThreadedSendObservedError::MailboxClosed)
    });
    let closed = HostBurstOutcomes::new();
    runtime
        .try_send_outcome(address, TestMsg::Count, &closed)
        .expect("worker accepts stale-target command");
    closed
        .wait_complete(Duration::from_secs(1))
        .expect("observe stale target");
    assert_eq!(closed.snapshot().mailbox_closed, 1);
    assert_eq!(
        runtime.send_observed_until(
            address,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            || TestMsg::Count,
        ),
        Err(SendObservedUntilError::Closed)
    );

    assert!(runtime.shutdown().is_ok());
}

#[test]
fn multi_ingress_full_settles_once_and_refills() {
    let runtime = runtime(1);
    let processed = Arc::new(AtomicU32::new(0));
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register on second shard");
    let gate = Arc::new(Gate::default());
    runtime
        .try_send(address, TestMsg::Hold(Arc::clone(&gate)))
        .expect("admit blocking handler");
    gate.wait_until_entered();
    runtime
        .try_send(address, TestMsg::Count)
        .expect("fill owning shard ingress");

    let drops = Arc::new(AtomicU32::new(0));
    let rejected = HostBurstOutcomes::new();
    assert_eq!(
        runtime.try_send_outcome(
            address,
            TestMsg::Owned(DropProbe(Arc::clone(&drops))),
            &rejected,
        ),
        Err(ThreadedTrySendError::IngressFull)
    );
    rejected
        .wait_complete(Duration::from_secs(1))
        .expect("host rejection settles");
    let snapshot = rejected.snapshot();
    assert_eq!(snapshot.submitted, 1);
    assert_eq!(snapshot.observed, 1);
    assert_eq!(snapshot.ingress_full, 1);
    assert_eq!(drops.load(Ordering::Acquire), 1);

    gate.release();
    wait_until(|| processed.load(Ordering::Acquire) == 1);
    let refilled = HostBurstOutcomes::new();
    runtime
        .try_send_outcome(
            address,
            TestMsg::Owned(DropProbe(Arc::clone(&drops))),
            &refilled,
        )
        .expect("refilled ingress accepts");
    refilled
        .wait_complete(Duration::from_secs(1))
        .expect("refilled outcome settles");
    assert_eq!(refilled.snapshot().admitted, 1);
    wait_until(|| processed.load(Ordering::Acquire) == 2);
    assert_eq!(drops.load(Ordering::Acquire), 2);

    assert!(runtime.shutdown().is_ok());
}

#[test]
fn multi_deadline_retry_rebuilds_messages_until_saturated_ingress_refills() {
    let runtime = runtime(1);
    let processed = Arc::new(AtomicU32::new(0));
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register on second shard");
    let gate = Arc::new(Gate::default());
    runtime
        .try_send(address, TestMsg::Hold(Arc::clone(&gate)))
        .expect("admit blocking handler");
    gate.wait_until_entered();
    runtime
        .try_send(address, TestMsg::Count)
        .expect("fill owning shard ingress");

    let release_gate = Arc::clone(&gate);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        release_gate.release();
    });
    let calls = Arc::new(AtomicU32::new(0));
    let drops = Arc::new(AtomicU32::new(0));
    runtime
        .send_observed_until(
            address,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(2),
            || {
                calls.fetch_add(1, Ordering::Release);
                TestMsg::Owned(DropProbe(Arc::clone(&drops)))
            },
        )
        .expect("refill admits rebuilt message");
    releaser.join().expect("releaser joins");
    assert!(calls.load(Ordering::Acquire) >= 2);
    wait_until(|| processed.load(Ordering::Acquire) == 2);
    wait_until(|| drops.load(Ordering::Acquire) == calls.load(Ordering::Acquire));

    assert!(runtime.shutdown().is_ok());
}

#[test]
fn multi_deadline_timeout_cancels_queued_delivery() {
    let runtime = runtime(2);
    let processed = Arc::new(AtomicU32::new(0));
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register on second shard");
    let gate = Arc::new(Gate::default());
    runtime
        .try_send(address, TestMsg::Hold(Arc::clone(&gate)))
        .expect("admit blocking handler");
    gate.wait_until_entered();

    let drops = Arc::new(AtomicU32::new(0));
    let calls = Arc::new(AtomicU32::new(0));
    assert_eq!(
        runtime.send_observed_until(
            address,
            Instant::now() + Duration::from_millis(50),
            Duration::from_millis(1),
            || {
                calls.fetch_add(1, Ordering::Release);
                TestMsg::Owned(DropProbe(Arc::clone(&drops)))
            },
        ),
        Err(SendObservedUntilError::Timeout)
    );
    assert_eq!(calls.load(Ordering::Acquire), 1);
    gate.release();
    wait_until(|| drops.load(Ordering::Acquire) == 1);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(processed.load(Ordering::Acquire), 0);
    assert_eq!(drops.load(Ordering::Acquire), 1);

    assert!(runtime.shutdown().is_ok());
}

#[test]
fn multi_message_factory_that_crosses_deadline_cannot_enqueue() {
    let runtime = runtime(8);
    let processed = Arc::new(AtomicU32::new(0));
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register on second shard");
    let drops = Arc::new(AtomicU32::new(0));

    assert_eq!(
        runtime.send_observed_until(
            address,
            Instant::now() + Duration::from_millis(10),
            Duration::from_millis(1),
            || {
                std::thread::sleep(Duration::from_millis(30));
                TestMsg::Owned(DropProbe(Arc::clone(&drops)))
            },
        ),
        Err(SendObservedUntilError::Timeout)
    );
    assert_eq!(drops.load(Ordering::Acquire), 1);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(processed.load(Ordering::Acquire), 0);
    assert_eq!(drops.load(Ordering::Acquire), 1);

    assert!(runtime.shutdown().is_ok());
}

#[derive(Clone)]
struct PanicFactory {
    gate: Arc<Gate>,
}

impl MailboxFactory for PanicFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        if capacity == 13 {
            self.gate.enter_and_wait();
            panic!("intentional worker failure");
        }
        DefaultThreadedMailboxFactory.create(capacity)
    }
}

#[test]
fn multi_accepted_observer_settles_when_owning_worker_fails() {
    let gate = Arc::new(Gate::default());
    let runtime = Arc::new(ThreadedMultiShardRuntime::with_config(
        [TestShard(11), TestShard(22)],
        PanicFactory {
            gate: Arc::clone(&gate),
        },
        ThreadedRuntimeConfig {
            command_capacity: 2,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let processed = Arc::new(AtomicU32::new(0));
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register before failure");

    let failing_runtime = Arc::clone(&runtime);
    let failure = std::thread::spawn(move || {
        failing_runtime.register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::new(AtomicU32::new(0)),
            },
            13,
        )
    });
    gate.wait_until_entered();
    let outcomes = HostBurstOutcomes::new();
    let drops = Arc::new(AtomicU32::new(0));
    runtime
        .try_send_outcome(
            address,
            TestMsg::Owned(DropProbe(Arc::clone(&drops))),
            &outcomes,
        )
        .expect("queue accepts observed command before failure");
    gate.release();
    assert!(matches!(
        failure.join().expect("failure thread joins"),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
    outcomes
        .wait_complete(Duration::from_secs(1))
        .expect("worker failure settles observer");
    let snapshot = outcomes.snapshot();
    assert_eq!(snapshot.submitted, 1);
    assert_eq!(snapshot.observed, 1);
    assert_eq!(snapshot.worker_stopped, 1);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(processed.load(Ordering::Acquire), 0);

    let runtime = Arc::try_unwrap(runtime).unwrap_or_else(|_| panic!("release runtime clones"));
    assert!(matches!(
        runtime.shutdown(),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
}

#[test]
fn multi_observed_admission_rejects_unknown_shard() {
    let owner = runtime(8);
    let foreign = ThreadedMultiShardRuntime::new([TestShard(99)], DefaultThreadedMailboxFactory);
    let address = foreign
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(99),
            TestIsolate {
                processed: Arc::new(AtomicU32::new(0)),
            },
            4,
        )
        .expect("register foreign isolate");
    let outcomes = HostBurstOutcomes::new();
    let rejected_drops = Arc::new(AtomicU32::new(0));
    let outcome_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        owner.try_send_outcome(
            address,
            TestMsg::Owned(DropProbe(Arc::clone(&rejected_drops))),
            &outcomes,
        )
    }));
    assert!(outcome_result.is_err());
    assert_eq!(rejected_drops.load(Ordering::Acquire), 1);
    assert_eq!(outcomes.snapshot().submitted, 0);
    assert_eq!(outcomes.snapshot().observed, 0);

    let drops = Arc::new(AtomicU32::new(0));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        owner.send_observed_until(
            address,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            || TestMsg::Owned(DropProbe(Arc::clone(&drops))),
        )
    }));
    assert!(result.is_err());
    assert_eq!(drops.load(Ordering::Acquire), 0, "factory must not run");

    assert!(foreign.shutdown().is_ok());
    assert!(owner.shutdown().is_ok());
}

struct NonDrainingMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
}

impl<T> Mailbox<T> for NonDrainingMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        let mut queue = self.queue.lock().expect("queue lock");
        if queue.len() == self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        None
    }

    fn is_empty(&self) -> bool {
        true
    }

    fn close(&self) {}
}

#[derive(Clone, Copy)]
struct NonDrainingFactory;

impl MailboxFactory for NonDrainingFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(NonDrainingMailbox {
            capacity,
            queue: Mutex::new(VecDeque::new()),
        })
    }
}

#[test]
fn multi_outcomes_keep_mailbox_full_distinct() {
    let runtime =
        ThreadedMultiShardRuntime::new([TestShard(11), TestShard(22)], NonDrainingFactory);
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::new(AtomicU32::new(0)),
            },
            1,
        )
        .expect("register on second shard");
    let outcomes = HostBurstOutcomes::new();
    runtime
        .try_send_outcome(address, TestMsg::Count, &outcomes)
        .expect("first observed command");
    runtime
        .try_send_outcome(address, TestMsg::Count, &outcomes)
        .expect("second observed command");
    outcomes
        .wait_complete(Duration::from_secs(1))
        .expect("both outcomes settle");
    let snapshot = outcomes.snapshot();
    assert_eq!(snapshot.admitted, 1);
    assert_eq!(snapshot.mailbox_full, 1);

    assert!(runtime.shutdown().is_ok());
}
