//! `try_send_outcome` + `HostBurstOutcomes`.
//!
//! Proves the burst accumulator counts each typed outcome distinctly:
//! `admitted`, `mailbox_full`, `mailbox_closed`, `ingress_full`, and
//! `worker_stopped`. The accumulator wraps the existing
//! `try_send_and_observe_with` shape; nothing about that contract
//! changes.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina::{Mailbox, TrySendError};
use tina_runtime::{
    DefaultThreadedMailboxFactory, HostBurstOutcomes, HostBurstWaitError, MailboxFactory,
    SendObservedUntilError, ThreadedRuntime, ThreadedRuntimeConfig, ThreadedRuntimeError,
    ThreadedTrySendError,
};

/// A mailbox that accepts up to `capacity` and then never yields anything: it
/// reports empty (so the worker never schedules the isolate) and `recv` is a
/// no-op. Once filled it stays full forever, making mailbox overflow provable
/// and deterministic instead of racing the worker's drain.
struct NonDrainingMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> Mailbox<T> for NonDrainingMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }
    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed") {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.lock().expect("queue");
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }
    fn recv(&self) -> Option<T> {
        None
    }
    fn is_empty(&self) -> bool {
        // Reports empty so the worker never steps this isolate: the mailbox
        // provably fills and never drains.
        true
    }
    fn close(&self) {
        *self.closed.lock().expect("closed") = true;
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
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct CapacityPanicMailboxFactory;

impl MailboxFactory for CapacityPanicMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        assert_ne!(capacity, 13, "intentional worker failure");
        DefaultThreadedMailboxFactory.create(capacity)
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

#[derive(Debug, Clone, Copy)]
struct SendPanicMailboxFactory;

impl MailboxFactory for SendPanicMailboxFactory {
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
    gate: Arc<FailureGate>,
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

#[derive(Debug, Clone)]
struct GatedSendFactory {
    gate: Arc<FailureGate>,
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

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum SlowMsg {
    Job(u32),
    Owned(DropProbe),
    Hold(Arc<FailureGate>),
    /// Stop the isolate cleanly so its mailbox closes.
    Stop,
}

#[derive(Debug)]
struct DropProbe(Arc<AtomicU32>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

struct Slow {
    processed: Arc<AtomicU32>,
}

#[tina_runtime::isolate(message = SlowMsg, shard = TestShard)]
impl Slow {
    fn handle(&mut self, msg: SlowMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        match msg {
            SlowMsg::Job(_) => {
                self.processed.fetch_add(1, Ordering::Release);
                noop()
            }
            SlowMsg::Owned(_probe) => {
                self.processed.fetch_add(1, Ordering::Release);
                noop()
            }
            SlowMsg::Hold(gate) => {
                gate.enter_and_wait();
                noop()
            }
            SlowMsg::Stop => stop(),
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

fn make_runtime() -> ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 256,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

#[test]
fn try_send_outcome_admits_full_burst_when_mailbox_has_room() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            32,
        )
        .expect("register slow");

    let outcomes = HostBurstOutcomes::new();
    for n in 0..16 {
        runtime
            .try_send_outcome(worker, SlowMsg::Job(n), &outcomes)
            .expect("burst admits");
    }
    outcomes
        .wait_complete(Duration::from_secs(2))
        .expect("observers fire");

    let snap = outcomes.snapshot();
    assert_eq!(snap.submitted, 16);
    assert_eq!(snap.observed, 16);
    assert_eq!(snap.admitted, 16);
    assert_eq!(snap.mailbox_full, 0);
    assert_eq!(snap.mailbox_closed, 0);
    assert_eq!(snap.ingress_full, 0);
    assert_eq!(snap.worker_stopped, 0);

    let _ = runtime.shutdown();
}

#[test]
fn try_send_outcome_overflow_marks_mailbox_full() {
    // Deterministic overflow: the target's mailbox never drains (see
    // `NonDrainingMailbox`), so a burst past its capacity provably overflows.
    // No wall-clock race against the worker's drain — the counts below are
    // fixed by capacity, not timing.
    let capacity = 4usize;
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        NonDrainingMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 256,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            capacity,
        )
        .expect("register slow");

    let outcomes = HostBurstOutcomes::new();
    let burst = 32u32;
    for n in 0..burst {
        let _ = runtime.try_send_outcome(worker, SlowMsg::Job(n), &outcomes);
    }
    outcomes
        .wait_complete(Duration::from_secs(2))
        .expect("observers fire");

    let snap = outcomes.snapshot();
    assert_eq!(snap.submitted, burst);
    assert_eq!(snap.observed, burst);
    // The mailbox never drains, so exactly `capacity` admit and the rest
    // overflow — deterministically, every run.
    assert_eq!(
        snap.admitted, capacity as u32,
        "exactly capacity sends admit into a non-draining mailbox; snapshot={snap:?}"
    );
    assert_eq!(
        snap.mailbox_full,
        burst - capacity as u32,
        "every send past capacity overflows as MailboxFull; snapshot={snap:?}"
    );
    assert_eq!(snap.mailbox_closed, 0);
    assert_eq!(snap.worker_stopped, 0);
    assert_eq!(
        snap.admitted
            + snap.mailbox_full
            + snap.mailbox_closed
            + snap.ingress_full
            + snap.worker_stopped,
        burst,
        "every send must show up in exactly one outcome bucket"
    );
    // The isolate never ran (its mailbox reports empty): drain never happened.
    assert_eq!(processed.load(Ordering::Acquire), 0);

    let _ = runtime.shutdown();
}

#[test]
fn try_send_outcome_marks_mailbox_closed_after_stop() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");

    let stopped = runtime
        .observe_isolate_complete(worker)
        .expect("register completion observer");
    runtime.try_send(worker, SlowMsg::Stop).expect("kick stop");
    stopped.wait(Duration::from_secs(3)).expect("worker stops");

    let outcomes = HostBurstOutcomes::new();
    for n in 0..4 {
        let _ = runtime.try_send_outcome(worker, SlowMsg::Job(n), &outcomes);
    }
    outcomes
        .wait_complete(Duration::from_secs(2))
        .expect("observers fire");

    let snap = outcomes.snapshot();
    assert_eq!(snap.submitted, 4);
    assert_eq!(snap.observed, 4);
    assert_eq!(snap.admitted, 0);
    assert_eq!(snap.mailbox_closed, 4);

    let _ = runtime.shutdown();
}

#[test]
fn observed_send_rejects_known_failed_worker_before_enqueue() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        CapacityPanicMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            4,
        )
        .expect("register root before worker failure");

    assert!(matches!(
        runtime.register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::new(AtomicU32::new(0)),
            },
            13,
        ),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
    let ingress_before = runtime.topology().shards()[0]
        .ingress()
        .accepted()
        .expect("accepted ingress is measured");
    let observer_calls = Arc::new(AtomicU32::new(0));
    let message_drops = Arc::new(AtomicU32::new(0));
    let observer_calls_for_send = Arc::clone(&observer_calls);
    assert_eq!(
        runtime.try_send_and_observe_with(
            worker,
            SlowMsg::Owned(DropProbe(Arc::clone(&message_drops))),
            move |_| {
                observer_calls_for_send.fetch_add(1, Ordering::Release);
            },
        ),
        Err(ThreadedTrySendError::WorkerStopped)
    );
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(observer_calls.load(Ordering::Acquire), 0);
    assert_eq!(message_drops.load(Ordering::Acquire), 1);
    assert_eq!(processed.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.try_send(worker, SlowMsg::Job(1)),
        Err(ThreadedTrySendError::WorkerStopped),
        "ordinary and observed ingress must have worker-stop parity"
    );
    assert_eq!(
        runtime.topology().shards()[0].ingress().accepted(),
        Some(ingress_before),
        "failed-state rejection must not accept another command"
    );

    let outcomes = HostBurstOutcomes::new();
    assert_eq!(
        runtime.try_send_outcome(worker, SlowMsg::Job(3), &outcomes),
        Err(ThreadedTrySendError::WorkerStopped)
    );
    outcomes
        .wait_complete(Duration::from_millis(100))
        .expect("host-side worker-stop settles the burst");
    let snapshot = outcomes.snapshot();
    assert_eq!(snapshot.submitted, 1);
    assert_eq!(snapshot.observed, 1);
    assert_eq!(snapshot.worker_stopped, 1);
    assert_eq!(snapshot.admitted, 0);
    assert_eq!(
        runtime.topology().shards()[0].ingress().accepted(),
        Some(ingress_before),
        "host-burst rejection must not accept another command"
    );

    let factory_calls = Arc::new(AtomicU32::new(0));
    assert_eq!(
        runtime.send_observed_until(
            worker,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            || {
                factory_calls.fetch_add(1, Ordering::Release);
                SlowMsg::Job(4)
            },
        ),
        Err(SendObservedUntilError::WorkerStopped)
    );
    assert_eq!(factory_calls.load(Ordering::Acquire), 0);

    assert_eq!(runtime.shutdown(), Err(ThreadedRuntimeError::WorkerStopped));
}

#[test]
fn accepted_observed_sends_settle_when_worker_fails_before_execution() {
    let gate = Arc::new(FailureGate::default());
    let runtime = Arc::new(ThreadedRuntime::with_config(
        TestShard,
        TransitionPanicMailboxFactory {
            gate: Arc::clone(&gate),
        },
        ThreadedRuntimeConfig {
            command_capacity: 2,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            4,
        )
        .expect("register root before transition failure");

    let failing_runtime = Arc::clone(&runtime);
    let failure = std::thread::spawn(move || {
        failing_runtime.register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::new(AtomicU32::new(0)),
            },
            13,
        )
    });
    gate.wait_until_entered();

    let message_drops = Arc::new(AtomicU32::new(0));
    let preflight_calls = Arc::new(AtomicU32::new(0));
    let preflight_calls_for_send = Arc::clone(&preflight_calls);
    let (observer_tx, observer_rx) = std::sync::mpsc::channel();
    runtime
        .try_send_and_observe_with_preflight(
            worker,
            SlowMsg::Owned(DropProbe(Arc::clone(&message_drops))),
            move |_| {
                preflight_calls_for_send.fetch_add(1, Ordering::Release);
                None
            },
            move |outcome| observer_tx.send(outcome).expect("record observer outcome"),
        )
        .expect("command queue accepts observed send before worker failure");

    let outcomes = HostBurstOutcomes::new();
    runtime
        .try_send_outcome(
            worker,
            SlowMsg::Owned(DropProbe(Arc::clone(&message_drops))),
            &outcomes,
        )
        .expect("command queue accepts host-burst send before worker failure");
    assert_eq!(
        runtime.try_send_outcome(
            worker,
            SlowMsg::Owned(DropProbe(Arc::clone(&message_drops))),
            &outcomes,
        ),
        Err(ThreadedTrySendError::IngressFull),
        "a host-side full rejection must disarm its queued observer"
    );

    gate.release();
    assert!(matches!(
        failure.join().expect("failure driver joins"),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
    assert_eq!(
        observer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("accepted observer settles"),
        Err(tina_runtime::ThreadedSendObservedError::WorkerStopped)
    );
    assert!(
        observer_rx.recv_timeout(Duration::from_millis(20)).is_err(),
        "accepted command must settle its observer exactly once"
    );

    outcomes
        .wait_complete(Duration::from_secs(1))
        .expect("accepted host-burst command settles after worker failure");
    let snapshot = outcomes.snapshot();
    assert_eq!(snapshot.submitted, 2);
    assert_eq!(snapshot.observed, 2);
    assert_eq!(snapshot.worker_stopped, 1);
    assert_eq!(snapshot.admitted, 0);
    assert_eq!(snapshot.mailbox_full, 0);
    assert_eq!(snapshot.mailbox_closed, 0);
    assert_eq!(snapshot.ingress_full, 1);
    assert_eq!(message_drops.load(Ordering::Acquire), 3);
    assert_eq!(preflight_calls.load(Ordering::Acquire), 0);
    assert_eq!(processed.load(Ordering::Acquire), 0);

    let runtime = match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime,
        Err(_) => panic!("all runtime clones must retire before shutdown"),
    };
    assert_eq!(runtime.shutdown(), Err(ThreadedRuntimeError::WorkerStopped));
}

#[test]
fn accepted_preflight_panic_settles_observer_once_before_worker_stops() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            4,
        )
        .expect("register slow");
    let drops = Arc::new(AtomicU32::new(0));
    let (observer_tx, observer_rx) = std::sync::mpsc::channel();

    runtime
        .try_send_and_observe_with_preflight(
            worker,
            SlowMsg::Owned(DropProbe(Arc::clone(&drops))),
            |_| panic!("intentional preflight failure"),
            move |outcome| observer_tx.send(outcome).expect("report observer outcome"),
        )
        .expect("worker accepts preflight command");

    assert_eq!(
        observer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("unwind settles observer"),
        Err(tina_runtime::ThreadedSendObservedError::WorkerStopped)
    );
    assert!(observer_rx.recv_timeout(Duration::from_millis(20)).is_err());
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(processed.load(Ordering::Acquire), 0);
    assert_eq!(runtime.shutdown(), Err(ThreadedRuntimeError::WorkerStopped));
}

