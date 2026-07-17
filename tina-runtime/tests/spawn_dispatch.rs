use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use tina::{
    Address, ChildDefinition, Isolate, IsolateId, Mailbox, Outbound, ServiceMessage, Shard,
    ShardId, TrySendError, prelude::*,
};
use tina_runtime::{
    CauseId, EffectKind, EventId, MailboxFactory, Runtime, RuntimeEvent, RuntimeEventKind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeverOutbound {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentEvent {
    StartChild,
}

#[derive(Debug)]
enum ObservedParentEvent {
    StartChild,
    StartInvalidChild,
    ChildStarted(Result<ChildRef<ChildEvent>, SpawnObservedError>),
}

#[derive(Debug)]
enum FullParentEvent {
    Start,
    Fill,
    ChildStarted(Result<ChildRef<ChildEvent>, SpawnObservedError>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildEvent {
    Data(u8),
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderMsg {
    Tick,
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
    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
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
struct Child {
    seen: Rc<RefCell<Vec<u8>>>,
    order_log: Rc<RefCell<Vec<&'static str>>>,
}

impl Isolate for Child {
    tina::isolate_types! {
        message: ChildEvent,
        reply: (),
        send: Outbound<NeverOutbound>,
        spawn: Infallible,
        io: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ChildEvent::Data(value) => {
                self.order_log.borrow_mut().push("child");
                self.seen.borrow_mut().push(value);
                noop()
            }
            ChildEvent::Stop => stop(),
        }
    }
}

#[derive(Debug)]
struct Parent {
    child_seen: Rc<RefCell<Vec<u8>>>,
    order_log: Rc<RefCell<Vec<&'static str>>>,
    child_capacity: usize,
}

#[derive(Debug)]
struct ObservedParent {
    child_seen: Rc<RefCell<Vec<u8>>>,
    order_log: Rc<RefCell<Vec<&'static str>>>,
    child_ref: Rc<RefCell<Option<ChildRef<ChildEvent>>>>,
    spawn_error: Rc<RefCell<Option<SpawnObservedError>>>,
}

#[derive(Debug)]
enum RestartObservedParentEvent {
    Start,
    Restart,
    RestartWithFullMailbox,
    Fill,
    ChildStarted(Result<ChildRef<ChildEvent>, SpawnObservedError>),
    ChildRestarted(ChildRef<ChildEvent>, DropProbe),
}

#[derive(Debug)]
struct DropProbe(Rc<Cell<usize>>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

struct RestartObservedParent {
    child_seen: Rc<RefCell<Vec<u8>>>,
    order_log: Rc<RefCell<Vec<&'static str>>>,
    incarnations: Rc<RefCell<Vec<ChildRef<ChildEvent>>>>,
    factory_calls: Rc<Cell<usize>>,
    panic_on_factory_call: Option<usize>,
    initial_errors: Rc<RefCell<Vec<SpawnObservedError>>>,
    initial_authority_drops: Rc<Cell<usize>>,
    restart_message_drops: Rc<Cell<usize>>,
}

impl Isolate for RestartObservedParent {
    tina::isolate_types! {
        message: ServiceMessage<RestartObservedParentEvent, Infallible>,
        reply: (),
        send: Outbound<ServiceMessage<RestartObservedParentEvent, Infallible>>,
        spawn: tina::RestartableChildDefinition<Child>,
        spawn_observed: tina::SpawnObserved<tina::RestartableChildDefinition<Child>, ServiceMessage<RestartObservedParentEvent, Infallible>, ChildEvent>,
        io: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        message: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        let msg = match message {
            ServiceMessage::Event(event) => event,
            ServiceMessage::Request(request) => match request {},
        };
        match msg {
            RestartObservedParentEvent::Start => {
                let child_seen = Rc::clone(&self.child_seen);
                let order_log = Rc::clone(&self.order_log);
                let factory_calls = Rc::clone(&self.factory_calls);
                let panic_on_factory_call = self.panic_on_factory_call;
                let initial_authority_drops = Rc::clone(&self.initial_authority_drops);
                let restart_message_drops = Rc::clone(&self.restart_message_drops);
                spawn_observed(tina::RestartableChildDefinition::new(
                    move || {
                        let call = factory_calls.get() + 1;
                        factory_calls.set(call);
                        assert_ne!(Some(call), panic_on_factory_call, "factory panic");
                        Child {
                            seen: Rc::clone(&child_seen),
                            order_log: Rc::clone(&order_log),
                        }
                    },
                    4,
                ))
                .then_service_event_with_restarts(
                    {
                        let initial_authority = DropProbe(initial_authority_drops);
                        move |result| {
                            drop(initial_authority);
                            RestartObservedParentEvent::ChildStarted(result)
                        }
                    },
                    move |child| {
                        RestartObservedParentEvent::ChildRestarted(
                            child,
                            DropProbe(Rc::clone(&restart_message_drops)),
                        )
                    },
                )
            }
            RestartObservedParentEvent::Restart => restart_children(),
            RestartObservedParentEvent::RestartWithFullMailbox => batch([
                send(
                    ctx.me(),
                    ServiceMessage::Event(RestartObservedParentEvent::Fill),
                ),
                restart_children(),
            ]),
            RestartObservedParentEvent::Fill => noop(),
            RestartObservedParentEvent::ChildStarted(Ok(child)) => {
                self.incarnations.borrow_mut().push(child);
                noop()
            }
            RestartObservedParentEvent::ChildRestarted(child, message_authority) => {
                self.incarnations.borrow_mut().push(child);
                drop(message_authority);
                noop()
            }
            RestartObservedParentEvent::ChildStarted(Err(error)) => {
                self.initial_errors.borrow_mut().push(error);
                noop()
            }
        }
    }
}

impl Isolate for ObservedParent {
    tina::isolate_types! {
        message: ObservedParentEvent,
        reply: (),
        send: Outbound<ChildEvent>,
        spawn: ChildDefinition<Child>,
        spawn_observed: tina::SpawnObserved<ChildDefinition<Child>, ObservedParentEvent, ChildEvent>,
        io: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ObservedParentEvent::StartChild => spawn_observed(ChildDefinition::new(
                Child {
                    seen: Rc::clone(&self.child_seen),
                    order_log: Rc::clone(&self.order_log),
                },
                4,
            ))
            .then(ObservedParentEvent::ChildStarted),
            ObservedParentEvent::StartInvalidChild => spawn_observed(ChildDefinition::new(
                Child {
                    seen: Rc::clone(&self.child_seen),
                    order_log: Rc::clone(&self.order_log),
                },
                0,
            ))
            .then(ObservedParentEvent::ChildStarted),
            ObservedParentEvent::ChildStarted(Ok(child)) => {
                *self.child_ref.borrow_mut() = Some(child);
                send(child.address, ChildEvent::Data(42))
            }
            ObservedParentEvent::ChildStarted(Err(error)) => {
                *self.spawn_error.borrow_mut() = Some(error);
                noop()
            }
        }
    }
}

#[derive(Debug)]
struct FullParent {
    child_seen: Rc<RefCell<Vec<u8>>>,
    order_log: Rc<RefCell<Vec<&'static str>>>,
    child_started_delivered: Rc<Cell<bool>>,
}

impl Isolate for FullParent {
    type Message = FullParentEvent;
    type Reply = ();
    type Send = Outbound<FullParentEvent>;
    type Spawn = ChildDefinition<Child>;
    type SpawnObserved = tina::SpawnObserved<Self::Spawn, Self::Message, ChildEvent, ()>;
    type Io = Infallible;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FullParentEvent::Start => batch([
                send(ctx.me(), FullParentEvent::Fill),
                spawn_observed(ChildDefinition::new(
                    Child {
                        seen: Rc::clone(&self.child_seen),
                        order_log: Rc::clone(&self.order_log),
                    },
                    4,
                ))
                .then(FullParentEvent::ChildStarted),
            ]),
            FullParentEvent::Fill => noop(),
            FullParentEvent::ChildStarted(result) => {
                let _ = result;
                self.child_started_delivered.set(true);
                noop()
            }
        }
    }
}

impl Isolate for Parent {
    tina::isolate_types! {
        message: ParentEvent,
        reply: (),
        send: Outbound<NeverOutbound>,
        spawn: ChildDefinition<Child>,
        io: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ParentEvent::StartChild => spawn(ChildDefinition::new(
                Child {
                    seen: Rc::clone(&self.child_seen),
                    order_log: Rc::clone(&self.order_log),
                },
                self.child_capacity,
            )),
        }
    }
}

#[derive(Debug)]
struct OrderIsolate {
    name: &'static str,
    log: Rc<RefCell<Vec<&'static str>>>,
}

impl Isolate for OrderIsolate {
    tina::isolate_types! {
        message: OrderMsg,
        reply: (),
        send: Outbound<NeverOutbound>,
        spawn: Infallible,
        io: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        self.log.borrow_mut().push(self.name);
        noop()
    }
}

fn child_address(system: tina::SystemIncarnation, child_isolate: IsolateId) -> Address<ChildEvent> {
    Address::new_in(system, ShardId::new(3), child_isolate)
}

fn spawned_child_isolate(trace: &[RuntimeEvent]) -> IsolateId {
    match trace
        .last()
        .expect("spawn trace should not be empty")
        .kind()
    {
        RuntimeEventKind::Spawned { child_isolate } => child_isolate,
        other => panic!("expected Spawned event, found {other:?}"),
    }
}

#[test]
fn spawn_creates_child_and_records_trace() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let child_seen = Rc::new(RefCell::new(Vec::new()));
    let order_log = Rc::new(RefCell::new(Vec::new()));
    let parent = runtime.register(
        Parent {
            child_seen: Rc::clone(&child_seen),
            order_log,
            child_capacity: 2,
        },
        TestMailbox::new(8),
    );

    assert_eq!(runtime.try_send(parent, ParentEvent::StartChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    assert!(child_seen.borrow().is_empty());

    let child_isolate = spawned_child_isolate(runtime.trace());
    assert_eq!(child_isolate, IsolateId::new(2));

    assert_eq!(
        runtime.trace(),
        [
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(3),
                parent.isolate(),
                RuntimeEventKind::MailboxAccepted,
            ),
            RuntimeEvent::new(
                EventId::new(2),
                Some(CauseId::new(EventId::new(1))),
                ShardId::new(3),
                parent.isolate(),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(3),
                Some(CauseId::new(EventId::new(2))),
                ShardId::new(3),
                parent.isolate(),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Spawn,
                },
            ),
            RuntimeEvent::new(
                EventId::new(4),
                Some(CauseId::new(EventId::new(3))),
                ShardId::new(3),
                parent.isolate(),
                RuntimeEventKind::Spawned { child_isolate },
            ),
        ]
    );
}

#[test]
fn spawn_observed_delivers_typed_child_ref_and_parent_uses_address() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let child_seen = Rc::new(RefCell::new(Vec::new()));
    let order_log = Rc::new(RefCell::new(Vec::new()));
    let child_ref = Rc::new(RefCell::new(None));
    let spawn_error = Rc::new(RefCell::new(None));
    let parent = runtime.register(
        ObservedParent {
            child_seen: Rc::clone(&child_seen),
            order_log: Rc::clone(&order_log),
            child_ref: Rc::clone(&child_ref),
            spawn_error,
        },
        TestMailbox::new(8),
    );

    assert!(
        runtime
            .try_send(parent, ObservedParentEvent::StartChild)
            .is_ok()
    );
    assert_eq!(runtime.step(), 1);
    assert!(child_seen.borrow().is_empty());
    assert!(child_ref.borrow().is_none());

    assert_eq!(runtime.step(), 1);
    let child = (*child_ref.borrow()).expect("child ref delivered to parent");
    assert_eq!(child.generation, child.address.generation());
    assert_eq!(child.address.shard(), ShardId::new(3));

    assert_eq!(runtime.step(), 1);
    assert_eq!(&*child_seen.borrow(), &[42]);
}

