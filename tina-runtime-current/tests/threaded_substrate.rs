use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallKind, MailboxFactory, RuntimeCall, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
    ThreadedRuntime, ThreadedRuntimeConfig, sleep,
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
