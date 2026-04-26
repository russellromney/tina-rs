use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;

use tina::{
    Address, Context, Effect, Isolate, IsolateId, Mailbox, SendMessage, Shard, ShardId,
    TrySendError,
};
use tina_runtime_current::{
    CauseId, CurrentRuntime, EffectKind, EventId, MailboxFactory, RuntimeEvent, RuntimeEventKind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeverOutbound {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderMsg {
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverMsg {
    Kick(u8),
}

#[derive(Debug)]
struct DropTracker {
    value: u8,
    drops: Rc<Cell<usize>>,
}

impl DropTracker {
    fn new(value: u8, drops: Rc<Cell<usize>>) -> Self {
        Self { value, drops }
    }
}

impl Drop for DropTracker {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

#[derive(Debug)]
enum StopMsg {
    Stop,
    Payload(DropTracker),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopAuditMsg {
    Stop,
    Record(u8),
}

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(3)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> Clone for TestMailbox<T> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            queue: Rc::clone(&self.queue),
            closed: Rc::clone(&self.closed),
        }
    }
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

#[derive(Debug)]
struct OrderIsolate {
    name: &'static str,
    log: Rc<RefCell<Vec<&'static str>>>,
}

impl Isolate for OrderIsolate {
    type Message = OrderMsg;
    type Reply = ();
    type Send = SendMessage<NeverOutbound>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        self.log.borrow_mut().push(self.name);
        Effect::Noop
    }
}

#[derive(Debug, Default)]
struct StopIsolate {
    handled_payloads: usize,
}

impl Isolate for StopIsolate {
    type Message = StopMsg;
    type Reply = ();
    type Send = SendMessage<NeverOutbound>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            StopMsg::Stop => Effect::Stop,
            StopMsg::Payload(payload) => {
                let _value = payload.value;
                self.handled_payloads += 1;
                Effect::Noop
            }
        }
    }
}

#[derive(Debug)]
struct StopAndAudit {
    seen: Rc<RefCell<Vec<u8>>>,
}

impl Isolate for StopAndAudit {
    type Message = StopAuditMsg;
    type Reply = ();
    type Send = SendMessage<NeverOutbound>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            StopAuditMsg::Stop => Effect::Stop,
            StopAuditMsg::Record(value) => {
                self.seen.borrow_mut().push(value);
                Effect::Noop
            }
        }
    }
}

#[derive(Debug)]
struct StopSender {
    target: Address<StopAuditMsg>,
}

impl Isolate for StopSender {
    type Message = DriverMsg;
    type Reply = ();
    type Send = SendMessage<StopAuditMsg>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            DriverMsg::Kick(value) => {
                Effect::Send(SendMessage::new(self.target, StopAuditMsg::Record(value)))
            }
        }
    }
}

#[test]
fn stop_abandons_buffered_messages_in_fifo_order_and_empties_the_mailbox() {
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
    let drops = Rc::new(Cell::new(0));
    let mailbox = TestMailbox::new(8);
    let address = runtime.register(StopIsolate::default(), mailbox.clone());

    assert!(matches!(mailbox.try_send(StopMsg::Stop), Ok(())));
    assert!(matches!(
        mailbox.try_send(StopMsg::Payload(DropTracker::new(1, Rc::clone(&drops)))),
        Ok(())
    ));
    assert!(matches!(
        mailbox.try_send(StopMsg::Payload(DropTracker::new(2, Rc::clone(&drops)))),
        Ok(())
    ));

    assert_eq!(runtime.step(), 1);
    assert_eq!(drops.get(), 2);
    assert_eq!(runtime.step(), 0);
    assert!(mailbox.recv().is_none());

    assert_eq!(
        runtime.trace(),
        [
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(3),
                address.isolate(),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(2),
                Some(CauseId::new(EventId::new(1))),
                ShardId::new(3),
                address.isolate(),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(3),
                Some(CauseId::new(EventId::new(2))),
                ShardId::new(3),
                address.isolate(),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Stop,
                },
            ),
            RuntimeEvent::new(
                EventId::new(4),
                Some(CauseId::new(EventId::new(3))),
                ShardId::new(3),
                address.isolate(),
                RuntimeEventKind::IsolateStopped,
            ),
            RuntimeEvent::new(
                EventId::new(5),
                Some(CauseId::new(EventId::new(4))),
                ShardId::new(3),
                address.isolate(),
                RuntimeEventKind::MessageAbandoned,
            ),
            RuntimeEvent::new(
                EventId::new(6),
                Some(CauseId::new(EventId::new(4))),
                ShardId::new(3),
                address.isolate(),
                RuntimeEventKind::MessageAbandoned,
            ),
        ]
    );
}

