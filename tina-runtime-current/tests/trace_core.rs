use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;

use tina::{
    Address, Context, Effect, Isolate, IsolateId, Mailbox, SendMessage, Shard, ShardId, SpawnSpec,
    TrySendError,
};
use tina_runtime_current::{
    CauseId, CurrentRuntime, EffectKind, EventId, MailboxFactory, RuntimeEvent, RuntimeEventKind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionMsg {
    Record(u8),
    Reply,
    Spawn,
    Restart,
    Stop,
}

#[derive(Debug)]
struct Worker;

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(3)
    }
}

impl Isolate for Worker {
    type Message = ();
    type Reply = ();
    type Send = SendMessage<Infallible>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        Effect::Noop
    }
}

#[derive(Debug)]
struct Session {
    handled: Rc<RefCell<usize>>,
    seen: Rc<RefCell<Vec<u8>>>,
}

impl Isolate for Session {
    type Message = SessionMsg;
    type Reply = usize;
    type Send = SendMessage<Infallible>;
    type Spawn = SpawnSpec<Worker>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        *self.handled.borrow_mut() += 1;

        match msg {
            SessionMsg::Record(value) => {
                self.seen.borrow_mut().push(value);
                Effect::Noop
            }
            SessionMsg::Reply => Effect::Reply(*self.handled.borrow()),
            SessionMsg::Spawn => Effect::Spawn(SpawnSpec::new(Worker, 2)),
            SessionMsg::Restart => Effect::RestartChildren,
            SessionMsg::Stop => Effect::Stop,
        }
    }
}

struct BoundedMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> Clone for BoundedMailbox<T> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            queue: Rc::clone(&self.queue),
            closed: Rc::clone(&self.closed),
        }
    }
}

impl<T> BoundedMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for BoundedMailbox<T> {
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
        Box::new(BoundedMailbox::new(capacity))
    }
}

struct RuntimeHarness {
    runtime: CurrentRuntime<TestShard, TestMailboxFactory>,
    session: Address<SessionMsg>,
    handled: Rc<RefCell<usize>>,
    seen: Rc<RefCell<Vec<u8>>>,
}

impl RuntimeHarness {
    fn new() -> Self {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let handled = Rc::new(RefCell::new(0));
        let seen = Rc::new(RefCell::new(Vec::new()));
        let session = runtime.register(
            Session {
                handled: Rc::clone(&handled),
                seen: Rc::clone(&seen),
            },
            BoundedMailbox::new(8),
        );

        Self {
            runtime,
            session,
            handled,
            seen,
        }
    }

    fn send(&self, message: SessionMsg) {
        assert_eq!(self.runtime.try_send(self.session, message), Ok(()));
    }
}

fn harness() -> RuntimeHarness {
    RuntimeHarness::new()
}

#[test]
fn one_delivery_records_the_exact_causal_chain() {
    let mut harness = harness();
    harness.send(SessionMsg::Record(7));

    assert_eq!(harness.runtime.step(), 1);
    assert_eq!(*harness.handled.borrow(), 1);
    assert_eq!(*harness.seen.borrow(), vec![7]);
    assert_eq!(
        harness.runtime.trace(),
        [
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(3),
                IsolateId::new(1),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(2),
                Some(CauseId::new(EventId::new(1))),
                ShardId::new(3),
                IsolateId::new(1),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(3),
                Some(CauseId::new(EventId::new(2))),
                ShardId::new(3),
                IsolateId::new(1),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Noop,
                },
            ),
            RuntimeEvent::new(
                EventId::new(4),
                Some(CauseId::new(EventId::new(3))),
                ShardId::new(3),
                IsolateId::new(1),
                RuntimeEventKind::EffectObserved {
                    effect: EffectKind::Noop,
                },
            ),
        ]
    );
}

#[test]
fn repeated_steps_preserve_fifo_order() {
    let mut harness = harness();
    harness.send(SessionMsg::Record(1));
    harness.send(SessionMsg::Record(2));
    harness.send(SessionMsg::Record(3));

    assert_eq!(harness.runtime.step(), 1);
    assert_eq!(harness.runtime.step(), 1);
    assert_eq!(harness.runtime.step(), 1);
    assert_eq!(harness.runtime.step(), 0);

    assert_eq!(*harness.handled.borrow(), 3);
    assert_eq!(*harness.seen.borrow(), vec![1, 2, 3]);
}

#[test]
fn stop_prevents_later_delivery() {
    let mut harness = harness();
    harness.send(SessionMsg::Stop);
    harness.send(SessionMsg::Record(9));

    assert_eq!(harness.runtime.step(), 1);
    let trace_len_after_stop = harness.runtime.trace().len();

    assert_eq!(harness.runtime.step(), 0);
    assert_eq!(harness.runtime.trace().len(), trace_len_after_stop);
    assert_eq!(*harness.handled.borrow(), 1);
    assert!(harness.seen.borrow().is_empty());
    assert_eq!(
        harness.runtime.trace().last().map(|event| event.kind()),
        Some(RuntimeEventKind::MessageAbandoned)
    );
    assert!(
        harness
            .runtime
            .trace()
            .iter()
            .any(|event| event.kind() == RuntimeEventKind::IsolateStopped)
    );
}

#[test]
fn non_stop_effects_are_observed_and_traced_without_execution() {
    let cases = [
        (SessionMsg::Record(1), EffectKind::Noop),
        (SessionMsg::Reply, EffectKind::Reply),
        (SessionMsg::Spawn, EffectKind::Spawn),
    ];

    for (message, expected_effect) in cases {
        let mut harness = harness();
        harness.send(message);

        assert_eq!(harness.runtime.step(), 1);
        assert_eq!(
            harness.runtime.trace()[2].kind(),
            RuntimeEventKind::HandlerFinished {
                effect: expected_effect,
            }
        );
    }
}

#[test]
fn restart_children_without_children_emits_no_restart_subtree() {
    let mut harness = harness();
    harness.send(SessionMsg::Restart);

    assert_eq!(harness.runtime.step(), 1);
    assert_eq!(
        harness.runtime.trace()[2].kind(),
        RuntimeEventKind::HandlerFinished {
            effect: EffectKind::RestartChildren,
        }
    );
    assert!(!harness.runtime.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::RestartChildAttempted { .. }
                | RuntimeEventKind::RestartChildSkipped { .. }
                | RuntimeEventKind::RestartChildCompleted { .. }
        )
    }));
}

#[test]
fn identical_runs_produce_identical_event_sequences_and_causal_links() {
    fn run_once() -> Vec<RuntimeEvent> {
        let mut harness = harness();
        harness.send(SessionMsg::Record(5));
        harness.send(SessionMsg::Reply);

        assert_eq!(harness.runtime.step(), 1);
        assert_eq!(harness.runtime.step(), 1);

        harness.runtime.trace().to_vec()
    }

    assert_eq!(run_once(), run_once());
}