#[test]
fn spawn_observed_reports_zero_capacity_without_spawning_child() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let child_seen = Rc::new(RefCell::new(Vec::new()));
    let order_log = Rc::new(RefCell::new(Vec::new()));
    let child_ref = Rc::new(RefCell::new(None));
    let spawn_error = Rc::new(RefCell::new(None));
    let parent = runtime.register(
        ObservedParent {
            child_seen,
            order_log,
            child_ref: Rc::clone(&child_ref),
            spawn_error: Rc::clone(&spawn_error),
        },
        TestMailbox::new(8),
    );

    assert!(
        runtime
            .try_send(parent, ObservedParentEvent::StartInvalidChild)
            .is_ok()
    );
    assert_eq!(runtime.step(), 1);
    assert!(child_ref.borrow().is_none());
    assert!(spawn_error.borrow().is_none());
    assert!(
        !runtime
            .trace()
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::Spawned { .. }))
    );

    assert_eq!(runtime.step(), 1);
    assert_eq!(
        *spawn_error.borrow(),
        Some(SpawnObservedError::ZeroMailboxCapacity)
    );
}

#[test]
fn spawn_observed_parent_delivery_full_is_traced_without_hidden_queue() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let child_seen = Rc::new(RefCell::new(Vec::new()));
    let order_log = Rc::new(RefCell::new(Vec::new()));
    let child_started_delivered = Rc::new(Cell::new(false));
    let parent = runtime.register(
        FullParent {
            child_seen,
            order_log,
            child_started_delivered: Rc::clone(&child_started_delivered),
        },
        TestMailbox::new(1),
    );

    assert!(runtime.try_send(parent, FullParentEvent::Start).is_ok());
    assert_eq!(runtime.step(), 1);
    assert!(runtime.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                reason: tina_runtime::SendRejectedReason::Full,
                ..
            }
        )
    }));

    assert_eq!(runtime.step(), 1);
    assert!(!child_started_delivered.get());
}

