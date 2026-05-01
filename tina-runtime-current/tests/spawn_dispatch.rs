use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use tina::{
    Address, ChildDefinition, Context, Effect, Isolate, IsolateId, Mailbox, Outbound, Shard,
    ShardId, TrySendError,
};
use tina_runtime::{
    CauseId, EffectKind, EventId, MailboxFactory, Runtime, RuntimeEvent, RuntimeEventKind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeverOutbound {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentMsg {
    SpawnChild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildMsg {
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
    type Message = ChildMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ChildMsg::Data(value) => {
                self.order_log.borrow_mut().push("child");
                self.seen.borrow_mut().push(value);
                Effect::Noop
            }
            ChildMsg::Stop => Effect::Stop,
        }
    }
}

#[derive(Debug)]
struct Parent {
    child_seen: Rc<RefCell<Vec<u8>>>,
    order_log: Rc<RefCell<Vec<&'static str>>>,
    child_capacity: usize,
}

impl Isolate for Parent {
    type Message = ParentMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = ChildDefinition<Child>;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ParentMsg::SpawnChild => Effect::Spawn(ChildDefinition::new(
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
    type Message = OrderMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        self.log.borrow_mut().push(self.name);
        Effect::Noop
    }
}

fn child_address(child_isolate: IsolateId) -> Address<ChildMsg> {
    Address::new(ShardId::new(3), child_isolate)
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

    assert_eq!(runtime.try_send(parent, ParentMsg::SpawnChild), Ok(()));
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

    assert_eq!(runtime.try_send(parent, ParentMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);

    let child = child_address(spawned_child_isolate(runtime.trace()));
    assert_eq!(runtime.try_send(child, ChildMsg::Data(7)), Ok(()));
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

    assert_eq!(runtime.try_send(parent, ParentMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);

    let child = child_address(spawned_child_isolate(runtime.trace()));
    assert_eq!(runtime.try_send(sibling, OrderMsg::Tick), Ok(()));
    assert_eq!(runtime.try_send(child, ChildMsg::Data(9)), Ok(()));

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

    assert_eq!(runtime.try_send(parent, ParentMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);

    let child = child_address(spawned_child_isolate(runtime.trace()));
    assert_eq!(runtime.try_send(child, ChildMsg::Data(1)), Ok(()));
    assert_eq!(
        runtime.try_send(child, ChildMsg::Data(2)),
        Err(TrySendError::Full(ChildMsg::Data(2))),
    );

    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.try_send(child, ChildMsg::Stop), Ok(()));
    assert_eq!(runtime.step(), 1);
    assert_eq!(
        runtime.try_send(child, ChildMsg::Data(3)),
        Err(TrySendError::Closed(ChildMsg::Data(3))),
    );
}

#[test]
fn runtime_ingress_to_unknown_isolate_still_panics() {
    let runtime = Runtime::new(TestShard, TestMailboxFactory);

    let result = catch_unwind(AssertUnwindSafe(|| {
        let _ = runtime.try_send(child_address(IsolateId::new(99)), ChildMsg::Data(1));
    }));

    assert!(result.is_err());
}

#[test]
fn runtime_ingress_to_other_shard_still_panics() {
    let runtime = Runtime::new(TestShard, TestMailboxFactory);

    let result = catch_unwind(AssertUnwindSafe(|| {
        let _ = runtime.try_send(
            Address::new(ShardId::new(9), IsolateId::new(1)),
            ChildMsg::Data(1),
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

    assert_eq!(runtime.try_send(parent, ParentMsg::SpawnChild), Ok(()));

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

        assert_eq!(runtime.try_send(parent, ParentMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);

        let child = child_address(spawned_child_isolate(runtime.trace()));
        assert_eq!(runtime.try_send(child, ChildMsg::Data(4)), Ok(()));
        assert_eq!(runtime.step(), 1);

        runtime.trace().to_vec()
    }

    assert_eq!(run_once(), run_once());
}
