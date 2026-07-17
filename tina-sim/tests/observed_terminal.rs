//! Simulator parity for typed child terminal observation.

use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tina::{
    ChildDefinition, ChildRef, Context, Effect, Isolate, Outbound, ServiceMessage, SingleShard,
    SpawnObservedError, batch, noop, restart_children, send, spawn_observed, stop, stop_with,
};
use tina_runtime::{
    ChildTerminalDisposedReason, RestartSkippedReason, RuntimeCall, RuntimeEventKind,
};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChildTerminal(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildEvent {
    Report(u32),
    ReportProbe(u32),
    StopPlain,
}

#[allow(dead_code)]
struct SendDropProbe {
    drops: Arc<AtomicUsize>,
    value: u32,
}

impl Drop for SendDropProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct Child {
    probe_drops: Arc<AtomicUsize>,
}

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
            ChildEvent::Report(v) => stop_with(ChildTerminal(v)),
            ChildEvent::ReportProbe(v) => stop_with(SendDropProbe {
                drops: Arc::clone(&self.probe_drops),
                value: v,
            }),
            ChildEvent::StopPlain => stop(),
        }
    }
}

#[derive(Debug)]
enum ParentEvent {
    Start,
    StartWithPanickingMapper,
    StartWhenFull,
    Fill,
    ChildStarted(Result<ChildRef<ChildEvent>, SpawnObservedError>),
    ChildDone(ChildTerminal),
}

struct Parent {
    child: Rc<RefCell<Option<ChildRef<ChildEvent>>>>,
    errors: Rc<RefCell<Vec<SpawnObservedError>>>,
    terminals: Rc<RefCell<Vec<ChildTerminal>>>,
    probe_drops: Arc<AtomicUsize>,
}

impl Isolate for Parent {
    type Message = ServiceMessage<ParentEvent, Infallible>;
    type Reply = ();
    type Send = Outbound<ServiceMessage<ParentEvent, Infallible>>;
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
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        let event = match message {
            ServiceMessage::Event(e) => e,
            ServiceMessage::Request(n) => match n {},
        };
        match event {
            ParentEvent::Start => {
                let drops = Arc::clone(&self.probe_drops);
                spawn_observed(ChildDefinition::new(Child { probe_drops: drops }, 4))
                    .then_service_result(ParentEvent::ChildDone)
                    .then_service_event(ParentEvent::ChildStarted)
            }
            ParentEvent::StartWithPanickingMapper => {
                let drops = Arc::clone(&self.probe_drops);
                spawn_observed(ChildDefinition::new(Child { probe_drops: drops }, 4))
                    .then_service_result(|_: ChildTerminal| -> ParentEvent {
                        panic!("terminal mapper panic")
                    })
                    .then_service_event(ParentEvent::ChildStarted)
            }
            ParentEvent::StartWhenFull => {
                batch([send(ctx.me(), ServiceMessage::Event(ParentEvent::Fill)), {
                    let drops = Arc::clone(&self.probe_drops);
                    spawn_observed(ChildDefinition::new(Child { probe_drops: drops }, 4))
                        .then_service_result(ParentEvent::ChildDone)
                        .then_service_event(ParentEvent::ChildStarted)
                }])
            }
            ParentEvent::Fill => noop(),
            ParentEvent::ChildStarted(Ok(c)) => {
                *self.child.borrow_mut() = Some(c);
                noop()
            }
            ParentEvent::ChildStarted(Err(e)) => {
                self.errors.borrow_mut().push(e);
                noop()
            }
            ParentEvent::ChildDone(t) => {
                self.terminals.borrow_mut().push(t);
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
            errors: Rc::new(RefCell::new(Vec::new())),
            terminals: Rc::clone(&terminals),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        8,
    );
    assert!(
        sim.try_send(parent, ServiceMessage::Event(ParentEvent::Start))
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
    assert_eq!(sim.step(), 1);
    let c = child.borrow().expect("child");
    assert!(sim.try_send(c.address, ChildEvent::Report(9)).is_ok());
    assert_eq!(sim.step(), 1);
    assert_eq!(sim.step(), 1);
    assert_eq!(terminals.borrow().as_slice(), &[ChildTerminal(9)]);
    assert!(
        sim.trace()
            .iter()
            .any(|e| { matches!(e.kind(), RuntimeEventKind::ChildTerminalDelivered { .. }) })
    );
}

#[test]
fn sim_panicking_terminal_mapper_is_disposed_and_progress_continues() {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let child = Rc::new(RefCell::new(None));
    let parent = sim.register_with_mailbox_capacity(
        Parent {
            child: Rc::clone(&child),
            errors: Rc::new(RefCell::new(Vec::new())),
            terminals: Rc::new(RefCell::new(Vec::new())),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        2,
    );
    assert!(
        sim.try_send(
            parent,
            ServiceMessage::Event(ParentEvent::StartWithPanickingMapper),
        )
        .is_ok()
    );
    assert_eq!(sim.step(), 1);
    assert_eq!(sim.step(), 1);
    let child = child.borrow().expect("child");
    assert!(sim.try_send(child.address, ChildEvent::Report(44)).is_ok());
    assert_eq!(sim.step(), 1);
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::ChildTerminalDisposed {
                reason: ChildTerminalDisposedReason::MapperPanicked,
                ..
            }
        )
    }));
    assert!(
        sim.try_send(parent, ServiceMessage::Event(ParentEvent::Fill))
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
}

