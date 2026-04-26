use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use tina::{
    Address, Context, Effect, Isolate, IsolateId, Mailbox, SendMessage, Shard, ShardId,
    TrySendError,
};
use tina_runtime_current::{
    CauseId, CurrentRuntime, EventId, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeverOutbound {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverMsg {
    Kick(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderMsg {
    Tick,
}

#[derive(Debug)]
struct DropTracker {
    value: u8,
    dropped: Rc<RefCell<Vec<u8>>>,
}

impl DropTracker {
    fn new(value: u8, dropped: Rc<RefCell<Vec<u8>>>) -> Self {
        Self { value, dropped }
    }
}

impl Drop for DropTracker {
    fn drop(&mut self) {
        self.dropped.borrow_mut().push(self.value);
    }
}

#[derive(Debug)]
enum PanicMsg {
    Panic,
    Payload(DropTracker),
    Data(u8),
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

#[derive(Debug)]
struct PanicIsolate {
    handled: Rc<RefCell<Vec<u8>>>,
}

impl Isolate for PanicIsolate {
    type Message = PanicMsg;
    type Reply = ();
    type Send = SendMessage<NeverOutbound>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            PanicMsg::Panic => panic!("panic inside handler"),
            PanicMsg::Payload(payload) => {
                self.handled.borrow_mut().push(payload.value);
                Effect::Noop
            }
            PanicMsg::Data(value) => {
                self.handled.borrow_mut().push(value);
                Effect::Noop
            }
        }
    }
}

#[derive(Debug)]
struct PanicSender {
    target: Address<PanicMsg>,
}

impl Isolate for PanicSender {
    type Message = DriverMsg;
    type Reply = ();
    type Send = SendMessage<PanicMsg>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            DriverMsg::Kick(value) => {
                Effect::Send(SendMessage::new(self.target, PanicMsg::Data(value)))
            }
        }
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

#[test]
fn panicking_handler_becomes_runtime_event_and_stops_isolate() {
    let mut runtime = CurrentRuntime::new(TestShard);
    let handled = Rc::new(RefCell::new(Vec::new()));
    let mailbox = TestMailbox::new(8);
    let address = runtime.register(
        PanicIsolate {
            handled: Rc::clone(&handled),
        },
        mailbox.clone(),
    );

    assert!(matches!(mailbox.try_send(PanicMsg::Panic), Ok(())));

    assert_eq!(runtime.step(), 1);
    assert!(handled.borrow().is_empty());
    assert_eq!(runtime.step(), 0);

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
                RuntimeEventKind::HandlerPanicked,
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
fn panic_abandons_buffered_messages_in_fifo_order() {
    let mut runtime = CurrentRuntime::new(TestShard);
    let handled = Rc::new(RefCell::new(Vec::new()));
    let dropped = Rc::new(RefCell::new(Vec::new()));
    let mailbox = TestMailbox::new(8);
    let address = runtime.register(
        PanicIsolate {
            handled: Rc::clone(&handled),
        },
        mailbox.clone(),
    );

    assert!(matches!(mailbox.try_send(PanicMsg::Panic), Ok(())));
    assert!(matches!(
        mailbox.try_send(PanicMsg::Payload(DropTracker::new(1, Rc::clone(&dropped)))),
        Ok(())
    ));
    assert!(matches!(
        mailbox.try_send(PanicMsg::Payload(DropTracker::new(2, Rc::clone(&dropped)))),
        Ok(())
    ));

    assert_eq!(runtime.step(), 1);
    assert!(handled.borrow().is_empty());
    assert_eq!(*dropped.borrow(), vec![1, 2]);
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
                RuntimeEventKind::HandlerPanicked,
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
fn later_isolates_still_run_after_panic_in_same_round() {
    let mut runtime = CurrentRuntime::new(TestShard);
    let handled = Rc::new(RefCell::new(Vec::new()));
    let dropped = Rc::new(RefCell::new(Vec::new()));
    let panic_mailbox = TestMailbox::new(8);
    let panic_address = runtime.register(
        PanicIsolate {
            handled: Rc::clone(&handled),
        },
        panic_mailbox.clone(),
    );

    let log = Rc::new(RefCell::new(Vec::new()));
    let follower_mailbox = TestMailbox::new(8);
    let follower_address = runtime.register(
        OrderIsolate {
            name: "follower",
            log: Rc::clone(&log),
        },
        follower_mailbox.clone(),
    );

    assert!(matches!(panic_mailbox.try_send(PanicMsg::Panic), Ok(())));
    assert!(matches!(
        panic_mailbox.try_send(PanicMsg::Payload(DropTracker::new(7, Rc::clone(&dropped)))),
        Ok(())
    ));
    assert_eq!(follower_mailbox.try_send(OrderMsg::Tick), Ok(()));

    assert_eq!(runtime.step(), 2);
    assert!(handled.borrow().is_empty());
    assert_eq!(*dropped.borrow(), vec![7]);
    assert_eq!(*log.borrow(), vec!["follower"]);

    let trace = runtime.trace();
    let stop_index = trace
        .iter()
        .position(|event| {
            event.isolate() == panic_address.isolate()
                && event.kind() == RuntimeEventKind::IsolateStopped
        })
        .unwrap();
    let abandoned_index = trace
        .iter()
        .position(|event| {
            event.isolate() == panic_address.isolate()
                && event.kind() == RuntimeEventKind::MessageAbandoned
        })
        .unwrap();
    let follower_index = trace
        .iter()
        .position(|event| {
            event.isolate() == follower_address.isolate()
                && event.kind() == RuntimeEventKind::MailboxAccepted
        })
        .unwrap();

    assert!(stop_index < abandoned_index);
    assert!(abandoned_index < follower_index);
}

#[test]
fn two_panics_in_one_round_interleave_abandonment_by_registration_order() {
    let mut runtime = CurrentRuntime::new(TestShard);
    let first_handled = Rc::new(RefCell::new(Vec::new()));
    let second_handled = Rc::new(RefCell::new(Vec::new()));
    let dropped = Rc::new(RefCell::new(Vec::new()));

    let first_mailbox = TestMailbox::new(8);
    let first = runtime.register(
        PanicIsolate {
            handled: Rc::clone(&first_handled),
        },
        first_mailbox.clone(),
    );

    let second_mailbox = TestMailbox::new(8);
    let second = runtime.register(
        PanicIsolate {
            handled: Rc::clone(&second_handled),
        },
        second_mailbox.clone(),
    );

    assert!(matches!(first_mailbox.try_send(PanicMsg::Panic), Ok(())));
    assert!(matches!(
        first_mailbox.try_send(PanicMsg::Payload(DropTracker::new(1, Rc::clone(&dropped)))),
        Ok(())
    ));
    assert!(matches!(second_mailbox.try_send(PanicMsg::Panic), Ok(())));
    assert!(matches!(
        second_mailbox.try_send(PanicMsg::Payload(DropTracker::new(2, Rc::clone(&dropped)))),
        Ok(())
    ));

    assert_eq!(runtime.step(), 2);
    assert!(first_handled.borrow().is_empty());
    assert!(second_handled.borrow().is_empty());
    assert_eq!(*dropped.borrow(), vec![1, 2]);

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
                RuntimeEventKind::HandlerPanicked,
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
                RuntimeEventKind::HandlerPanicked,
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
fn sends_after_panic_still_become_closed() {
    let mut runtime = CurrentRuntime::new(TestShard);
    let handled = Rc::new(RefCell::new(Vec::new()));

    let target_mailbox = TestMailbox::new(8);
    let target_address = runtime.register(
        PanicIsolate {
            handled: Rc::clone(&handled),
        },
        target_mailbox.clone(),
    );

    let sender_mailbox = TestMailbox::new(8);
    let sender_address = runtime.register(
        PanicSender {
            target: target_address,
        },
        sender_mailbox.clone(),
    );

    assert!(matches!(target_mailbox.try_send(PanicMsg::Panic), Ok(())));
    assert_eq!(runtime.step(), 1);
    assert_eq!(sender_mailbox.try_send(DriverMsg::Kick(4)), Ok(()));

    assert_eq!(runtime.step(), 1);
    assert!(handled.borrow().is_empty());

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
                reason: SendRejectedReason::Closed,
            },
        ))
    );
}

#[test]
fn unknown_target_send_still_panics_instead_of_becoming_handler_panicked() {
    let mut runtime = CurrentRuntime::new(TestShard);
    let sender_mailbox = TestMailbox::new(8);
    let sender_address = runtime.register(
        PanicSender {
            target: Address::new(ShardId::new(3), IsolateId::new(99)),
        },
        sender_mailbox.clone(),
    );

    assert_eq!(sender_mailbox.try_send(DriverMsg::Kick(1)), Ok(()));

    let result = catch_unwind(AssertUnwindSafe(|| runtime.step()));
    assert!(result.is_err());
    assert!(
        !runtime
            .trace()
            .iter()
            .any(|event| event.kind() == RuntimeEventKind::HandlerPanicked)
    );
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
                    effect: tina_runtime_current::EffectKind::Send,
                },
            ),
            RuntimeEvent::new(
                EventId::new(4),
                Some(CauseId::new(EventId::new(3))),
                ShardId::new(3),
                sender_address.isolate(),
                RuntimeEventKind::SendDispatchAttempted {
                    target_shard: ShardId::new(3),
                    target_isolate: IsolateId::new(99),
                },
            ),
        ]
    );
}