#[test]
fn accepted_mailbox_panic_settles_observer_once_before_worker_stops() {
    let runtime = ThreadedRuntime::new(TestShard, SendPanicMailboxFactory);
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            13,
        )
        .expect("register panic-on-send mailbox");
    let drops = Arc::new(AtomicU32::new(0));
    let (observer_tx, observer_rx) = std::sync::mpsc::channel();

    runtime
        .try_send_and_observe_with(
            worker,
            SlowMsg::Owned(DropProbe(Arc::clone(&drops))),
            move |outcome| observer_tx.send(outcome).expect("report observer outcome"),
        )
        .expect("worker accepts mailbox command");

    assert_eq!(
        observer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("unwind settles observer"),
        Err(tina_runtime::ThreadedSendObservedError::WorkerStopped)
    );
    assert!(observer_rx.recv_timeout(Duration::from_millis(20)).is_err());
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(processed.load(Ordering::Acquire), 0);
    assert_eq!(runtime.shutdown(), Err(ThreadedRuntimeError::WorkerStopped));
}

#[test]
fn deadline_claim_reports_worker_stopped_when_mailbox_admission_panics() {
    let runtime = ThreadedRuntime::new(TestShard, SendPanicMailboxFactory);
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            13,
        )
        .expect("register panic-on-send mailbox");
    let drops = Arc::new(AtomicU32::new(0));

    assert_eq!(
        runtime.send_observed_until(
            worker,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            || SlowMsg::Owned(DropProbe(Arc::clone(&drops))),
        ),
        Err(SendObservedUntilError::WorkerStopped)
    );
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(processed.load(Ordering::Acquire), 0);
    assert_eq!(runtime.shutdown(), Err(ThreadedRuntimeError::WorkerStopped));
}

