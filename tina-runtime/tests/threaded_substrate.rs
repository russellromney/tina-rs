use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallKind, MailboxFactory, RuntimeCall, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
    ThreadedMultiShardRuntime, ThreadedRuntime, ThreadedRuntimeConfig, sleep,
};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(61)
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
        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
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

fn wait_until<F>(timeout: Duration, label: &str, mut predicate: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate() {
        if Instant::now() > deadline {
            panic!("wait_until({label}): predicate not satisfied within timeout");
        }
        thread::yield_now();
    }
}

fn count_event(trace: &[RuntimeEvent], predicate: impl Fn(&RuntimeEventKind) -> bool) -> usize {
    trace
        .iter()
        .filter(|event| predicate(&event.kind()))
        .count()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryMsg {
    TryWork,
    RetryNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryObservation {
    Attempted(usize),
    Failed(usize),
    BackoffElapsed,
    Succeeded(usize),
}

#[derive(Debug)]
struct RetryWorker {
    backoff: Duration,
    attempts: usize,
    observations: Arc<Mutex<Vec<RetryObservation>>>,
}

impl Isolate for RetryWorker {
    tina::isolate_types! {
        message: RetryMsg,
        reply: (),
        send: Outbound<RetryMsg>,
        spawn: Infallible,
        call: RuntimeCall<RetryMsg>,
        shard: TestShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            RetryMsg::TryWork => {
                self.attempts += 1;
                self.observations
                    .lock()
                    .expect("observations mutex")
                    .push(RetryObservation::Attempted(self.attempts));
                if self.attempts == 1 {
                    self.observations
                        .lock()
                        .expect("observations mutex")
                        .push(RetryObservation::Failed(self.attempts));
                    sleep(self.backoff).reply(|_| RetryMsg::RetryNow)
                } else {
                    self.observations
                        .lock()
                        .expect("observations mutex")
                        .push(RetryObservation::Succeeded(self.attempts));
                    noop()
                }
            }
            RetryMsg::RetryNow => {
                self.observations
                    .lock()
                    .expect("observations mutex")
                    .push(RetryObservation::BackoffElapsed);
                ctx.send_self(RetryMsg::TryWork)
            }
        }
    }
}

#[test]
fn threaded_runtime_timer_retry_runs_without_manual_stepping() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
        },
    );
    let worker = runtime
        .register_with_capacity::<RetryWorker, _>(
            RetryWorker {
                backoff: Duration::from_millis(5),
                attempts: 0,
                observations: Arc::clone(&observations),
            },
            8,
        )
        .expect("threaded register accepts");

    runtime
        .try_send(worker, RetryMsg::TryWork)
        .expect("retry handoff accepted");

    wait_until(Duration::from_secs(2), "threaded retry", || {
        observations.lock().expect("observations mutex").as_slice()
            == [
                RetryObservation::Attempted(1),
                RetryObservation::Failed(1),
                RetryObservation::BackoffElapsed,
                RetryObservation::Attempted(2),
                RetryObservation::Succeeded(2),
            ]
    });

    let trace = runtime.shutdown().expect("threaded shutdown");
    assert_eq!(
        count_event(&trace, |kind| matches!(
            kind,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::Sleep,
                ..
            }
        )),
        1
    );
}

#[derive(Debug, Clone, Copy)]
enum DriverMsg {
    FillTwice,
}

#[derive(Debug, Clone, Copy)]
enum SinkMsg {
    Hit,
}

#[derive(Debug)]
struct Driver {
    sink: Address<SinkMsg>,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: Outbound<SinkMsg>,
        spawn: Infallible,
        call: RuntimeCall<DriverMsg>,
        shard: TestShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            DriverMsg::FillTwice => {
                batch([send(self.sink, SinkMsg::Hit), send(self.sink, SinkMsg::Hit)])
            }
        }
    }
}

#[derive(Debug)]
struct Sink;

impl Isolate for Sink {
    tina::isolate_types! {
        message: SinkMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<SinkMsg>,
        shard: TestShard,
    }

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        noop()
    }
}

#[test]
fn threaded_runtime_local_mailbox_full_is_visible_in_trace() {
    let runtime = ThreadedRuntime::new(TestShard, TestMailboxFactory);
    let sink = runtime
        .register_with_capacity::<Sink, _>(Sink, 1)
        .expect("sink register accepts");
    let driver = runtime
        .register_with_capacity::<Driver, _>(Driver { sink }, 8)
        .expect("driver register accepts");

    runtime
        .try_send(driver, DriverMsg::FillTwice)
        .expect("driver handoff accepted");

    wait_until(Duration::from_secs(2), "threaded local full", || {
        let trace = runtime.trace().expect("threaded trace");
        trace.iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Full,
                    ..
                }
            )
        })
    });

    let trace = runtime.shutdown().expect("threaded shutdown");
    assert_eq!(
        count_event(&trace, |kind| matches!(
            kind,
            RuntimeEventKind::SendRejected {
                reason: SendRejectedReason::Full,
                ..
            }
        )),
        1
    );
}

