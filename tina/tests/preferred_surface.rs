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
enum AuditEvent {
    Recorded(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Message {
    Note(String),
    SnapshotRequested,
    StartWorker,
    StopNow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChildEvent {
    Begin,
}

#[derive(Debug)]
struct ChildWorker;

impl Isolate for ChildWorker {
    tina::isolate_types! {
        message: ChildEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: InlineShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ChildEvent::Begin => stop(),
        }
    }
}

#[derive(Debug)]
struct Session {
    notes: Vec<String>,
    audit: Address<AuditEvent>,
}

impl Isolate for Session {
    tina::isolate_types! {
        message: Message,
        reply: Vec<String>,
        send: Outbound<AuditEvent>,
        spawn: ChildDefinition<ChildWorker>,
        call: Infallible,
        shard: InlineShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            Message::Note(note) => {
                self.notes.push(note.clone());
                send(self.audit, AuditEvent::Recorded(note))
            }
            Message::SnapshotRequested => reply(self.notes.clone()),
            Message::StartWorker => {
                spawn(ChildDefinition::new(ChildWorker, 4).with_initial_message(ChildEvent::Begin))
            }
            Message::StopNow => stop(),
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

    match session.handle(Message::Note("hello".to_owned()), &mut ctx) {
        Effect::Send(outbound) => {
            assert_eq!(outbound.destination(), audit);
            assert_eq!(
                outbound.message(),
                &AuditEvent::Recorded("hello".to_owned())
            );
        }
        other => panic!("expected send effect, got {other:?}"),
    }

    assert!(matches!(
        session.handle(Message::SnapshotRequested, &mut ctx),
        Effect::Reply(ref notes) if notes == &vec!["hello".to_owned()]
    ));

    match session.handle(Message::StartWorker, &mut ctx) {
        Effect::Spawn(definition) => {
            assert_eq!(definition.mailbox_capacity(), 4);
            let (_child, capacity, initial_message) = definition.into_parts();
            assert_eq!(capacity, 4);
            assert_eq!(initial_message, Some(ChildEvent::Begin));
        }
        other => panic!("expected spawn effect, got {other:?}"),
    }

    assert!(matches!(
        session.handle(Message::StopNow, &mut ctx),
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