#[test]
fn observer_panic_is_contained_after_settlement() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            4,
        )
        .expect("register slow");
    let observer_calls = Arc::new(AtomicU32::new(0));
    let observer_calls_for_send = Arc::clone(&observer_calls);

    runtime
        .try_send_and_observe_with(worker, SlowMsg::Job(1), move |_| {
            observer_calls_for_send.fetch_add(1, Ordering::Release);
            panic!("intentional observer panic");
        })
        .expect("worker accepts observed command");
    wait_until(|| observer_calls.load(Ordering::Acquire) == 1);
    runtime
        .send_and_observe(worker, SlowMsg::Job(2))
        .expect("worker survives observer panic");
    wait_until(|| processed.load(Ordering::Acquire) == 2);

    assert_eq!(observer_calls.load(Ordering::Acquire), 1);
    assert!(runtime.shutdown().is_ok());
}

// ---------- send_observed_until (Rock 4) ----------

#[test]
fn send_observed_until_succeeds_immediately_when_mailbox_has_room() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");

    runtime
        .send_observed_until(
            worker,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(2),
            || SlowMsg::Job(0),
        )
        .expect("admits on first try");

    let _ = runtime.shutdown();
}

#[test]
fn send_observed_until_returns_closed_when_target_stopped() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");

    let stopped = runtime
        .observe_isolate_complete(worker)
        .expect("register completion observer");
    runtime.try_send(worker, SlowMsg::Stop).expect("stop");
    stopped.wait(Duration::from_secs(3)).expect("worker stops");

    let outcome = runtime.send_observed_until(
        worker,
        Instant::now() + Duration::from_secs(1),
        Duration::from_millis(2),
        || SlowMsg::Job(0),
    );
    assert_eq!(outcome, Err(SendObservedUntilError::Closed));

    let _ = runtime.shutdown();
}

