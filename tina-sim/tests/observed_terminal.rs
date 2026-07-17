//! Simulator parity for typed child terminal observation.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;

use tina::{
    ChildDefinition, ChildRef, Context, Effect, Isolate, Outbound, ServiceMessage, SingleShard,
    SpawnObservedError, noop, spawn_observed, stop, stop_with,
};
use tina_runtime::{ChildTerminalDisposedReason, RuntimeCall, RuntimeEventKind};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChildTerminal(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildEvent {
    Report(u32),
    StopPlain,
}

struct Child;

impl Isolate for Child {
    type Message = ChildEvent;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<ChildEvent>;
    type Fact = Infallible;
    type Shard = SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ChildEvent::Report(value) => stop_with(ChildTerminal(value)),
            ChildEvent::StopPlain => stop(),
        }
    }
}

#[derive(Debug)]
enum ParentEvent {
    Start,
    ChildStarted(Result<ChildRef<ChildEvent>, SpawnObservedError>),
    ChildDone(ChildTerminal),
}

struct Parent {
    child: Rc<RefCell<Option<ChildRef<ChildEvent>>>>,
    terminals: Rc<RefCell<Vec<ChildTerminal>>>,
}

impl Isolate for Parent {
    type Message = ServiceMessage<ParentEvent, Infallible>;
    type Reply = ();
    type Send = Outbound<ChildEvent>;
    type Spawn = ChildDefinition<Child>;
    type SpawnObserved = tina::SpawnObserved<
        ChildDefinition<Child>,
        ServiceMessage<ParentEvent, Infallible>,
        ChildEvent,
    >;
    type Io = RuntimeCall<ServiceMessage<ParentEvent, Infallible>>;
    type Fact = Infallible;
    type Shard = SingleShard;

    fn handle(
        &mut self,
        message: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        let event = match message {
            ServiceMessage::Event(event) => event,
            ServiceMessage::Request(never) => match never {},
        };
        match event {
            ParentEvent::Start => spawn_observed(ChildDefinition::new(Child, 4))
                .then_service_result(ParentEvent::ChildDone)
                .then_service_event(ParentEvent::ChildStarted),
            ParentEvent::ChildStarted(Ok(child)) => {
                *self.child.borrow_mut() = Some(child);
                noop()
            }
            ParentEvent::ChildStarted(Err(_)) => noop(),
            ParentEvent::ChildDone(terminal) => {
                self.terminals.borrow_mut().push(terminal);
                noop()
            }
        }
    }
}

#[test]
fn sim_delivers_typed_child_terminal_to_parent() {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let child = Rc::new(RefCell::new(None));
    let terminals = Rc::new(RefCell::new(Vec::new()));
    let parent = sim.register_with_mailbox_capacity(
        Parent {
            child: Rc::clone(&child),
            terminals: Rc::clone(&terminals),
        },
        8,
    );

    assert!(
        sim.try_send(parent, ServiceMessage::Event(ParentEvent::Start))
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
    assert_eq!(sim.step(), 1);
    let child_ref = child.borrow().expect("child started");

    assert!(
        sim.try_send(child_ref.address, ChildEvent::Report(9))
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
    assert_eq!(sim.step(), 1);
    assert_eq!(terminals.borrow().as_slice(), &[ChildTerminal(9)]);
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::ChildTerminalDelivered { .. }
        )
    }));
}

#[test]
fn sim_plain_stop_disposes_terminal_reservation() {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let child = Rc::new(RefCell::new(None));
    let terminals = Rc::new(RefCell::new(Vec::new()));
    let parent = sim.register_with_mailbox_capacity(
        Parent {
            child: Rc::clone(&child),
            terminals: Rc::clone(&terminals),
        },
        8,
    );

    assert!(
        sim.try_send(parent, ServiceMessage::Event(ParentEvent::Start))
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
    assert_eq!(sim.step(), 1);
    let child_ref = child.borrow().expect("child started");

    assert!(
        sim.try_send(child_ref.address, ChildEvent::StopPlain)
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
    assert!(terminals.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::ChildTerminalDisposed {
                reason: ChildTerminalDisposedReason::StoppedWithoutResult,
                ..
            }
        )
    }));
}
