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

#[derive(Debug)]
enum ServiceEvent {
    Count,
}

struct EventService {
    processed: Arc<AtomicU32>,
}

#[tina_runtime::isolate(event = ServiceEvent, shard = TestShard)]
impl EventService {
    fn handle_event(
        &mut self,
        _event: ServiceEvent,
        _ctx: &mut Context<'_, TestShard>,
    ) -> Effect<Self> {
        self.processed.fetch_add(1, Ordering::Release);
        noop()
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
fn multi_preflight_rejection_settles_once_without_mailbox_delivery() {
    let runtime = runtime(8);
    let processed = Arc::new(AtomicU32::new(0));
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::clone(&processed),
            },
            4,
        )
        .expect("register target");
    let preflight_calls = Arc::new(AtomicU32::new(0));
    let preflight_calls_for_send = Arc::clone(&preflight_calls);
    let drops = Arc::new(AtomicU32::new(0));
    let drops_for_observer = Arc::clone(&drops);
    let drops_seen_by_observer = Arc::new(AtomicU32::new(0));
    let drops_seen_by_observer_for_send = Arc::clone(&drops_seen_by_observer);
    let (observer_tx, observer_rx) = std::sync::mpsc::channel();

    runtime
        .try_send_and_observe_with_preflight(
            address,
            TestMsg::Owned(DropProbe(Arc::clone(&drops))),
            move |_| {
                preflight_calls_for_send.fetch_add(1, Ordering::Release);
                Some(ThreadedSendObservedError::MailboxClosed)
            },
            move |outcome| {
                drops_seen_by_observer_for_send.store(
                    drops_for_observer.load(Ordering::Acquire),
                    Ordering::Release,
                );
                observer_tx.send(outcome).expect("report observer outcome");
            },
        )
        .expect("owning worker accepts preflight command");

    assert_eq!(
        observer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("preflight settles observer"),
        Err(ThreadedSendObservedError::MailboxClosed)
    );
    assert!(observer_rx.recv_timeout(Duration::from_millis(20)).is_err());
    assert_eq!(preflight_calls.load(Ordering::Acquire), 1);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(
        drops_seen_by_observer.load(Ordering::Acquire),
        1,
        "rejected message ownership must settle before terminal observation"
    );
    assert_eq!(processed.load(Ordering::Acquire), 0);
    runtime
        .send_and_observe(address, TestMsg::Count)
        .expect("preflight rejection does not poison worker");
    wait_until(|| processed.load(Ordering::Acquire) == 1);

    assert!(runtime.shutdown().is_ok());
}

#[test]
fn multi_event_observation_helpers_preserve_capability_and_deadline_shapes() {
    let runtime = runtime(8);
    let processed = Arc::new(AtomicU32::new(0));
    let events = runtime
        .register_event_service_on::<EventService, ServiceEvent, Infallible>(
            ShardId::new(22),
            EventService {
                processed: Arc::clone(&processed),
            },
            4,
        )
        .expect("register event service");

    runtime
        .send_event_and_observe(events, ServiceEvent::Count)
        .expect("observe direct event admission");
    runtime
        .send_event_observed_until(
            events,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            || ServiceEvent::Count,
        )
        .expect("observe deadline event admission");
    wait_until(|| processed.load(Ordering::Acquire) == 2);

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
    let topology = runtime.topology();
    let owning = topology
        .shards()
        .iter()
        .find(|report| report.shard() == ShardId::new(22))
        .expect("owning shard report");
    assert_eq!(owning.ingress().rejected_full(), Some(1));

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

struct PanicOnSendMailbox<T> {
    capacity: usize,
    _message: std::marker::PhantomData<T>,
}

impl<T> Mailbox<T> for PanicOnSendMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, _message: T) -> Result<(), TrySendError<T>> {
        panic!("intentional mailbox admission failure");
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
struct SendPanicFactory;

impl MailboxFactory for SendPanicFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        if capacity == 13 {
            return Box::new(PanicOnSendMailbox {
                capacity,
                _message: std::marker::PhantomData,
            });
        }
        DefaultThreadedMailboxFactory.create(capacity)
    }
}

struct GatedSendMailbox<T> {
    inner: Box<dyn Mailbox<T>>,
    gate: Arc<Gate>,
}