#[test]
fn send_observed_until_past_deadline_returns_timeout_without_attempting() {
    // Phase-062 P2-2: with a past deadline at entry, the helper must
    // not enqueue a command (which would race with the bounded recv
    // and risk delivering the message *and* reporting Timeout). It
    // must return Timeout up-front so the caller knows the side
    // effect did not happen.
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");

    let result = runtime.send_observed_until(
        worker,
        Instant::now() - Duration::from_secs(1),
        Duration::from_millis(10),
        || SlowMsg::Job(42),
    );
    assert_eq!(result, Err(SendObservedUntilError::Timeout));

    // Give the worker a moment in case any command leaked through;
    // none should have, so `processed` must stay at 0.
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(processed.load(Ordering::Acquire), 0);

    let _ = runtime.shutdown();
}

#[test]
fn message_factory_that_crosses_deadline_cannot_enqueue() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");
    let drops = Arc::new(AtomicU32::new(0));

    assert_eq!(
        runtime.send_observed_until(
            worker,
            Instant::now() + Duration::from_millis(10),
            Duration::from_millis(1),
            || {
                std::thread::sleep(Duration::from_millis(30));
                SlowMsg::Owned(DropProbe(Arc::clone(&drops)))
            },
        ),
        Err(SendObservedUntilError::Timeout)
    );
    assert_eq!(drops.load(Ordering::Acquire), 1);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(processed.load(Ordering::Acquire), 0);
    assert_eq!(drops.load(Ordering::Acquire), 1);

    let _ = runtime.shutdown();
}

