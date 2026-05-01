//! Public-path retry/backoff proof for runtime-owned time.
//!
//! This complements the crate-local timer semantics tests in `src/tests.rs`
//! with a black-box integration test that drives the shipped runtime surface:
//!
//! - an isolate issues `sleep(..).reply(..)`
//! - the runtime delays wake delivery through its normal call-completion path
//! - the translated wake message causes a real second attempt on a later turn
//! - the second attempt succeeds

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallKind, MailboxFactory, Runtime, RuntimeCall, RuntimeEvent, RuntimeEventKind, sleep,
};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(23)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryEvent {
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
    observations: Rc<RefCell<Vec<RetryObservation>>>,
}

impl Isolate for RetryWorker {
    tina::isolate_types! {
        message: RetryEvent,
        reply: (),
        send: Outbound<RetryEvent>,
        spawn: Infallible,
        call: RuntimeCall<RetryEvent>,
        shard: TestShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            RetryEvent::TryWork => {
                self.attempts += 1;
                self.observations
                    .borrow_mut()
                    .push(RetryObservation::Attempted(self.attempts));
                if self.attempts == 1 {
                    self.observations
                        .borrow_mut()
                        .push(RetryObservation::Failed(self.attempts));
                    sleep(self.backoff).reply(|_| RetryEvent::RetryNow)
                } else {
                    self.observations
                        .borrow_mut()
                        .push(RetryObservation::Succeeded(self.attempts));
                    noop()
                }
            }
            RetryEvent::RetryNow => {
                self.observations
                    .borrow_mut()
                    .push(RetryObservation::BackoffElapsed);
                ctx.send_self(RetryEvent::TryWork)
            }
        }
    }
}

fn step_until<F>(
    runtime: &mut Runtime<TestShard, TestMailboxFactory>,
    timeout: Duration,
    label: &str,
    predicate: F,
) where
    F: Fn(&Runtime<TestShard, TestMailboxFactory>) -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate(runtime) {
        if Instant::now() > deadline {
            panic!(
                "step_until({label}): predicate not satisfied within timeout; trace = {:#?}",
                runtime.trace()
            );
        }
        runtime.step();
        thread::sleep(Duration::from_millis(2));
    }
}

fn count_call_completed(trace: &[RuntimeEvent], kind: CallKind) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == kind
            )
        })
        .count()
}

fn count_call_dispatch_attempted(trace: &[RuntimeEvent], kind: CallKind) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallDispatchAttempted { call_kind, .. } if call_kind == kind
            )
        })
        .count()
}

#[test]
fn retry_backoff_public_path_retries_after_timer_wake() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let observations = Rc::new(RefCell::new(Vec::new()));
    let worker = runtime.register(
        RetryWorker {
            backoff: Duration::from_millis(50),
            attempts: 0,
            observations: Rc::clone(&observations),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(worker, RetryEvent::TryWork)
        .expect("ingress accepts first attempt");

    assert_eq!(runtime.step(), 1);
    assert_eq!(
        observations.borrow().as_slice(),
        [RetryObservation::Attempted(1), RetryObservation::Failed(1),]
    );
    assert!(
        runtime.has_in_flight_calls(),
        "the backoff timer should now be pending"
    );

    // Immediate next step should not see the timer fire yet on the public
    // monotonic-clock runtime.
    assert_eq!(runtime.step(), 0);
    assert_eq!(
        observations.borrow().as_slice(),
        [RetryObservation::Attempted(1), RetryObservation::Failed(1),]
    );
    assert!(
        runtime.has_in_flight_calls(),
        "backoff timer should still be pending before its due time"
    );

    step_until(
        &mut runtime,
        Duration::from_secs(2),
        "retry_backoff_success",
        |_| {
            observations.borrow().as_slice()
                == [
                    RetryObservation::Attempted(1),
                    RetryObservation::Failed(1),
                    RetryObservation::BackoffElapsed,
                    RetryObservation::Attempted(2),
                    RetryObservation::Succeeded(2),
                ]
        },
    );

    assert!(
        !runtime.has_in_flight_calls(),
        "retry workload should drain cleanly after the second attempt succeeds"
    );

    let trace = runtime.trace();
    assert_eq!(
        count_call_dispatch_attempted(trace, CallKind::Sleep),
        1,
        "retry workload should dispatch exactly one backoff Sleep call"
    );
    assert_eq!(
        count_call_completed(trace, CallKind::Sleep),
        1,
        "retry workload should complete exactly one backoff Sleep call"
    );
    assert!(
        !trace.iter().any(|event| matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::Sleep,
                ..
            } | RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::Sleep,
                ..
            }
        )),
        "retry workload should not fail or reject the Sleep completion"
    );
}