#[test]
fn stop_with_empty_mailbox_produces_no_message_abandoned_events() {
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
    let mailbox = TestMailbox::new(8);
    let address = runtime.register(StopIsolate::default(), mailbox.clone());

    assert!(matches!(mailbox.try_send(StopMsg::Stop), Ok(())));

    assert_eq!(runtime.step(), 1);
    assert_eq!(
        runtime.trace(),
        [
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(3),
                address.isolate(),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(2),
                Some(CauseId::new(EventId::new(1))),
                ShardId::new(3),
                address.isolate(),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(3),
                Some(CauseId::new(EventId::new(2))),
                ShardId::new(3),
                address.isolate(),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Stop,
                },
            ),
            RuntimeEvent::new(
                EventId::new(4),
                Some(CauseId::new(EventId::new(3))),
                ShardId::new(3),
                address.isolate(),
                RuntimeEventKind::IsolateStopped,
            ),
        ]
    );
}

#[test]
fn abandonment_happens_before_later_isolate_handlers_in_the_same_round() {
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
    let drops = Rc::new(Cell::new(0));

    let first_mailbox = TestMailbox::new(8);
    let first = runtime.register(StopIsolate::default(), first_mailbox.clone());
    let second_mailbox = TestMailbox::new(8);
    let second = runtime.register(
        OrderIsolate {
            name: "second",
            log: Rc::new(RefCell::new(Vec::new())),
        },
        second_mailbox.clone(),
    );

    assert!(matches!(first_mailbox.try_send(StopMsg::Stop), Ok(())));
    assert!(matches!(
        first_mailbox.try_send(StopMsg::Payload(DropTracker::new(1, Rc::clone(&drops)))),
        Ok(())
    ));
    assert_eq!(second_mailbox.try_send(OrderMsg::Tick), Ok(()));

    assert_eq!(runtime.step(), 2);
    assert_eq!(drops.get(), 1);

    let trace = runtime.trace();
    let stop_index = trace
        .iter()
        .position(|event| {
            event.isolate() == first.isolate() && event.kind() == RuntimeEventKind::IsolateStopped
        })
        .unwrap();
    let abandoned_index = trace
        .iter()
        .position(|event| {
            event.isolate() == first.isolate() && event.kind() == RuntimeEventKind::MessageAbandoned
        })
        .unwrap();
    let second_mailbox_index = trace
        .iter()
        .position(|event| {
            event.isolate() == second.isolate() && event.kind() == RuntimeEventKind::MailboxAccepted
        })
        .unwrap();

    assert!(stop_index < abandoned_index);
    assert!(abandoned_index < second_mailbox_index);
}

#[test]
fn two_stops_in_one_round_interleave_abandonment_by_registration_order() {
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
    let first_drops = Rc::new(Cell::new(0));
    let second_drops = Rc::new(Cell::new(0));

    let first_mailbox = TestMailbox::new(8);
    let first = runtime.register(StopIsolate::default(), first_mailbox.clone());
    let second_mailbox = TestMailbox::new(8);
    let second = runtime.register(StopIsolate::default(), second_mailbox.clone());

    assert!(matches!(first_mailbox.try_send(StopMsg::Stop), Ok(())));
    assert!(matches!(
        first_mailbox.try_send(StopMsg::Payload(DropTracker::new(
            1,
            Rc::clone(&first_drops)
        ))),
        Ok(())
    ));
    assert!(matches!(second_mailbox.try_send(StopMsg::Stop), Ok(())));
    assert!(matches!(
        second_mailbox.try_send(StopMsg::Payload(DropTracker::new(
            2,
            Rc::clone(&second_drops)
        ))),
        Ok(())
    ));

    assert_eq!(runtime.step(), 2);
    assert_eq!(first_drops.get(), 1);
    assert_eq!(second_drops.get(), 1);

    assert_eq!(
        runtime.trace(),
        [
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(3),
                first.isolate(),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(2),
                Some(CauseId::new(EventId::new(1))),
                ShardId::new(3),
                first.isolate(),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(3),
                Some(CauseId::new(EventId::new(2))),
                ShardId::new(3),
                first.isolate(),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Stop,
                },
            ),
            RuntimeEvent::new(
                EventId::new(4),
                Some(CauseId::new(EventId::new(3))),
                ShardId::new(3),
                first.isolate(),
                RuntimeEventKind::IsolateStopped,
            ),
            RuntimeEvent::new(
                EventId::new(5),
                Some(CauseId::new(EventId::new(4))),
                ShardId::new(3),
                first.isolate(),
                RuntimeEventKind::MessageAbandoned,
            ),
            RuntimeEvent::new(
                EventId::new(6),
                None,
                ShardId::new(3),
                second.isolate(),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(7),
                Some(CauseId::new(EventId::new(6))),
                ShardId::new(3),
                second.isolate(),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(8),
                Some(CauseId::new(EventId::new(7))),
                ShardId::new(3),
                second.isolate(),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Stop,
                },
            ),
            RuntimeEvent::new(
                EventId::new(9),
                Some(CauseId::new(EventId::new(8))),
                ShardId::new(3),
                second.isolate(),
                RuntimeEventKind::IsolateStopped,
            ),
            RuntimeEvent::new(
                EventId::new(10),
                Some(CauseId::new(EventId::new(9))),
                ShardId::new(3),
                second.isolate(),
                RuntimeEventKind::MessageAbandoned,
            ),
        ]
    );
}