#[derive(Debug, Clone, Copy)]
struct WorkShard(u32);

impl Shard for WorkShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoordinatorMsg {
    Submit { job_id: u64, value: u64 },
    SubmitAfterBadRemote { job_id: u64, value: u64 },
    Completed { job_id: u64, doubled: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerMsg {
    Run {
        job_id: u64,
        value: u64,
        reply_to: Address<CoordinatorMsg>,
    },
}

#[derive(Debug)]
struct Coordinator {
    worker: Address<WorkerMsg>,
    bad_worker: Option<Address<WorkerMsg>>,
    completed: Arc<Mutex<Vec<(u64, u64)>>>,
}

impl Isolate for Coordinator {
    tina::isolate_types! {
        message: CoordinatorMsg,
        reply: (),
        send: Outbound<WorkerMsg>,
        spawn: Infallible,
        call: RuntimeCall<CoordinatorMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CoordinatorMsg::Submit { job_id, value } => send(
                self.worker,
                WorkerMsg::Run {
                    job_id,
                    value,
                    reply_to: ctx.me(),
                },
            ),
            CoordinatorMsg::SubmitAfterBadRemote { job_id, value } => batch([
                send(
                    self.bad_worker.expect("bad worker address configured"),
                    WorkerMsg::Run {
                        job_id: 0,
                        value: 0,
                        reply_to: ctx.me(),
                    },
                ),
                send(
                    self.worker,
                    WorkerMsg::Run {
                        job_id,
                        value,
                        reply_to: ctx.me(),
                    },
                ),
            ]),
            CoordinatorMsg::Completed { job_id, doubled } => {
                self.completed
                    .lock()
                    .expect("completed mutex")
                    .push((job_id, doubled));
                noop()
            }
        }
    }
}

#[derive(Debug)]
struct Worker;

impl Isolate for Worker {
    tina::isolate_types! {
        message: WorkerMsg,
        reply: (),
        send: Outbound<CoordinatorMsg>,
        spawn: Infallible,
        call: RuntimeCall<WorkerMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Run {
                job_id,
                value,
                reply_to,
            } => send(
                reply_to,
                CoordinatorMsg::Completed {
                    job_id,
                    doubled: value * 2,
                },
            ),
        }
    }
}

#[derive(Debug)]
struct WorkSink;

impl Isolate for WorkSink {
    tina::isolate_types! {
        message: SinkMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<SinkMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        noop()
    }
}

fn count_send_rejected_full(trace: &[RuntimeEvent]) -> usize {
    count_event(trace, |kind| {
        matches!(
            kind,
            RuntimeEventKind::SendRejected {
                reason: SendRejectedReason::Full,
                ..
            }
        )
    })
}

fn has_send_accepted_between(trace: &[RuntimeEvent], from: u32, to: u32) -> bool {
    trace.iter().any(|event| {
        event.shard() == ShardId::new(from)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendAccepted { target_shard, .. }
                    if target_shard == ShardId::new(to)
            )
    })
}

#[test]
fn threaded_multishard_dispatcher_round_trips_between_worker_threads() {
    let runtime = ThreadedMultiShardRuntime::with_config(
        [WorkShard(1), WorkShard(2)],
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
        },
    );
    let completed = Arc::new(Mutex::new(Vec::new()));
    let worker = runtime
        .register_with_capacity_on::<Worker, _>(ShardId::new(2), Worker, 8)
        .expect("worker register accepts");
    let coordinator = runtime
        .register_with_capacity_on::<Coordinator, _>(
            ShardId::new(1),
            Coordinator {
                worker,
                bad_worker: None,
                completed: Arc::clone(&completed),
            },
            8,
        )
        .expect("coordinator register accepts");

    runtime
        .try_send(
            coordinator,
            CoordinatorMsg::Submit {
                job_id: 7,
                value: 21,
            },
        )
        .expect("submit handoff accepted");

    wait_until(
        Duration::from_secs(2),
        "threaded multishard dispatch",
        || completed.lock().expect("completed mutex").as_slice() == [(7, 42)],
    );

    let trace = runtime.shutdown().expect("threaded multishard shutdown");
    assert!(has_send_accepted_between(&trace, 1, 2));
    assert!(has_send_accepted_between(&trace, 2, 1));
    assert_eq!(count_send_rejected_full(&trace), 0);
}

