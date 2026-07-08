//! Debug tripwire for the "answers `call()` but only implements `handle`" bug.
//!
//! An isolate that receives `call()` traffic but never defined `handle_call`
//! keeps the default `handle_call`, which auto-rejects every call as
//! `UnsupportedMessage`. That whole class shipped invisibly once. The runtime
//! now counts those rejections in debug builds so the next occurrence surfaces
//! without an e2e test having to exist first. The signal is a side channel — it
//! records no trace event, so golden hashes do not move — and is a zero-cost
//! no-op in release.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::prelude::*;
use tina::{Mailbox, TrySendError};
use tina_runtime::{CallOutcome, MailboxFactory, Runtime, call};

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: RefCell<std::collections::VecDeque<T>>,
    closed: RefCell<bool>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: RefCell::new(std::collections::VecDeque::new()),
            closed: RefCell::new(false),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.borrow() {
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
    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }
    fn close(&self) {
        *self.closed.borrow_mut() = true;
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
struct Answer(u32);

#[derive(Debug)]
enum TargetMsg {
    Ping,
}

/// Handle-only target: no `handle_call`, so the default auto-rejects calls.
struct HandleOnly;

#[tina_runtime::isolate(message = TargetMsg, reply = Answer, shard = TestShard)]
impl HandleOnly {
    fn handle(
        &mut self,
        _msg: TargetMsg,
        _ctx: &mut Context<'_, TestShard, Answer>,
    ) -> Effect<Self> {
        noop()
    }
}

/// Proper callable target: a real `handle_call` that replies.
struct Answerer;

#[tina_runtime::isolate(message = TargetMsg, reply = Answer, shard = TestShard)]
impl Answerer {
    fn handle(
        &mut self,
        _msg: TargetMsg,
        _ctx: &mut Context<'_, TestShard, Answer>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _msg: TargetMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(Answer(7))
    }
}

#[derive(Debug)]
enum CallerMsg {
    CallTarget(Address<TargetMsg, Answer>),
    Returned(CallOutcome<Answer>),
}

struct Caller {
    outcomes: Rc<RefCell<Vec<CallOutcome<Answer>>>>,
}

#[tina_runtime::isolate(message = CallerMsg, shard = TestShard)]
impl Caller {
    fn handle(
        &mut self,
        msg: CallerMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CallerMsg::CallTarget(target) => {
                call(target, TargetMsg::Ping, Duration::from_secs(60)).then(CallerMsg::Returned)
            }
            CallerMsg::Returned(outcome) => {
                self.outcomes.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

fn drive_call_to<F>(register_target: F) -> (Vec<CallOutcome<Answer>>, u64)
where
    F: FnOnce(&mut Runtime<TestShard, TestMailboxFactory>) -> Address<TargetMsg, Answer>,
{
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let target = register_target(&mut runtime);
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register_with_capacity::<Caller, Infallible>(
        Caller {
            outcomes: Rc::clone(&outcomes),
        },
        8,
    );

    runtime
        .try_send(caller, CallerMsg::CallTarget(target))
        .expect("kick caller");
    for _ in 0..8 {
        runtime.step();
    }

    let recorded = outcomes.borrow().clone();
    (recorded, runtime.unsupported_message_rejections())
}

#[test]
fn call_to_handle_only_target_trips_the_default_handle_call_guard() {
    let (outcomes, tripwire) = drive_call_to(|runtime| {
        runtime.register_with_capacity::<HandleOnly, Infallible>(HandleOnly, 8)
    });

    assert_eq!(
        outcomes.as_slice(),
        [CallOutcome::Rejected(
            tina::CallRejectedReason::UnsupportedMessage
        )],
        "a handle-only target must reject the call as UnsupportedMessage"
    );
    if cfg!(debug_assertions) {
        assert_eq!(
            tripwire, 1,
            "the default-handle_call tripwire must fire exactly once in debug builds"
        );
    } else {
        assert_eq!(tripwire, 0, "the tripwire is a zero-cost no-op in release");
    }
}

#[test]
fn call_to_proper_handle_call_target_does_not_trip_the_guard() {
    let (outcomes, tripwire) = drive_call_to(|runtime| {
        runtime.register_with_capacity::<Answerer, Infallible>(Answerer, 8)
    });

    assert_eq!(
        outcomes.as_slice(),
        [CallOutcome::Replied(Answer(7))],
        "a proper handle_call target must reply, not reject"
    );
    assert_eq!(
        tripwire, 0,
        "a proper handle_call isolate must never trip the default-handle_call guard"
    );
}