#[test]
fn repeated_runs_with_panic_events_produce_identical_traces() {
    fn run_once() -> Vec<RuntimeEvent> {
        let mut runtime = CurrentRuntime::new(TestShard);
        let handled = Rc::new(RefCell::new(Vec::new()));
        let dropped = Rc::new(RefCell::new(Vec::new()));

        let panic_mailbox = TestMailbox::new(8);
        let panic_address = runtime.register(
            PanicIsolate {
                handled: Rc::clone(&handled),
            },
            panic_mailbox.clone(),
        );

        let follower_mailbox = TestMailbox::new(8);
        let follower_address = runtime.register(
            OrderIsolate {
                name: "follower",
                log: Rc::new(RefCell::new(Vec::new())),
            },
            follower_mailbox.clone(),
        );

        assert!(matches!(panic_mailbox.try_send(PanicMsg::Panic), Ok(())));
        assert!(matches!(
            panic_mailbox.try_send(PanicMsg::Payload(DropTracker::new(8, Rc::clone(&dropped)))),
            Ok(())
        ));
        assert_eq!(follower_mailbox.try_send(OrderMsg::Tick), Ok(()));

        assert_eq!(runtime.step(), 2);
        assert_eq!(runtime.step(), 0);
        assert!(handled.borrow().is_empty());
        assert_eq!(*dropped.borrow(), vec![8]);

        let expected = vec![
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(3),
                panic_address.isolate(),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(2),
                Some(CauseId::new(EventId::new(1))),
                ShardId::new(3),
                panic_address.isolate(),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(3),
                Some(CauseId::new(EventId::new(2))),
                ShardId::new(3),
                panic_address.isolate(),
                RuntimeEventKind::HandlerPanicked,
            ),
            RuntimeEvent::new(
                EventId::new(4),
                Some(CauseId::new(EventId::new(3))),
                ShardId::new(3),
                panic_address.isolate(),
                RuntimeEventKind::IsolateStopped,
            ),
            RuntimeEvent::new(
                EventId::new(5),
                Some(CauseId::new(EventId::new(4))),
                ShardId::new(3),
                panic_address.isolate(),
                RuntimeEventKind::MessageAbandoned,
            ),
            RuntimeEvent::new(
                EventId::new(6),
                None,
                ShardId::new(3),
                follower_address.isolate(),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(7),
                Some(CauseId::new(EventId::new(6))),
                ShardId::new(3),
                follower_address.isolate(),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(8),
                Some(CauseId::new(EventId::new(7))),
                ShardId::new(3),
                follower_address.isolate(),
                RuntimeEventKind::HandlerFinished {
                    effect: tina_runtime_current::EffectKind::Noop,
                },
            ),
            RuntimeEvent::new(
                EventId::new(9),
                Some(CauseId::new(EventId::new(8))),
                ShardId::new(3),
                follower_address.isolate(),
                RuntimeEventKind::EffectObserved {
                    effect: tina_runtime_current::EffectKind::Noop,
                },
            ),
        ];

        assert_eq!(runtime.trace(), expected);
        runtime.trace().to_vec()
    }

    assert_eq!(run_once(), run_once());
}