#[test]
fn threaded_multishard_bad_remote_does_not_poison_good_remote_work() {
    let runtime = ThreadedMultiShardRuntime::new([WorkShard(1), WorkShard(2)], TestMailboxFactory);
    let completed = Arc::new(Mutex::new(Vec::new()));
    let worker = runtime
        .register_with_capacity_on::<Worker, _>(ShardId::new(2), Worker, 8)
        .expect("worker register accepts");
    let bad_worker = Address::new(ShardId::new(2), IsolateId::new(99));
    let coordinator = runtime
        .register_with_capacity_on::<Coordinator, _>(
            ShardId::new(1),
            Coordinator {
                worker,
                bad_worker: Some(bad_worker),
                completed: Arc::clone(&completed),
            },
            8,
        )
        .expect("coordinator register accepts");

    runtime
        .try_send(
            coordinator,
            CoordinatorMsg::SubmitAfterBadRemote {
                job_id: 11,
                value: 5,
            },
        )
        .expect("submit handoff accepted");

    wait_until(
        Duration::from_secs(2),
        "threaded bad then good remote",
        || completed.lock().expect("completed mutex").as_slice() == [(11, 10)],
    );

    let trace = runtime.shutdown().expect("threaded multishard shutdown");
    assert!(trace.iter().any(|event| {
        event.isolate() == IsolateId::new(99)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Closed,
                    ..
                }
            )
    }));
    assert!(has_send_accepted_between(&trace, 2, 1));
}

#[derive(Debug, Clone, Copy)]
enum ParkMsg {
    Park,
    Wake,
}

#[derive(Debug)]
struct ParkWorker {
    parked_tx: Option<mpsc::SyncSender<()>>,
    wake_rx: mpsc::Receiver<()>,
}

impl Isolate for ParkWorker {
    tina::isolate_types! {
        message: ParkMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<ParkMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ParkMsg::Park => {
                if let Some(parked_tx) = self.parked_tx.take() {
                    parked_tx.send(()).expect("test observes parked handler");
                }
                self.wake_rx.recv().expect("test releases parked handler");
                noop()
            }
            ParkMsg::Wake => noop(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum BurstMsg {
    Burst,
}

#[derive(Debug)]
struct RemoteBurst {
    sink: Address<SinkMsg>,
}

impl Isolate for RemoteBurst {
    tina::isolate_types! {
        message: BurstMsg,
        reply: (),
        send: Outbound<SinkMsg>,
        spawn: Infallible,
        call: RuntimeCall<BurstMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            BurstMsg::Burst => {
                batch([send(self.sink, SinkMsg::Hit), send(self.sink, SinkMsg::Hit)])
            }
        }
    }
}

#[test]
fn threaded_multishard_remote_queue_full_is_visible_at_source() {
    let runtime = ThreadedMultiShardRuntime::with_config(
        [WorkShard(1), WorkShard(2)],
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 1,
            idle_wait: Duration::from_millis(1),
        },
    );
    let (parked_tx, parked_rx) = mpsc::sync_channel(0);
    let (wake_tx, wake_rx) = mpsc::sync_channel(0);
    let parker = runtime
        .register_with_capacity_on::<ParkWorker, _>(
            ShardId::new(2),
            ParkWorker {
                parked_tx: Some(parked_tx),
                wake_rx,
            },
            8,
        )
        .expect("parker register accepts");
    let sink = runtime
        .register_with_capacity_on::<WorkSink, _>(ShardId::new(2), WorkSink, 8)
        .expect("sink register accepts");
    let burst = runtime
        .register_with_capacity_on::<RemoteBurst, _>(ShardId::new(1), RemoteBurst { sink }, 8)
        .expect("burst register accepts");

    runtime
        .try_send(parker, ParkMsg::Park)
        .expect("park handoff accepted");
    parked_rx.recv().expect("worker reached parked handler");
    runtime
        .try_send(parker, ParkMsg::Wake)
        .expect("wake command fills target queue");
    runtime
        .try_send(burst, BurstMsg::Burst)
        .expect("burst handoff accepted");

    wait_until(Duration::from_secs(2), "threaded remote full", || {
        let trace = runtime
            .trace_on(ShardId::new(1))
            .expect("source shard trace");
        count_send_rejected_full(&trace) >= 1
    });
    wake_tx.send(()).expect("release parked target worker");

    let trace = runtime.shutdown().expect("threaded multishard shutdown");
    assert!(trace.iter().any(|event| {
        event.shard() == ShardId::new(1)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    target_shard,
                    reason: SendRejectedReason::Full,
                    ..
                } if target_shard == ShardId::new(2)
            )
    }));
}