#[test]
fn observed_restart_delivers_each_typed_replacement_and_stales_old_address() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let child_seen = Rc::new(RefCell::new(Vec::new()));
    let incarnations = Rc::new(RefCell::new(Vec::new()));
    let restart_message_drops = Rc::new(Cell::new(0));
    let parent = runtime.register(
        RestartObservedParent {
            child_seen: Rc::clone(&child_seen),
            order_log: Rc::new(RefCell::new(Vec::new())),
            incarnations: Rc::clone(&incarnations),
            factory_calls: Rc::new(Cell::new(0)),
            panic_on_factory_call: None,
            initial_errors: Rc::new(RefCell::new(Vec::new())),
            initial_authority_drops: Rc::new(Cell::new(0)),
            restart_message_drops: Rc::clone(&restart_message_drops),
        },
        TestMailbox::new(4),
    );

    assert!(
        runtime
            .try_send(
                parent,
                ServiceMessage::Event(RestartObservedParentEvent::Start),
            )
            .is_ok()
    );
    assert_eq!(runtime.step(), 1);
    assert!(incarnations.borrow().is_empty());
    assert_eq!(runtime.step(), 1);
    let first = incarnations.borrow()[0];

    for expected_len in 2..=4 {
        assert!(
            runtime
                .try_send(
                    parent,
                    ServiceMessage::Event(RestartObservedParentEvent::Restart),
                )
                .is_ok()
        );
        assert_eq!(runtime.step(), 1);
        assert_eq!(incarnations.borrow().len(), expected_len - 1);
        assert_eq!(runtime.step(), 1);
        assert_eq!(incarnations.borrow().len(), expected_len);
    }

    let latest = *incarnations.borrow().last().expect("replacement child ref");
    assert_eq!(latest.address.system(), parent.system());
    assert_eq!(latest.address.shard(), parent.shard());
    assert_ne!(latest.address.isolate(), first.address.isolate());
    assert!(matches!(
        runtime.try_send(first.address, ChildEvent::Data(1)),
        Err(tina_runtime::IngressSendError::Closed(ChildEvent::Data(1)))
    ));
    assert_eq!(
        runtime.try_send(latest.address, ChildEvent::Data(9)),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);
    assert_eq!(&*child_seen.borrow(), &[9]);
    assert_eq!(restart_message_drops.get(), 3);
}

