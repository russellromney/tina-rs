use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;

use tina::{Mailbox, TrySendError, prelude::*};

#[derive(Debug, Default)]
struct InlineShard;

impl Shard for InlineShard {
    fn id(&self) -> ShardId {
        ShardId::new(3)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuditMsg {
    Recorded(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionMsg {
    Note(String),
    Snapshot,
    SpawnChild,
    StopNow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChildMsg {
    Start,
}

#[derive(Debug)]
struct ChildWorker;

impl Isolate for ChildWorker {
    type Message = ChildMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = InlineShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ChildMsg::Start => stop(),
        }
    }
}

#[derive(Debug)]
struct Session {
    notes: Vec<String>,
    audit: Address<AuditMsg>,
}

impl Isolate for Session {
    type Message = SessionMsg;
    type Reply = Vec<String>;
    type Send = Outbound<AuditMsg>;
    type Spawn = ChildDefinition<ChildWorker>;
    type Call = Infallible;
    type Shard = InlineShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            SessionMsg::Note(note) => {
                self.notes.push(note.clone());
                send(self.audit, AuditMsg::Recorded(note))
            }
            SessionMsg::Snapshot => reply(self.notes.clone()),
            SessionMsg::SpawnChild => {
                spawn(ChildDefinition::new(ChildWorker, 4).with_initial_message(ChildMsg::Start))
            }
            SessionMsg::StopNow => stop(),
        }
    }
}

struct LocalMailbox<T> {
    capacity: usize,
    queue: RefCell<VecDeque<T>>,
    closed: Cell<bool>,
}

impl<T> LocalMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        }
    }
}

impl<T> Mailbox<T> for LocalMailbox<T> {
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

#[test]
fn preferred_surface_helpers_build_expected_effects() {
    let audit = Address::new(ShardId::new(3), IsolateId::new(41));
    let mut shard = InlineShard;
    let mut ctx = Context::new(&mut shard, IsolateId::new(7));
    let mut session = Session {
        notes: Vec::new(),
        audit,
    };

    match session.handle(SessionMsg::Note("hello".to_owned()), &mut ctx) {
        Effect::Send(outbound) => {
            assert_eq!(outbound.destination(), audit);
            assert_eq!(outbound.message(), &AuditMsg::Recorded("hello".to_owned()));
        }
        other => panic!("expected send effect, got {other:?}"),
    }

    assert!(matches!(
        session.handle(SessionMsg::Snapshot, &mut ctx),
        Effect::Reply(ref notes) if notes == &vec!["hello".to_owned()]
    ));

    match session.handle(SessionMsg::SpawnChild, &mut ctx) {
        Effect::Spawn(definition) => {
            assert_eq!(definition.mailbox_capacity(), 4);
            let (_child, capacity, initial_message) = definition.into_parts();
            assert_eq!(capacity, 4);
            assert_eq!(initial_message, Some(ChildMsg::Start));
        }
        other => panic!("expected spawn effect, got {other:?}"),
    }

    assert!(matches!(
        session.handle(SessionMsg::StopNow, &mut ctx),
        Effect::Stop
    ));
}

#[test]
fn preferred_surface_prelude_and_boxed_mailbox_work_together() {
    let mailbox: Box<dyn Mailbox<u8>> = Box::new(LocalMailbox::new(2));
    assert_eq!(mailbox.capacity(), 2);
    mailbox.try_send(7).unwrap();
    mailbox.try_send(9).unwrap();
    assert_eq!(mailbox.recv(), Some(7));
    assert_eq!(mailbox.recv(), Some(9));
    mailbox.close();
    assert_eq!(mailbox.try_send(11), Err(TrySendError::Closed(11)));
}
