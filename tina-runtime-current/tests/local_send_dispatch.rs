use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use tina::{
    Address, Context, Effect, Isolate, IsolateId, Mailbox, SendMessage, Shard, ShardId, SpawnSpec,
    TrySendError,
};
use tina_runtime_current::{
    CauseId, CurrentRuntime, EffectKind, EventId, RuntimeEvent, RuntimeEventKind,
    SendRejectedReason,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditMsg {
    Record(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservedMsg {
    Reply,
    Spawn,
    Restart,
}

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(3)
    }
}

#[derive(Clone)]
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

#[derive(Debug)]
struct Driver {
    target: Address<AuditMsg>,
    handled: Rc<RefCell<Vec<u8>>>,
}

impl Isolate for Driver {
    type Message = DriverMsg;
    type Reply = ();
    type Send = SendMessage<AuditMsg>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            DriverMsg::Kick(value) => {
                self.handled.borrow_mut().push(value);
                Effect::Send(SendMessage::new(self.target, AuditMsg::Record(value)))
            }
        }
    }
}

#[derive(Debug)]
struct Audit {
    seen: Rc<RefCell<Vec<u8>>>,
}

impl Isolate for Audit {
    type Message = AuditMsg;
    type Reply = ();
    type Send = SendMessage<NeverOutbound>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        let AuditMsg::Record(value) = msg;
        self.seen.borrow_mut().push(value);
        Effect::Noop
    }
}

#[derive(Debug)]
struct Worker;

impl Isolate for Worker {
    type Message = ();
    type Reply = ();
    type Send = SendMessage<NeverOutbound>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        Effect::Noop
    }
}

#[derive(Debug)]
struct ObservedIsolate;

impl Isolate for ObservedIsolate {
    type Message = ObservedMsg;
    type Reply = u8;
    type Send = SendMessage<NeverOutbound>;
    type Spawn = SpawnSpec<Worker>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ObservedMsg::Reply => Effect::Reply(7),
            ObservedMsg::Spawn => Effect::Spawn(SpawnSpec::new(Worker, 2)),
            ObservedMsg::Restart => Effect::RestartChildren,
        }
    }
}

#[test]
fn registration_returns_typed_addresses_and_step_uses_registration_order() {
    let mut runtime = CurrentRuntime::new(TestShard);
    let log = Rc::new(RefCell::new(Vec::new()));

    let alpha_mailbox = TestMailbox::new(8);
    let beta_mailbox = TestMailbox::new(8);
    let gamma_mailbox = TestMailbox::new(8);

    let alpha = runtime.register(
        OrderIsolate {
            name: "alpha",
            log: Rc::clone(&log),
        },
        alpha_mailbox.clone(),
    );
    let beta = runtime.register(
        OrderIsolate {
            name: "beta",
            log: Rc::clone(&log),
        },
        beta_mailbox.clone(),
    );
    let gamma = runtime.register(
        OrderIsolate {
            name: "gamma",
            log: Rc::clone(&log),
        },
        gamma_mailbox.clone(),
    );

    assert_eq!(alpha.shard(), ShardId::new(3));
    assert_eq!(alpha.isolate(), IsolateId::new(1));
    assert_eq!(beta.isolate(), IsolateId::new(2));
    assert_eq!(gamma.isolate(), IsolateId::new(3));

    assert_eq!(alpha_mailbox.try_send(OrderMsg::Tick), Ok(()));
    assert_eq!(alpha_mailbox.try_send(OrderMsg::Tick), Ok(()));
    assert_eq!(beta_mailbox.try_send(OrderMsg::Tick), Ok(()));
    assert_eq!(gamma_mailbox.try_send(OrderMsg::Tick), Ok(()));

    assert_eq!(runtime.step(), 3);
    assert_eq!(*log.borrow(), vec!["alpha", "beta", "gamma"]);

    assert_eq!(runtime.step(), 1);
    assert_eq!(*log.borrow(), vec!["alpha", "beta", "gamma", "alpha"]);
}

#[test]
fn accepted_local_send_runs_target_on_a_later_step_and_records_trace() {
    let mut runtime = CurrentRuntime::new(TestShard);
    let driver_handled = Rc::new(RefCell::new(Vec::new()));
    let audit_seen = Rc::new(RefCell::new(Vec::new()));

    let audit_mailbox = TestMailbox::new(8);
    let audit_address = runtime.register(
        Audit {
            seen: Rc::clone(&audit_seen),
        },
        audit_mailbox.clone(),
    );

    let driver_mailbox = TestMailbox::new(8);
    let driver_address = runtime.register(
        Driver {
            target: audit_address,
            handled: Rc::clone(&driver_handled),
        },
        driver_mailbox.clone(),
    );

    assert_eq!(driver_mailbox.try_send(DriverMsg::Kick(7)), Ok(()));

    assert_eq!(runtime.step(), 1);
    assert_eq!(*driver_handled.borrow(), vec![7]);
    assert!(audit_seen.borrow().is_empty());

    assert_eq!(runtime.step(), 1);
    assert_eq!(*audit_seen.borrow(), vec![7]);

    assert_eq!(
        runtime.trace(),
        [
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(3),
                driver_address.isolate(),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(2),
                Some(CauseId::new(EventId::new(1))),
                ShardId::new(3),
                driver_address.isolate(),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(3),
                Some(CauseId::new(EventId::new(2))),
                ShardId::new(3),
                driver_address.isolate(),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Send,
                },
            ),
            RuntimeEvent::new(
                EventId::new(4),
                Some(CauseId::new(EventId::new(3))),
                ShardId::new(3),
                driver_address.isolate(),
                RuntimeEventKind::SendDispatchAttempted {
                    target_shard: ShardId::new(3),
                    target_isolate: audit_address.isolate(),
                },
            ),
            RuntimeEvent::new(
                EventId::new(5),
                Some(CauseId::new(EventId::new(4))),
                ShardId::new(3),
                driver_address.isolate(),
                RuntimeEventKind::SendAccepted {
                    target_shard: ShardId::new(3),
                    target_isolate: audit_address.isolate(),
                },
            ),
            RuntimeEvent::new(
                EventId::new(6),
                None,
                ShardId::new(3),
                audit_address.isolate(),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(7),
                Some(CauseId::new(EventId::new(6))),
                ShardId::new(3),
                audit_address.isolate(),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(8),
                Some(CauseId::new(EventId::new(7))),
                ShardId::new(3),
                audit_address.isolate(),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Noop,
                },
            ),
            RuntimeEvent::new(
                EventId::new(9),
                Some(CauseId::new(EventId::new(8))),
                ShardId::new(3),
                audit_address.isolate(),
                RuntimeEventKind::EffectObserved {
                    effect: EffectKind::Noop,
                },
            ),
        ]
    );
}