#[test]
fn service_observed_initial_factory_panic_is_typed_and_runtime_keeps_progressing() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let incarnations = Rc::new(RefCell::new(Vec::new()));
    let factory_calls = Rc::new(Cell::new(0));
    let initial_errors = Rc::new(RefCell::new(Vec::new()));
    let initial_authority_drops = Rc::new(Cell::new(0));
    let parent = runtime.register(
        RestartObservedParent {
            child_seen: Rc::new(RefCell::new(Vec::new())),
            order_log: Rc::new(RefCell::new(Vec::new())),
            incarnations: Rc::clone(&incarnations),
            factory_calls: Rc::clone(&factory_calls),
            panic_on_factory_call: Some(1),
            initial_errors: Rc::clone(&initial_errors),
            initial_authority_drops: Rc::clone(&initial_authority_drops),
            restart_message_drops: Rc::new(Cell::new(0)),
        },
        TestMailbox::new(4),
    );

    let start = || ServiceMessage::Event(RestartObservedParentEvent::Start);
    assert!(runtime.try_send(parent, start()).is_ok());
    assert_eq!(
        runtime.step(),
        1,
        "factory panic is contained in the effect"
    );
    assert_eq!(runtime.step(), 1, "typed error callback is delivered once");
    assert_eq!(factory_calls.get(), 1);
    assert_eq!(
        initial_errors.borrow().as_slice(),
        &[SpawnObservedError::FactoryPanicked]
    );
    assert_eq!(initial_authority_drops.get(), 1);
    assert!(incarnations.borrow().is_empty());
    assert!(
        !runtime
            .trace()
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::Spawned { .. }))
    );

    assert!(runtime.try_send(parent, start()).is_ok());
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);
    assert_eq!(factory_calls.get(), 2);
    assert_eq!(initial_errors.borrow().len(), 1);
    assert_eq!(initial_authority_drops.get(), 2);
    assert_eq!(incarnations.borrow().len(), 1);
}