#[test]
fn accepted_deadline_attempt_cannot_deliver_after_timeout() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 2,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");
    let gate = Arc::new(FailureGate::default());
    runtime
        .try_send(worker, SlowMsg::Hold(Arc::clone(&gate)))
        .expect("admit blocking handler");
    gate.wait_until_entered();

    let drops = Arc::new(AtomicU32::new(0));
    let calls = Arc::new(AtomicU32::new(0));
    let started = Instant::now();
    assert_eq!(
        runtime.send_observed_until(
            worker,
            started + Duration::from_millis(50),
            Duration::from_millis(1),
            || {
                calls.fetch_add(1, Ordering::Release);
                SlowMsg::Owned(DropProbe(Arc::clone(&drops)))
            },
        ),
        Err(SendObservedUntilError::Timeout)
    );
    assert!(started.elapsed() < Duration::from_millis(250));
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(processed.load(Ordering::Acquire), 0);

    gate.release();
    let drop_deadline = Instant::now() + Duration::from_secs(1);
    while drops.load(Ordering::Acquire) != 1 {
        assert!(
            Instant::now() < drop_deadline,
            "cancelled message was not retired"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(processed.load(Ordering::Acquire), 0);

    let _ = runtime.shutdown();
}

#[test]
fn worker_claim_wins_deadline_race_with_exact_admission_outcome() {
    let gate = Arc::new(FailureGate::default());
    let runtime = Arc::new(ThreadedRuntime::new(
        TestShard,
        GatedSendFactory {
            gate: Arc::clone(&gate),
        },
    ));
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            13,
        )
        .expect("register gated mailbox");
    let runtime_for_send = Arc::clone(&runtime);
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let send = std::thread::spawn(move || {
        let result = runtime_for_send.send_observed_until(
            worker,
            Instant::now() + Duration::from_millis(30),
            Duration::from_millis(1),
            || SlowMsg::Job(1),
        );
        result_tx.send(result).expect("report deadline outcome");
    });

    gate.wait_until_entered();
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        result_rx.try_recv().is_err(),
        "host cannot cancel after the worker claims mailbox authority"
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
        .expect("clean shutdown");
}