#[test]
fn rejected_local_send_is_traced_and_not_silently_buffered() {
    let mut runtime = CurrentRuntime::new(TestShard);
    let audit_seen = Rc::new(RefCell::new(Vec::new()));

    let audit_mailbox = TestMailbox::new(1);
    let audit_address = runtime.register(
        Audit {
            seen: Rc::clone(&audit_seen),
        },
        audit_mailbox.clone(),
    );

    let first_driver_mailbox = TestMailbox::new(8);
    runtime.register(
        Driver {
            target: audit_address,
            handled: Rc::new(RefCell::new(Vec::new())),
        },
        first_driver_mailbox.clone(),
    );

    let second_driver_mailbox = TestMailbox::new(8);
    let second_driver_address = runtime.register(
        Driver {
            target: audit_address,
            handled: Rc::new(RefCell::new(Vec::new())),
        },
        second_driver_mailbox.clone(),
    );

    assert_eq!(first_driver_mailbox.try_send(DriverMsg::Kick(7)), Ok(()));
    assert_eq!(second_driver_mailbox.try_send(DriverMsg::Kick(8)), Ok(()));

    assert_eq!(runtime.step(), 2);
    assert_eq!(
        runtime.trace().last().copied(),
        Some(RuntimeEvent::new(
            EventId::new(10),
            Some(CauseId::new(EventId::new(9))),
            ShardId::new(3),
            second_driver_address.isolate(),
            RuntimeEventKind::SendRejected {
                target_shard: ShardId::new(3),
                target_isolate: audit_address.isolate(),
                reason: SendRejectedReason::Full,
            },
        ))
    );

    assert_eq!(runtime.step(), 1);
    assert_eq!(*audit_seen.borrow(), vec![7]);
    assert_eq!(runtime.step(), 0);
    assert_eq!(*audit_seen.borrow(), vec![7]);
}

#[test]
fn send_to_unknown_isolate_panics() {
    let mut runtime = CurrentRuntime::new(TestShard);
    let driver_mailbox = TestMailbox::new(8);

    runtime.register(
        Driver {
            target: Address::new(ShardId::new(3), IsolateId::new(99)),
            handled: Rc::new(RefCell::new(Vec::new())),
        },
        driver_mailbox.clone(),
    );

    assert_eq!(driver_mailbox.try_send(DriverMsg::Kick(1)), Ok(()));

    let result = catch_unwind(AssertUnwindSafe(|| runtime.step()));
    assert!(result.is_err());
}

#[test]
fn reply_spawn_and_restart_children_remain_observed_and_not_executed() {
    let cases = [
        (ObservedMsg::Reply, EffectKind::Reply),
        (ObservedMsg::Spawn, EffectKind::Spawn),
        (ObservedMsg::Restart, EffectKind::RestartChildren),
    ];

    for (message, expected_effect) in cases {
        let mut runtime = CurrentRuntime::new(TestShard);
        let mailbox = TestMailbox::new(8);
        let address = runtime.register(ObservedIsolate, mailbox.clone());

        assert_eq!(mailbox.try_send(message), Ok(()));

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
                        effect: expected_effect,
                    },
                ),
                RuntimeEvent::new(
                    EventId::new(4),
                    Some(CauseId::new(EventId::new(3))),
                    ShardId::new(3),
                    address.isolate(),
                    RuntimeEventKind::EffectObserved {
                        effect: expected_effect,
                    },
                ),
            ]
        );
    }
}

#[test]
fn identical_runs_produce_identical_event_sequences_and_causal_links() {
    fn run_once() -> Vec<RuntimeEvent> {
        let mut runtime = CurrentRuntime::new(TestShard);
        let audit_mailbox = TestMailbox::new(8);
        let audit_address = runtime.register(
            Audit {
                seen: Rc::new(RefCell::new(Vec::new())),
            },
            audit_mailbox,
        );

        let driver_mailbox = TestMailbox::new(8);
        runtime.register(
            Driver {
                target: audit_address,
                handled: Rc::new(RefCell::new(Vec::new())),
            },
            driver_mailbox.clone(),
        );

        assert_eq!(driver_mailbox.try_send(DriverMsg::Kick(5)), Ok(()));
        assert_eq!(runtime.step(), 1);
        assert_eq!(runtime.step(), 1);

        runtime.trace().to_vec()
    }

    assert_eq!(run_once(), run_once());
}