#[test]
fn accepted_send_can_become_message_abandoned_when_target_stops_in_the_same_round() {
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
    let target_seen = Rc::new(RefCell::new(Vec::new()));

    let sender_mailbox = TestMailbox::new(8);
    let sender_address = runtime.register(
        StopSender {
            target: Address::new(ShardId::new(3), IsolateId::new(2)),
        },
        sender_mailbox.clone(),
    );
    let target_mailbox = TestMailbox::new(8);
    let target_address = runtime.register(
        StopAndAudit {
            seen: Rc::clone(&target_seen),
        },
        target_mailbox.clone(),
    );

    assert_eq!(target_address.isolate(), IsolateId::new(2));
    assert_eq!(sender_mailbox.try_send(DriverMsg::Kick(7)), Ok(()));
    assert_eq!(target_mailbox.try_send(StopAuditMsg::Stop), Ok(()));

    assert_eq!(runtime.step(), 2);
    assert!(target_seen.borrow().is_empty());

    assert_eq!(
        runtime.trace(),
        [
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(3),
                sender_address.isolate(),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(2),
                Some(CauseId::new(EventId::new(1))),
                ShardId::new(3),
                sender_address.isolate(),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(3),
                Some(CauseId::new(EventId::new(2))),
                ShardId::new(3),
                sender_address.isolate(),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Send,
                },
            ),
            RuntimeEvent::new(
                EventId::new(4),
                Some(CauseId::new(EventId::new(3))),
                ShardId::new(3),
                sender_address.isolate(),
                RuntimeEventKind::SendDispatchAttempted {
                    target_shard: ShardId::new(3),
                    target_isolate: target_address.isolate(),
                },
            ),
            RuntimeEvent::new(
                EventId::new(5),
                Some(CauseId::new(EventId::new(4))),
                ShardId::new(3),
                sender_address.isolate(),
                RuntimeEventKind::SendAccepted {
                    target_shard: ShardId::new(3),
                    target_isolate: target_address.isolate(),
                },
            ),
            RuntimeEvent::new(
                EventId::new(6),
                None,
                ShardId::new(3),
                target_address.isolate(),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(7),
                Some(CauseId::new(EventId::new(6))),
                ShardId::new(3),
                target_address.isolate(),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(8),
                Some(CauseId::new(EventId::new(7))),
                ShardId::new(3),
                target_address.isolate(),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Stop,
                },
            ),
            RuntimeEvent::new(
                EventId::new(9),
                Some(CauseId::new(EventId::new(8))),
                ShardId::new(3),
                target_address.isolate(),
                RuntimeEventKind::IsolateStopped,
            ),
            RuntimeEvent::new(
                EventId::new(10),
                Some(CauseId::new(EventId::new(9))),
                ShardId::new(3),
                target_address.isolate(),
                RuntimeEventKind::MessageAbandoned,
            ),
        ]
    );
}

#[test]
fn sends_after_stop_still_become_closed() {
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
    let target_seen = Rc::new(RefCell::new(Vec::new()));

    let target_mailbox = TestMailbox::new(8);
    let target_address = runtime.register(
        StopAndAudit {
            seen: Rc::clone(&target_seen),
        },
        target_mailbox.clone(),
    );
    let sender_mailbox = TestMailbox::new(8);
    let sender_address = runtime.register(
        StopSender {
            target: target_address,
        },
        sender_mailbox.clone(),
    );

    assert_eq!(target_mailbox.try_send(StopAuditMsg::Stop), Ok(()));
    assert_eq!(runtime.step(), 1);
    assert!(target_seen.borrow().is_empty());

    assert_eq!(sender_mailbox.try_send(DriverMsg::Kick(7)), Ok(()));

    assert_eq!(runtime.step(), 1);
    assert_eq!(
        runtime.trace().last().copied(),
        Some(RuntimeEvent::new(
            EventId::new(9),
            Some(CauseId::new(EventId::new(8))),
            ShardId::new(3),
            sender_address.isolate(),
            RuntimeEventKind::SendRejected {
                target_shard: ShardId::new(3),
                target_isolate: target_address.isolate(),
                reason: tina_runtime_current::SendRejectedReason::Closed,
            },
        ))
    );
}

#[test]
fn repeated_runs_with_abandonment_produce_identical_traces() {
    fn run_once() -> Vec<RuntimeEvent> {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let drops = Rc::new(Cell::new(0));
        let mailbox = TestMailbox::new(8);
        runtime.register(StopIsolate::default(), mailbox.clone());

        assert!(matches!(mailbox.try_send(StopMsg::Stop), Ok(())));
        assert!(matches!(
            mailbox.try_send(StopMsg::Payload(DropTracker::new(1, Rc::clone(&drops)))),
            Ok(())
        ));
        assert!(matches!(
            mailbox.try_send(StopMsg::Payload(DropTracker::new(2, Rc::clone(&drops)))),
            Ok(())
        ));

        assert_eq!(runtime.step(), 1);
        assert_eq!(drops.get(), 2);

        runtime.trace().to_vec()
    }

    assert_eq!(run_once(), run_once());
}