impl<T> Mailbox<T> for GatedSendMailbox<T> {
    fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        self.gate.enter_and_wait();
        self.inner.try_send(message)
    }

    fn recv(&self) -> Option<T> {
        self.inner.recv()
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn close(&self) {
        self.inner.close();
    }
}

#[derive(Clone)]
struct GatedSendFactory {
    gate: Arc<Gate>,
}

impl MailboxFactory for GatedSendFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        let inner = DefaultThreadedMailboxFactory.create(capacity);
        if capacity == 13 {
            return Box::new(GatedSendMailbox {
                inner,
                gate: Arc::clone(&self.gate),
            });
        }
        inner
    }
}

#[test]
fn multi_preflight_panic_settles_once_and_stops_only_the_owning_worker() {
    let runtime = ThreadedMultiShardRuntime::new(
        [TestShard(11), TestShard(22)],
        DefaultThreadedMailboxFactory,
    );
    let processed = Arc::new(AtomicU32::new(0));
    let healthy_processed = Arc::new(AtomicU32::new(0));
    let healthy = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(11),
            TestIsolate {
                processed: Arc::clone(&healthy_processed),
            },
            4,
        )
        .expect("register healthy peer shard target");
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::clone(&processed),
            },
            4,
        )
        .expect("register target");
    let drops = Arc::new(AtomicU32::new(0));
    let (observer_tx, observer_rx) = std::sync::mpsc::channel();

    runtime
        .try_send_and_observe_with_preflight(
            address,
            TestMsg::Owned(DropProbe(Arc::clone(&drops))),
            |_| panic!("intentional multi preflight failure"),
            move |outcome| observer_tx.send(outcome).expect("report observer outcome"),
        )
        .expect("owning worker accepts preflight command");

    assert_eq!(
        observer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("unwind settles observer"),
        Err(ThreadedSendObservedError::WorkerStopped)
    );
    assert!(observer_rx.recv_timeout(Duration::from_millis(20)).is_err());
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(processed.load(Ordering::Acquire), 0);
    runtime
        .send_and_observe(healthy, TestMsg::Count)
        .expect("peer shard survives owning-worker preflight panic");
    wait_until(|| healthy_processed.load(Ordering::Acquire) == 1);
    assert!(matches!(
        runtime.shutdown(),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
}

#[test]
fn multi_mailbox_panic_settles_once_and_stops_only_the_owning_worker() {
    let runtime = ThreadedMultiShardRuntime::new([TestShard(11), TestShard(22)], SendPanicFactory);
    let processed = Arc::new(AtomicU32::new(0));
    let healthy_processed = Arc::new(AtomicU32::new(0));
    let healthy = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(11),
            TestIsolate {
                processed: Arc::clone(&healthy_processed),
            },
            4,
        )
        .expect("register healthy peer shard target");
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::clone(&processed),
            },
            13,
        )
        .expect("register panic-on-send target");
    let drops = Arc::new(AtomicU32::new(0));
    let (observer_tx, observer_rx) = std::sync::mpsc::channel();

    runtime
        .try_send_and_observe_with(
            address,
            TestMsg::Owned(DropProbe(Arc::clone(&drops))),
            move |outcome| observer_tx.send(outcome).expect("report observer outcome"),
        )
        .expect("owning worker accepts mailbox command");

    assert_eq!(
        observer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("unwind settles observer"),
        Err(ThreadedSendObservedError::WorkerStopped)
    );
    assert!(observer_rx.recv_timeout(Duration::from_millis(20)).is_err());
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(processed.load(Ordering::Acquire), 0);
    runtime
        .send_and_observe(healthy, TestMsg::Count)
        .expect("peer shard survives owning-worker mailbox panic");
    wait_until(|| healthy_processed.load(Ordering::Acquire) == 1);
    assert!(matches!(
        runtime.shutdown(),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
}

#[test]
fn multi_deadline_claim_reports_worker_stopped_when_mailbox_admission_panics() {
    let runtime = ThreadedMultiShardRuntime::new([TestShard(11), TestShard(22)], SendPanicFactory);
    let processed = Arc::new(AtomicU32::new(0));
    let healthy_processed = Arc::new(AtomicU32::new(0));
    let healthy = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(11),
            TestIsolate {
                processed: Arc::clone(&healthy_processed),
            },
            4,
        )
        .expect("register healthy peer shard target");
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::clone(&processed),
            },
            13,
        )
        .expect("register panic-on-send target");
    let drops = Arc::new(AtomicU32::new(0));

    assert_eq!(
        runtime.send_observed_until(
            address,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            || TestMsg::Owned(DropProbe(Arc::clone(&drops))),
        ),
        Err(SendObservedUntilError::WorkerStopped)
    );
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(processed.load(Ordering::Acquire), 0);
    runtime
        .send_and_observe(healthy, TestMsg::Count)
        .expect("peer shard survives deadline mailbox panic");
    wait_until(|| healthy_processed.load(Ordering::Acquire) == 1);
    assert!(matches!(
        runtime.shutdown(),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
}

#[test]
fn multi_worker_claim_wins_deadline_race_on_the_owning_shard() {
    let gate = Arc::new(Gate::default());
    let runtime = Arc::new(ThreadedMultiShardRuntime::new(
        [TestShard(11), TestShard(22)],
        GatedSendFactory {
            gate: Arc::clone(&gate),
        },
    ));
    let processed = Arc::new(AtomicU32::new(0));
    let address = runtime
        .register_with_capacity_on::<TestIsolate, Infallible>(
            ShardId::new(22),
            TestIsolate {
                processed: Arc::clone(&processed),
            },
            13,
        )
        .expect("register gated target");
    let runtime_for_send = Arc::clone(&runtime);
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let send = std::thread::spawn(move || {
        let result = runtime_for_send.send_observed_until(
            address,
            Instant::now() + Duration::from_millis(30),
            Duration::from_millis(1),
            || TestMsg::Count,
        );
        result_tx.send(result).expect("report deadline outcome");
    });

    gate.wait_until_entered();
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        result_rx.try_recv().is_err(),
        "host cannot cancel after the owning worker claims mailbox authority"
    );
    gate.release();
    assert_eq!(
        result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("claimed admission settles"),
        Ok(())
    );
    send.join().expect("deadline sender joins");
    wait_until(|| processed.load(Ordering::Acquire) == 1);

    Arc::try_unwrap(runtime)
        .unwrap_or_else(|_| panic!("sole runtime owner"))
        .shutdown()
        .expect("clean multi shutdown");
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
    assert_eq!(
        owner.try_send_outcome(
            address,
            TestMsg::Owned(DropProbe(Arc::clone(&rejected_drops))),
            &outcomes,
        ),
        Err(ThreadedTrySendError::UnknownShard(ShardId::new(99)))
    );
    assert_eq!(rejected_drops.load(Ordering::Acquire), 1);
    assert_eq!(outcomes.snapshot().submitted, 0);
    assert_eq!(outcomes.snapshot().observed, 0);

    let drops = Arc::new(AtomicU32::new(0));
    assert_eq!(
        owner.send_observed_until(
            address,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            || TestMsg::Owned(DropProbe(Arc::clone(&drops))),
        ),
        Err(SendObservedUntilError::UnknownShard(ShardId::new(99)))
    );
    assert_eq!(drops.load(Ordering::Acquire), 0, "factory must not run");

    let observer_calls = Arc::new(AtomicU32::new(0));
    let observer_calls_for_send = Arc::clone(&observer_calls);
    let observed_drops = Arc::new(AtomicU32::new(0));
    assert_eq!(
        owner.try_send_and_observe_with(
            address,
            TestMsg::Owned(DropProbe(Arc::clone(&observed_drops))),
            move |_| {
                observer_calls_for_send.fetch_add(1, Ordering::Release);
            },
        ),
        Err(ThreadedTrySendError::UnknownShard(ShardId::new(99)))
    );
    assert_eq!(observed_drops.load(Ordering::Acquire), 1);
    assert_eq!(observer_calls.load(Ordering::Acquire), 0);

    let synchronous_drops = Arc::new(AtomicU32::new(0));
    assert_eq!(
        owner.send_and_observe(
            address,
            TestMsg::Owned(DropProbe(Arc::clone(&synchronous_drops))),
        ),
        Err(ThreadedSendObservedError::UnknownShard(ShardId::new(99)))
    );
    assert_eq!(synchronous_drops.load(Ordering::Acquire), 1);

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
