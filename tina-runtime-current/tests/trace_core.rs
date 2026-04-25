use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;

use tina::{
    Address, Context, Effect, Isolate, IsolateId, Mailbox, SendMessage, Shard, ShardId, SpawnSpec,
    TrySendError,
};
use tina_runtime_current::{
    CauseId, EffectKind, EventId, RuntimeEvent, RuntimeEventKind, SingleIsolateRunner, StepOutcome,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionMsg {
    Record(u8),
    Reply,
    Send,
    Spawn,
    Restart,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditMsg {
    Record(u8),
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
    type Send = ();
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        Effect::Noop
    }
}

#[derive(Debug)]
struct Session {
    handled: usize,
    seen: Vec<u8>,
    audit: Address<AuditMsg>,
}

impl Isolate for Session {
    type Message = SessionMsg;
    type Reply = usize;
    type Send = SendMessage<AuditMsg>;
    type Spawn = SpawnSpec<Worker>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        self.handled += 1;

        match msg {
            SessionMsg::Record(value) => {
                self.seen.push(value);
                Effect::Noop
            }
            SessionMsg::Reply => Effect::Reply(self.handled),
            SessionMsg::Send => Effect::Send(SendMessage::new(
                self.audit,
                AuditMsg::Record(self.handled as u8),
            )),
            SessionMsg::Spawn => Effect::Spawn(SpawnSpec::new(Worker, 2)),
            SessionMsg::Restart => Effect::RestartChildren,
            SessionMsg::Stop => Effect::Stop,
        }
    }
}

struct BoundedMailbox<T> {
    capacity: usize,
    queue: RefCell<VecDeque<T>>,
    closed: Cell<bool>,
}

impl<T> BoundedMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
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

fn runner() -> SingleIsolateRunner<Session, BoundedMailbox<SessionMsg>, TestShard> {
    let audit = Address::new(ShardId::new(9), IsolateId::new(41));

    SingleIsolateRunner::new(
        Session {
            handled: 0,
            seen: Vec::new(),
            audit,
        },
        BoundedMailbox::new(8),
        TestShard,
        IsolateId::new(12),
    )
}

#[test]
fn one_delivery_records_the_exact_causal_chain() {
    let mut runner = runner();
    assert_eq!(runner.mailbox().try_send(SessionMsg::Record(7)), Ok(()));

    assert_eq!(runner.step_once(), StepOutcome::Delivered);
    assert_eq!(runner.isolate().handled, 1);
    assert_eq!(runner.isolate().seen, vec![7]);
    assert_eq!(
        runner.trace(),
        [
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(3),
                IsolateId::new(12),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(2),
                Some(CauseId::new(EventId::new(1))),
                ShardId::new(3),
                IsolateId::new(12),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(3),
                Some(CauseId::new(EventId::new(2))),
                ShardId::new(3),
                IsolateId::new(12),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Noop,
                },
            ),
            RuntimeEvent::new(
                EventId::new(4),
                Some(CauseId::new(EventId::new(3))),
                ShardId::new(3),
                IsolateId::new(12),
                RuntimeEventKind::EffectObserved {
                    effect: EffectKind::Noop,
                },
            ),
        ]
    );
}

#[test]
fn repeated_steps_preserve_fifo_order() {
    let mut runner = runner();
    assert_eq!(runner.mailbox().try_send(SessionMsg::Record(1)), Ok(()));
    assert_eq!(runner.mailbox().try_send(SessionMsg::Record(2)), Ok(()));
    assert_eq!(runner.mailbox().try_send(SessionMsg::Record(3)), Ok(()));

    assert_eq!(runner.step_once(), StepOutcome::Delivered);
    assert_eq!(runner.step_once(), StepOutcome::Delivered);
    assert_eq!(runner.step_once(), StepOutcome::Delivered);
    assert_eq!(runner.step_once(), StepOutcome::Idle);

    assert_eq!(runner.isolate().handled, 3);
    assert_eq!(runner.isolate().seen, vec![1, 2, 3]);
}

#[test]
fn stop_prevents_later_delivery() {
    let mut runner = runner();
    assert_eq!(runner.mailbox().try_send(SessionMsg::Stop), Ok(()));
    assert_eq!(runner.mailbox().try_send(SessionMsg::Record(9)), Ok(()));

    assert_eq!(runner.step_once(), StepOutcome::Delivered);
    assert!(runner.is_stopped());
    let trace_len_after_stop = runner.trace().len();

    assert_eq!(runner.step_once(), StepOutcome::Idle);
    assert_eq!(runner.trace().len(), trace_len_after_stop);
    assert_eq!(runner.isolate().handled, 1);
    assert!(runner.isolate().seen.is_empty());
    assert_eq!(
        runner.trace().last().map(|event| event.kind()),
        Some(RuntimeEventKind::IsolateStopped)
    );
}

#[test]
fn non_stop_effects_are_observed_and_traced_without_execution() {
    let cases = [
        (SessionMsg::Record(1), EffectKind::Noop),
        (SessionMsg::Reply, EffectKind::Reply),
        (SessionMsg::Send, EffectKind::Send),
        (SessionMsg::Spawn, EffectKind::Spawn),
        (SessionMsg::Restart, EffectKind::RestartChildren),
    ];

    for (message, expected_effect) in cases {
        let mut runner = runner();
        assert_eq!(runner.mailbox().try_send(message), Ok(()));

        assert_eq!(runner.step_once(), StepOutcome::Delivered);
        assert!(!runner.is_stopped());
        assert_eq!(runner.trace().len(), 4);
        assert_eq!(
            runner.trace()[2].kind(),
            RuntimeEventKind::HandlerFinished {
                effect: expected_effect,
            }
        );
        assert_eq!(
            runner.trace()[3].kind(),
            RuntimeEventKind::EffectObserved {
                effect: expected_effect,
            }
        );
    }
}

#[test]
fn identical_runs_produce_identical_event_sequences_and_causal_links() {
    let mut left = runner();
    let mut right = runner();

    assert_eq!(left.mailbox().try_send(SessionMsg::Record(5)), Ok(()));
    assert_eq!(left.mailbox().try_send(SessionMsg::Reply), Ok(()));
    assert_eq!(right.mailbox().try_send(SessionMsg::Record(5)), Ok(()));
    assert_eq!(right.mailbox().try_send(SessionMsg::Reply), Ok(()));

    assert_eq!(left.step_once(), StepOutcome::Delivered);
    assert_eq!(left.step_once(), StepOutcome::Delivered);
    assert_eq!(right.step_once(), StepOutcome::Delivered);
    assert_eq!(right.step_once(), StepOutcome::Delivered);

    assert_eq!(left.trace(), right.trace());
}