#[test]
fn observed_restart_parent_full_rejects_without_hidden_delivery_queue() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let incarnations = Rc::new(RefCell::new(Vec::new()));
    let restart_message_drops = Rc::new(Cell::new(0));
    let parent = runtime.register(
        RestartObservedParent {
            child_seen: Rc::new(RefCell::new(Vec::new())),
            order_log: Rc::new(RefCell::new(Vec::new())),
            incarnations: Rc::clone(&incarnations),
            factory_calls: Rc::new(Cell::new(0)),
            panic_on_factory_call: None,
            initial_errors: Rc::new(RefCell::new(Vec::new())),
            initial_authority_drops: Rc::new(Cell::new(0)),
            restart_message_drops: Rc::clone(&restart_message_drops),
        },
        TestMailbox::new(1),
    );

    assert!(
        runtime
            .try_send(
                parent,
                ServiceMessage::Event(RestartObservedParentEvent::Start),
            )
            .is_ok()
    );
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);
    let initial = incarnations.borrow()[0];

    assert!(
        runtime
            .try_send(
                parent,
                ServiceMessage::Event(RestartObservedParentEvent::RestartWithFullMailbox),
            )
            .is_ok()
    );
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);
    assert_eq!(incarnations.borrow().as_slice(), &[initial]);
    assert!(runtime.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                target_isolate,
                reason: tina_runtime::SendRejectedReason::Full,
                ..
            } if target_isolate == parent.isolate()
        )
    }));
    assert_eq!(restart_message_drops.get(), 1);
    assert_eq!(runtime.step(), 0, "no hidden replacement delivery remains");
}

#[test]
fn observed_restart_parent_closed_rejects_without_hidden_delivery_queue() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let incarnations = Rc::new(RefCell::new(Vec::new()));
    let restart_message_drops = Rc::new(Cell::new(0));
    let parent_mailbox = TestMailbox::new(4);
    let parent = runtime.register(
        RestartObservedParent {
            child_seen: Rc::new(RefCell::new(Vec::new())),
            order_log: Rc::new(RefCell::new(Vec::new())),
            incarnations: Rc::clone(&incarnations),
            factory_calls: Rc::new(Cell::new(0)),
            panic_on_factory_call: None,
            initial_errors: Rc::new(RefCell::new(Vec::new())),
            initial_authority_drops: Rc::new(Cell::new(0)),
            restart_message_drops: Rc::clone(&restart_message_drops),
        },
        parent_mailbox.clone(),
    );

    assert!(
        runtime
            .try_send(
                parent,
                ServiceMessage::Event(RestartObservedParentEvent::Start),
            )
            .is_ok()
    );
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);
    let initial = incarnations.borrow()[0];

    assert!(
        runtime
            .try_send(
                parent,
                ServiceMessage::Event(RestartObservedParentEvent::Restart),
            )
            .is_ok()
    );
    parent_mailbox.close();
    assert_eq!(runtime.step(), 1);
    assert_eq!(incarnations.borrow().as_slice(), &[initial]);
    assert!(runtime.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                target_isolate,
                reason: tina_runtime::SendRejectedReason::Closed,
                ..
            } if target_isolate == parent.isolate()
        )
    }));
    assert_eq!(restart_message_drops.get(), 1);
    assert_eq!(runtime.step(), 0, "no hidden replacement delivery remains");
}