#[test]
fn sim_plain_stop_disposes_terminal_reservation() {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let child = Rc::new(RefCell::new(None));
    let parent = sim.register_with_mailbox_capacity(
        Parent {
            child: Rc::clone(&child),
            errors: Rc::new(RefCell::new(Vec::new())),
            terminals: Rc::new(RefCell::new(Vec::new())),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        8,
    );
    assert!(
        sim.try_send(parent, ServiceMessage::Event(ParentEvent::Start))
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
    assert_eq!(sim.step(), 1);
    let c = child.borrow().expect("child");
    assert!(sim.try_send(c.address, ChildEvent::StopPlain).is_ok());
    assert_eq!(sim.step(), 1);
    assert!(sim.trace().iter().any(|e| {
        matches!(
            e.kind(),
            RuntimeEventKind::ChildTerminalDisposed {
                reason: ChildTerminalDisposedReason::StoppedWithoutResult,
                ..
            }
        )
    }));
}

#[test]
fn sim_type_mismatch_disposes_and_drops_probe() {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let child = Rc::new(RefCell::new(None));
    let probe_drops = Arc::new(AtomicUsize::new(0));
    let parent = sim.register_with_mailbox_capacity(
        Parent {
            child: Rc::clone(&child),
            errors: Rc::new(RefCell::new(Vec::new())),
            terminals: Rc::new(RefCell::new(Vec::new())),
            probe_drops: Arc::clone(&probe_drops),
        },
        8,
    );
    assert!(
        sim.try_send(parent, ServiceMessage::Event(ParentEvent::Start))
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
    assert_eq!(sim.step(), 1);
    let c = child.borrow().expect("child");
    assert!(sim.try_send(c.address, ChildEvent::ReportProbe(3)).is_ok());
    assert_eq!(sim.step(), 1);
    assert_eq!(probe_drops.load(Ordering::SeqCst), 1);
    assert!(sim.trace().iter().any(|e| {
        matches!(
            e.kind(),
            RuntimeEventKind::ChildTerminalDisposed {
                reason: ChildTerminalDisposedReason::TypeMismatch,
                ..
            }
        )
    }));
}

#[test]
fn sim_admission_full_delivers_parent_mailbox_full() {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let errors = Rc::new(RefCell::new(Vec::new()));
    let child = Rc::new(RefCell::new(None));
    let parent = sim.register_with_mailbox_capacity(
        Parent {
            child: Rc::clone(&child),
            errors: Rc::clone(&errors),
            terminals: Rc::new(RefCell::new(Vec::new())),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        1,
    );
    assert!(
        sim.try_send(parent, ServiceMessage::Event(ParentEvent::StartWhenFull))
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
    // Admission error is force-pushed front; delivered next (or same drain).
    assert_eq!(sim.step(), 1);
    assert_eq!(
        errors.borrow().as_slice(),
        &[SpawnObservedError::ParentMailboxFull]
    );
    assert!(child.borrow().is_none());
}

#[test]
fn sim_parent_stop_disposes_reservation() {
    #[derive(Debug)]
    #[allow(dead_code)]
    enum Ev {
        Go,
        Started(Result<ChildRef<ChildEvent>, SpawnObservedError>),
        Done(ChildTerminal),
    }
    struct P;
    impl Isolate for P {
        type Message = ServiceMessage<Ev, Infallible>;
        type Reply = ();
        type Send = Outbound<ServiceMessage<Ev, Infallible>>;
        type Spawn = ChildDefinition<Child>;
        type SpawnObserved =
            tina::SpawnObserved<ChildDefinition<Child>, ServiceMessage<Ev, Infallible>, ChildEvent>;
        type Io = RuntimeCall<ServiceMessage<Ev, Infallible>>;
        type Fact = Infallible;
        type Shard = SingleShard;

        fn handle(
            &mut self,
            message: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            let event = match message {
                ServiceMessage::Event(e) => e,
                ServiceMessage::Request(n) => match n {},
            };
            match event {
                Ev::Go => batch([
                    spawn_observed(ChildDefinition::new(
                        Child {
                            probe_drops: Arc::new(AtomicUsize::new(0)),
                        },
                        4,
                    ))
                    .then_service_result(Ev::Done)
                    .then_service_event(Ev::Started),
                    stop(),
                ]),
                Ev::Started(_) | Ev::Done(_) => noop(),
            }
        }
    }
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let parent = sim.register_with_mailbox_capacity(P, 8);
    assert!(sim.try_send(parent, ServiceMessage::Event(Ev::Go)).is_ok());
    assert_eq!(sim.step(), 1);
    assert!(sim.trace().iter().any(|e| {
        matches!(
            e.kind(),
            RuntimeEventKind::ChildTerminalDisposed {
                reason: ChildTerminalDisposedReason::ParentStopped,
                ..
            }
        )
    }));
}

#[test]
fn sim_child_started_delivers_under_capacity_one_with_terminal_reservation() {
    // capacity 1 + terminal reservation holds the only ordinary slot; ChildStarted
    // must still land via lifecycle force-admit (live overflow parity).
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let child = Rc::new(RefCell::new(None));
    let parent = sim.register_with_mailbox_capacity(
        Parent {
            child: Rc::clone(&child),
            errors: Rc::new(RefCell::new(Vec::new())),
            terminals: Rc::new(RefCell::new(Vec::new())),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        1,
    );
    assert!(
        sim.try_send(parent, ServiceMessage::Event(ParentEvent::Start))
            .is_ok()
    );
    assert_eq!(sim.step(), 1, "spawn effect");
    assert_eq!(sim.step(), 1, "ChildStarted via lifecycle force-admit");
    assert!(
        child.borrow().is_some(),
        "ChildStarted must deliver under capacity-1 + terminal reservation"
    );
    assert!(
        sim.trace()
            .iter()
            .any(|e| { matches!(e.kind(), RuntimeEventKind::Spawned { .. }) })
    );
}

#[test]
fn sim_restart_full_skips_with_parent_mailbox_full() {
    #[derive(Debug)]
    #[allow(dead_code)]
    enum REv {
        Start,
        RestartWhenFull,
        Fill,
        Started(Result<ChildRef<ChildEvent>, SpawnObservedError>),
        Restarted(ChildRef<ChildEvent>),
        Done(ChildTerminal),
    }
    struct RP {
        incarnations: Rc<RefCell<Vec<ChildRef<ChildEvent>>>>,
        factory: Rc<Cell<usize>>,
        probe_drops: Arc<AtomicUsize>,
    }
    impl Isolate for RP {
        type Message = ServiceMessage<REv, Infallible>;
        type Reply = ();
        type Send = Outbound<ServiceMessage<REv, Infallible>>;
        type Spawn = tina::RestartableChildDefinition<Child>;
        type SpawnObserved = tina::SpawnObserved<
            tina::RestartableChildDefinition<Child>,
            ServiceMessage<REv, Infallible>,
            ChildEvent,
        >;
        type Io = RuntimeCall<ServiceMessage<REv, Infallible>>;
        type Fact = Infallible;
        type Shard = SingleShard;

        fn handle(
            &mut self,
            message: Self::Message,
            ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            let event = match message {
                ServiceMessage::Event(e) => e,
                ServiceMessage::Request(n) => match n {},
            };
            match event {
                REv::Start => {
                    let factory = Rc::clone(&self.factory);
                    let drops = Arc::clone(&self.probe_drops);
                    spawn_observed(tina::RestartableChildDefinition::new(
                        move || {
                            factory.set(factory.get() + 1);
                            Child {
                                probe_drops: Arc::clone(&drops),
                            }
                        },
                        4,
                    ))
                    .then_service_result(REv::Done)
                    .then_service_event_with_restarts(REv::Started, REv::Restarted)
                }
                REv::RestartWhenFull => batch([
                    send(ctx.me(), ServiceMessage::Event(REv::Fill)),
                    restart_children(),
                ]),
                REv::Fill => noop(),
                REv::Started(Ok(c)) => {
                    self.incarnations.borrow_mut().push(c);
                    noop()
                }
                REv::Started(Err(_)) => noop(),
                REv::Restarted(c) => {
                    self.incarnations.borrow_mut().push(c);
                    noop()
                }
                REv::Done(_) => noop(),
            }
        }
    }

    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let incarnations = Rc::new(RefCell::new(Vec::new()));
    let parent = sim.register_with_mailbox_capacity(
        RP {
            incarnations: Rc::clone(&incarnations),
            factory: Rc::new(Cell::new(0)),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        1,
    );
    assert!(
        sim.try_send(parent, ServiceMessage::Event(REv::Start))
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
    assert_eq!(sim.step(), 1, "ChildStarted must land under capacity 1");
    assert_eq!(
        incarnations.borrow().len(),
        1,
        "must have a child before restart Full"
    );
    let first = incarnations.borrow()[0];

    // Free reservation so RestartWhenFull can enter the mailbox.
    assert!(sim.try_send(first.address, ChildEvent::StopPlain).is_ok());
    assert_eq!(sim.step(), 1);

    assert!(
        sim.try_send(parent, ServiceMessage::Event(REv::RestartWhenFull))
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
    assert!(
        sim.trace().iter().any(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::RestartChildSkipped {
                    reason: RestartSkippedReason::ParentMailboxFull,
                    ..
                }
            )
        }),
        "restart must skip with ParentMailboxFull when Fill packs capacity"
    );
    assert_eq!(
        incarnations.borrow().len(),
        1,
        "no replacement incarnation when restart reservation Full"
    );
}

#[test]
fn sim_restart_continuation_delivers_under_mailbox_full() {
    // Mirror of live: no terminal reservation; Fill packs capacity 1; restart
    // succeeds; Restarted force-admits via lifecycle continuation.
    #[derive(Debug)]
    #[allow(dead_code)]
    enum Ev {
        Start,
        RestartWhenFull,
        Fill,
        Started(Result<ChildRef<ChildEvent>, SpawnObservedError>),
        Restarted(ChildRef<ChildEvent>),
    }
    struct P {
        incarnations: Rc<RefCell<Vec<ChildRef<ChildEvent>>>>,
        factory: Rc<Cell<usize>>,
        probe_drops: Arc<AtomicUsize>,
    }
    impl Isolate for P {
        type Message = ServiceMessage<Ev, Infallible>;
        type Reply = ();
        type Send = Outbound<ServiceMessage<Ev, Infallible>>;
        type Spawn = tina::RestartableChildDefinition<Child>;
        type SpawnObserved = tina::SpawnObserved<
            tina::RestartableChildDefinition<Child>,
            ServiceMessage<Ev, Infallible>,
            ChildEvent,
        >;
        type Io = RuntimeCall<ServiceMessage<Ev, Infallible>>;
        type Fact = Infallible;
        type Shard = SingleShard;

        fn handle(
            &mut self,
            message: Self::Message,
            ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            let event = match message {
                ServiceMessage::Event(e) => e,
                ServiceMessage::Request(n) => match n {},
            };
            match event {
                Ev::Start => {
                    let factory = Rc::clone(&self.factory);
                    let drops = Arc::clone(&self.probe_drops);
                    spawn_observed(tina::RestartableChildDefinition::new(
                        move || {
                            factory.set(factory.get() + 1);
                            Child {
                                probe_drops: Arc::clone(&drops),
                            }
                        },
                        4,
                    ))
                    .then_service_event_with_restarts(Ev::Started, Ev::Restarted)
                }
                Ev::RestartWhenFull => batch([
                    send(ctx.me(), ServiceMessage::Event(Ev::Fill)),
                    restart_children(),
                ]),
                Ev::Fill => noop(),
                Ev::Started(Ok(c)) => {
                    self.incarnations.borrow_mut().push(c);
                    noop()
                }
                Ev::Started(Err(_)) => noop(),
                Ev::Restarted(c) => {
                    self.incarnations.borrow_mut().push(c);
                    noop()
                }
            }
        }
    }

    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let incarnations = Rc::new(RefCell::new(Vec::new()));
    let parent = sim.register_with_mailbox_capacity(
        P {
            incarnations: Rc::clone(&incarnations),
            factory: Rc::new(Cell::new(0)),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        1,
    );
    assert!(
        sim.try_send(parent, ServiceMessage::Event(Ev::Start))
            .is_ok()
    );
    assert_eq!(sim.step(), 1);
    assert_eq!(sim.step(), 1);
    assert_eq!(incarnations.borrow().len(), 1);

    assert!(
        sim.try_send(parent, ServiceMessage::Event(Ev::RestartWhenFull))
            .is_ok()
    );
    assert_eq!(sim.step(), 1, "restart under packed mailbox");
    assert!(
        sim.trace()
            .iter()
            .any(|e| { matches!(e.kind(), RuntimeEventKind::RestartChildCompleted { .. }) }),
        "restart must complete"
    );
    // Force-admit may drain Restarted in the same step as restart effects, or
    // the next — either way the replacement incarnation must land.
    if incarnations.borrow().len() < 2 {
        assert_eq!(sim.step(), 1, "Restarted via lifecycle force-admit");
    }
    assert_eq!(
        incarnations.borrow().len(),
        2,
        "restart continuation must deliver under Full"
    );
}