#[test]
fn send_observed_until_bounds_observation_when_worker_is_slow() {
    // Worker accepts the command but does not respond before the
    // deadline elapses. Without per-attempt bounding, recv() would
    // block until the worker eventually answered, blowing past the
    // caller's deadline. With per-attempt bounding, the helper
    // returns Timeout within ~deadline.
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            // cap=1, the burst will fill it quickly so the next
            // observe waits on a worker that is processing slowly.
            1,
        )
        .expect("register slow");

    // Pre-fill the mailbox so subsequent send_observed_until attempts
    // see Full and have to retry — which is exactly when a stuck
    // worker would extend the wait.
    runtime
        .try_send(worker, SlowMsg::Job(0))
        .expect("seed mailbox");

    let started = Instant::now();
    let cap = Duration::from_millis(150);
    let result =
        runtime.send_observed_until(worker, started + cap, Duration::from_millis(20), || {
            SlowMsg::Job(1)
        });
    let elapsed = started.elapsed();

    // Either the worker drains and we admit, or we Timeout at the
    // bounded deadline. Both are acceptable; the load-bearing
    // property is the elapsed cap.
    match result {
        Ok(()) | Err(SendObservedUntilError::Timeout) => {}
        other => panic!("expected Ok or Timeout, got {other:?}"),
    }
    // 2x cap is generous; without per-attempt bounding a wedged
    // worker could block this for seconds.
    assert!(
        elapsed < cap * 2,
        "send_observed_until exceeded 2x deadline cap: {elapsed:?}"
    );

    let _ = runtime.shutdown();
}

#[test]
fn send_observed_until_does_not_retry_closed() {
    // The match arms in `send_observed_until` retry only `MailboxFull`
    // and `IngressFull`. Closed targets must surface immediately so
    // the caller doesn't burn the deadline on a dead address.
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");

    let stopped = runtime
        .observe_isolate_complete(worker)
        .expect("register completion observer");
    runtime.try_send(worker, SlowMsg::Stop).expect("kick stop");
    stopped.wait(Duration::from_secs(3)).expect("worker stops");

    // Generous deadline. If the helper looped on Closed, this would
    // sleep until the deadline. It must return Closed immediately.
    let start = Instant::now();
    let result = runtime.send_observed_until(
        worker,
        Instant::now() + Duration::from_secs(10),
        Duration::from_millis(50),
        || SlowMsg::Job(0),
    );
    let elapsed = start.elapsed();
    assert_eq!(result, Err(SendObservedUntilError::Closed));
    assert!(
        elapsed < Duration::from_millis(500),
        "Closed must surface immediately, took {elapsed:?}"
    );

    let _ = runtime.shutdown();
}

#[test]
fn host_burst_wait_error_display_names_the_timeout() {
    // Pure API surface: Timeout's Display message is part of the
    // public contract because it shows up in `{e}` formatting.
    assert_eq!(
        format!("{}", HostBurstWaitError::Timeout),
        "timed out before every host-burst observer fired"
    );
}

#[test]
fn send_observed_until_error_display_names_each_variant() {
    assert_eq!(
        format!("{}", SendObservedUntilError::Timeout),
        "deadline elapsed before mailbox accepted the message"
    );
    assert_eq!(
        format!("{}", SendObservedUntilError::Closed),
        "target isolate mailbox is closed or stale"
    );
    assert_eq!(
        format!("{}", SendObservedUntilError::WorkerStopped),
        "worker thread stopped before the send could be observed"
    );
}