#[test]
fn observed_restart_factory_panic_skips_callback_and_later_retry_succeeds() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let incarnations = Rc::new(RefCell::new(Vec::new()));
    let factory_calls = Rc::new(Cell::new(0));
    let restart_message_drops = Rc::new(Cell::new(0));
    let parent = runtime.register(
        RestartObservedParent {
            child_seen: Rc::new(RefCell::new(Vec::new())),
            order_log: Rc::new(RefCell::new(Vec::new())),
            incarnations: Rc::clone(&incarnations),
            factory_calls: Rc::clone(&factory_calls),
            panic_on_factory_call: Some(2),
            initial_errors: Rc::new(RefCell::new(Vec::new())),
            initial_authority_drops: Rc::new(Cell::new(0)),
            restart_message_drops: Rc::clone(&restart_message_drops),
        },
        TestMailbox::new(4),
    );

    assert!(
        runtime
            .try_send(
                parent,
                ServiceMessage::Event(RestartObservedParentEvent::Start),
            )
            .is_ok()
    );
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);
    let initial = incarnations.borrow()[0];

    assert!(
        runtime
            .try_send(
                parent,
                ServiceMessage::Event(RestartObservedParentEvent::Restart),
            )
            .is_ok()
    );
    assert_eq!(runtime.step(), 1);
    assert_eq!(factory_calls.get(), 2);
    assert_eq!(restart_message_drops.get(), 0);
    assert_eq!(incarnations.borrow().as_slice(), &[initial]);
    assert!(runtime.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::RestartChildSkipped {
                reason: tina_runtime::RestartSkippedReason::FactoryPanicked,
                ..
            }
        )
    }));
    assert_eq!(
        runtime.step(),
        0,
        "failed restart must not queue a callback"
    );

    assert!(
        runtime
            .try_send(
                parent,
                ServiceMessage::Event(RestartObservedParentEvent::Restart),
            )
            .is_ok()
    );
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);
    assert_eq!(factory_calls.get(), 3);
    assert_eq!(incarnations.borrow().len(), 2);
    assert_eq!(restart_message_drops.get(), 1);
    assert_ne!(incarnations.borrow()[1].address, initial.address);
}

#[test]
fn spawned_child_runs_only_on_a_later_step_and_runtime_ingress_reaches_it() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let child_seen = Rc::new(RefCell::new(Vec::new()));
    let order_log = Rc::new(RefCell::new(Vec::new()));
    let parent = runtime.register(
        Parent {
            child_seen: Rc::clone(&child_seen),
            order_log,
            child_capacity: 2,
        },
        TestMailbox::new(8),
    );

    assert_eq!(runtime.try_send(parent, ParentEvent::StartChild), Ok(()));
    assert_eq!(runtime.step(), 1);

    let child = child_address(
        runtime.system_incarnation(),
        spawned_child_isolate(runtime.trace()),
    );
    assert_eq!(runtime.try_send(child, ChildEvent::Data(7)), Ok(()));
    assert!(child_seen.borrow().is_empty());

    assert_eq!(runtime.step(), 1);
    assert_eq!(*child_seen.borrow(), vec![7]);
}

#[test]
fn spawned_child_appends_to_registration_order() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let child_seen = Rc::new(RefCell::new(Vec::new()));
    let order_log = Rc::new(RefCell::new(Vec::new()));
    let parent = runtime.register(
        Parent {
            child_seen: Rc::clone(&child_seen),
            order_log: Rc::clone(&order_log),
            child_capacity: 2,
        },
        TestMailbox::new(8),
    );
    let sibling = runtime.register(
        OrderIsolate {
            name: "sibling",
            log: Rc::clone(&order_log),
        },
        TestMailbox::new(8),
    );

    assert_eq!(runtime.try_send(parent, ParentEvent::StartChild), Ok(()));
    assert_eq!(runtime.step(), 1);

    let child = child_address(
        runtime.system_incarnation(),
        spawned_child_isolate(runtime.trace()),
    );
    assert_eq!(runtime.try_send(sibling, OrderMsg::Tick), Ok(()));
    assert_eq!(runtime.try_send(child, ChildEvent::Data(9)), Ok(()));

    assert_eq!(runtime.step(), 2);
    assert_eq!(*order_log.borrow(), vec!["sibling", "child"]);
    assert_eq!(*child_seen.borrow(), vec![9]);
}

#[test]
fn runtime_ingress_returns_typed_full_and_closed_for_spawned_child() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let parent = runtime.register(
        Parent {
            child_seen: Rc::new(RefCell::new(Vec::new())),
            order_log: Rc::new(RefCell::new(Vec::new())),
            child_capacity: 1,
        },
        TestMailbox::new(8),
    );

    assert_eq!(runtime.try_send(parent, ParentEvent::StartChild), Ok(()));
    assert_eq!(runtime.step(), 1);

    let child = child_address(
        runtime.system_incarnation(),
        spawned_child_isolate(runtime.trace()),
    );
    assert_eq!(runtime.try_send(child, ChildEvent::Data(1)), Ok(()));
    assert_eq!(
        runtime.try_send(child, ChildEvent::Data(2)),
        Err(tina_runtime::IngressSendError::Full(ChildEvent::Data(2))),
    );

    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.try_send(child, ChildEvent::Stop), Ok(()));
    assert_eq!(runtime.step(), 1);
    assert_eq!(
        runtime.try_send(child, ChildEvent::Data(3)),
        Err(tina_runtime::IngressSendError::Closed(ChildEvent::Data(3))),
    );
}

#[test]
fn runtime_ingress_to_unknown_isolate_returns_closed() {
    let runtime = Runtime::new(TestShard, TestMailboxFactory);

    assert_eq!(
        runtime.try_send(
            child_address(runtime.system_incarnation(), IsolateId::new(99)),
            ChildEvent::Data(1),
        ),
        Err(tina_runtime::IngressSendError::Closed(ChildEvent::Data(1)))
    );
    assert!(runtime.trace().is_empty());
}

#[test]
fn runtime_ingress_to_other_shard_still_panics() {
    let runtime = Runtime::new(TestShard, TestMailboxFactory);

    let result = catch_unwind(AssertUnwindSafe(|| {
        let _ = runtime.try_send(
            Address::new_in(
                runtime.system_incarnation(),
                ShardId::new(9),
                IsolateId::new(1),
            ),
            ChildEvent::Data(1),
        );
    }));

    assert!(result.is_err());
}

#[test]
fn spawn_with_zero_capacity_panics_instead_of_creating_unreachable_child() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let parent = runtime.register(
        Parent {
            child_seen: Rc::new(RefCell::new(Vec::new())),
            order_log: Rc::new(RefCell::new(Vec::new())),
            child_capacity: 0,
        },
        TestMailbox::new(8),
    );

    assert_eq!(runtime.try_send(parent, ParentEvent::StartChild), Ok(()));

    let result = catch_unwind(AssertUnwindSafe(|| runtime.step()));
    assert!(result.is_err());
    assert!(
        runtime
            .trace()
            .iter()
            .all(|event| !matches!(event.kind(), RuntimeEventKind::Spawned { .. }))
    );
}

#[test]
fn identical_runs_produce_identical_spawn_sequences_and_causal_links() {
    fn run_once() -> Vec<RuntimeEvent> {
        let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
        let parent = runtime.register(
            Parent {
                child_seen: Rc::new(RefCell::new(Vec::new())),
                order_log: Rc::new(RefCell::new(Vec::new())),
                child_capacity: 2,
            },
            TestMailbox::new(8),
        );

        assert_eq!(runtime.try_send(parent, ParentEvent::StartChild), Ok(()));
        assert_eq!(runtime.step(), 1);

        let child = child_address(
            runtime.system_incarnation(),
            spawned_child_isolate(runtime.trace()),
        );
        assert_eq!(runtime.try_send(child, ChildEvent::Data(4)), Ok(()));
        assert_eq!(runtime.step(), 1);

        runtime.trace().to_vec()
    }

    assert_eq!(run_once(), run_once());
}
