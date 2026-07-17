//! Live runtime proofs for typed child terminal observation.

use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tina::{
    ChildDefinition, ChildRef, Context, Effect, Isolate, Mailbox, Outbound, ServiceMessage, Shard,
    ShardId, SpawnObservedError, TrySendError, noop, restart_children, send, spawn_observed, stop,
    stop_with,
};
use tina_runtime::{
    ChildTerminalDisposedReason, MailboxFactory, RestartSkippedReason, Runtime, RuntimeEventKind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChildTerminal(u32);

#[allow(dead_code)]
struct SendDropProbe {
    drops: Arc<AtomicUsize>,
    value: u32,
}

impl SendDropProbe {
    fn new(value: u32, drops: Arc<AtomicUsize>) -> Self {
        Self { drops, value }
    }
}

impl Drop for SendDropProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
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
    queue: Rc<RefCell<std::collections::VecDeque<T>>>,
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
            queue: Rc::new(RefCell::new(std::collections::VecDeque::new())),
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
        let mut q = self.queue.borrow_mut();
        if q.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        q.push_back(message);
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
    fn is_closed(&self) -> bool {
        self.closed.get()
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;
impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildEvent {
    Report(u32),
    ReportProbe(u32),
    StopPlain,
}

struct Child {
    probe_drops: Arc<AtomicUsize>,
}

impl Isolate for Child {
    tina::isolate_types! {
        message: ChildEvent,
        reply: (),
        send: Outbound<Infallible>,
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
            ChildEvent::Report(v) => stop_with(ChildTerminal(v)),
            ChildEvent::ReportProbe(v) => {
                stop_with(SendDropProbe::new(v, Arc::clone(&self.probe_drops)))
            }
            ChildEvent::StopPlain => stop(),
        }
    }
}

#[derive(Debug)]
enum ParentEvent {
    Start,
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
    tina::isolate_types! {
        message: ParentEvent,
        reply: (),
        send: Outbound<ParentEvent>,
        spawn: ChildDefinition<Child>,
        spawn_observed: tina::SpawnObserved<ChildDefinition<Child>, ParentEvent, ChildEvent>,
        io: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ParentEvent::Start => {
                let drops = Arc::clone(&self.probe_drops);
                spawn_observed(ChildDefinition::new(Child { probe_drops: drops }, 4))
                    .then_result(ParentEvent::ChildDone)
                    .then(ParentEvent::ChildStarted)
            }
            ParentEvent::StartWhenFull => batch([send(ctx.me(), ParentEvent::Fill), {
                let drops = Arc::clone(&self.probe_drops);
                spawn_observed(ChildDefinition::new(Child { probe_drops: drops }, 4))
                    .then_result(ParentEvent::ChildDone)
                    .then(ParentEvent::ChildStarted)
            }]),
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

use tina::batch;

#[test]
fn delivers_typed_terminal_once() {
    let mut rt = Runtime::new(TestShard, TestMailboxFactory);
    let child = Rc::new(RefCell::new(None));
    let terminals = Rc::new(RefCell::new(Vec::new()));
    let parent = rt.register(
        Parent {
            child: Rc::clone(&child),
            errors: Rc::new(RefCell::new(Vec::new())),
            terminals: Rc::clone(&terminals),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        TestMailbox::new(8),
    );
    assert!(rt.try_send(parent, ParentEvent::Start).is_ok());
    assert_eq!(rt.step(), 1);
    assert_eq!(rt.step(), 1);
    let c = child.borrow().expect("child");
    assert!(rt.try_send(c.address, ChildEvent::Report(9)).is_ok());
    assert_eq!(rt.step(), 1);
    assert_eq!(rt.step(), 1);
    assert_eq!(terminals.borrow().as_slice(), &[ChildTerminal(9)]);
    assert!(
        rt.trace()
            .iter()
            .any(|e| { matches!(e.kind(), RuntimeEventKind::ChildTerminalDelivered { .. }) })
    );
}

#[test]
fn plain_stop_disposes_without_result() {
    let mut rt = Runtime::new(TestShard, TestMailboxFactory);
    let child = Rc::new(RefCell::new(None));
    let parent = rt.register(
        Parent {
            child: Rc::clone(&child),
            errors: Rc::new(RefCell::new(Vec::new())),
            terminals: Rc::new(RefCell::new(Vec::new())),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        TestMailbox::new(8),
    );
    assert!(rt.try_send(parent, ParentEvent::Start).is_ok());
    assert_eq!(rt.step(), 1);
    assert_eq!(rt.step(), 1);
    let c = child.borrow().expect("child");
    assert!(rt.try_send(c.address, ChildEvent::StopPlain).is_ok());
    assert_eq!(rt.step(), 1);
    assert!(rt.trace().iter().any(|e| {
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
fn type_mismatch_disposes_and_drops_payload() {
    let mut rt = Runtime::new(TestShard, TestMailboxFactory);
    let child = Rc::new(RefCell::new(None));
    let probe_drops = Arc::new(AtomicUsize::new(0));
    let parent = rt.register(
        Parent {
            child: Rc::clone(&child),
            errors: Rc::new(RefCell::new(Vec::new())),
            terminals: Rc::new(RefCell::new(Vec::new())),
            probe_drops: Arc::clone(&probe_drops),
        },
        TestMailbox::new(8),
    );
    assert!(rt.try_send(parent, ParentEvent::Start).is_ok());
    assert_eq!(rt.step(), 1);
    assert_eq!(rt.step(), 1);
    let c = child.borrow().expect("child");
    // Parent mapper expects ChildTerminal; child sends SendDropProbe.
    assert!(rt.try_send(c.address, ChildEvent::ReportProbe(7)).is_ok());
    assert_eq!(rt.step(), 1);
    assert_eq!(probe_drops.load(Ordering::SeqCst), 1);
    assert!(rt.trace().iter().any(|e| {
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
fn admission_full_delivers_parent_mailbox_full() {
    let mut rt = Runtime::new(TestShard, TestMailboxFactory);
    let errors = Rc::new(RefCell::new(Vec::new()));
    let child = Rc::new(RefCell::new(None));
    let parent = rt.register(
        Parent {
            child: Rc::clone(&child),
            errors: Rc::clone(&errors),
            terminals: Rc::new(RefCell::new(Vec::new())),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        TestMailbox::new(1),
    );
    assert!(rt.try_send(parent, ParentEvent::StartWhenFull).is_ok());
    assert_eq!(rt.step(), 1);
    // Priority overflow delivers the typed admission error next.
    assert_eq!(rt.step(), 1);
    assert_eq!(
        errors.borrow().as_slice(),
        &[SpawnObservedError::ParentMailboxFull]
    );
    assert!(child.borrow().is_none());
    assert!(
        !rt.trace()
            .iter()
            .any(|e| matches!(e.kind(), RuntimeEventKind::Spawned { .. }))
    );
}

#[test]
fn admission_closed_rejects_spawn() {
    let mut rt = Runtime::new(TestShard, TestMailboxFactory);
    let mb = TestMailbox::new(4);
    let errors = Rc::new(RefCell::new(Vec::new()));
    let parent = rt.register(
        Parent {
            child: Rc::new(RefCell::new(None)),
            errors: Rc::clone(&errors),
            terminals: Rc::new(RefCell::new(Vec::new())),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        mb.clone(),
    );
    assert!(rt.try_send(parent, ParentEvent::Start).is_ok());
    mb.close();
    assert_eq!(rt.step(), 1);
    assert!(
        !rt.trace()
            .iter()
            .any(|e| matches!(e.kind(), RuntimeEventKind::Spawned { .. }))
    );
    // Closed admission: error may be parked or rejected Closed; no child either way.
    let closed_err = errors.borrow().as_slice() == [SpawnObservedError::ParentMailboxClosed];
    let closed_reject = rt.trace().iter().any(|e| {
        matches!(
            e.kind(),
            RuntimeEventKind::SendRejected {
                reason: tina_runtime::SendRejectedReason::Closed,
                ..
            }
        )
    });
    assert!(closed_err || closed_reject);
}

#[test]
fn parent_stop_disposes_reservation() {
    #[derive(Debug)]
    #[allow(dead_code)]
    enum Ev {
        Go,
        Started(Result<ChildRef<ChildEvent>, SpawnObservedError>),
        Done(ChildTerminal),
    }
    struct P;
    impl Isolate for P {
        tina::isolate_types! {
            message: Ev,
            reply: (),
            send: Outbound<ChildEvent>,
            spawn: ChildDefinition<Child>,
            spawn_observed: tina::SpawnObserved<ChildDefinition<Child>, Ev, ChildEvent>,
            io: Infallible,
            shard: TestShard,
        }
        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                Ev::Go => batch([
                    spawn_observed(ChildDefinition::new(
                        Child {
                            probe_drops: Arc::new(AtomicUsize::new(0)),
                        },
                        4,
                    ))
                    .then_result(Ev::Done)
                    .then(Ev::Started),
                    stop(),
                ]),
                Ev::Started(_) | Ev::Done(_) => noop(),
            }
        }
    }
    let mut rt = Runtime::new(TestShard, TestMailboxFactory);
    let parent = rt.register(P, TestMailbox::new(8));
    assert!(rt.try_send(parent, Ev::Go).is_ok());
    assert_eq!(rt.step(), 1);
    assert!(rt.trace().iter().any(|e| {
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
fn shutdown_dispose_emits_shutdown_reason() {
    let mut rt = Runtime::new(TestShard, TestMailboxFactory);
    let child = Rc::new(RefCell::new(None));
    let parent = rt.register(
        Parent {
            child: Rc::clone(&child),
            errors: Rc::new(RefCell::new(Vec::new())),
            terminals: Rc::new(RefCell::new(Vec::new())),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        TestMailbox::new(8),
    );
    assert!(rt.try_send(parent, ParentEvent::Start).is_ok());
    assert_eq!(rt.step(), 1);
    assert_eq!(rt.step(), 1);
    assert!(child.borrow().is_some());
    rt.settle_terminal_reservations_on_shutdown();
    assert!(rt.trace().iter().any(|e| {
        matches!(
            e.kind(),
            RuntimeEventKind::ChildTerminalDisposed {
                reason: ChildTerminalDisposedReason::Shutdown,
                ..
            }
        )
    }));
}

// Restart matrix

#[derive(Debug)]
enum REv {
    Start,
    Restart,
    RestartWhenFull,
    Fill,
    Started(Result<ChildRef<ChildEvent>, SpawnObservedError>),
    Restarted(ChildRef<ChildEvent>),
    Done(ChildTerminal),
}

struct RParent {
    incarnations: Rc<RefCell<Vec<ChildRef<ChildEvent>>>>,
    terminals: Rc<RefCell<Vec<ChildTerminal>>>,
    factory: Rc<Cell<usize>>,
    probe_drops: Arc<AtomicUsize>,
}

impl Isolate for RParent {
    tina::isolate_types! {
        message: ServiceMessage<REv, Infallible>,
        reply: (),
        send: Outbound<ServiceMessage<REv, Infallible>>,
        spawn: tina::RestartableChildDefinition<Child>,
        spawn_observed: tina::SpawnObserved<tina::RestartableChildDefinition<Child>, ServiceMessage<REv, Infallible>, ChildEvent>,
        io: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        message: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        let msg = match message {
            ServiceMessage::Event(e) => e,
            ServiceMessage::Request(r) => match r {},
        };
        match msg {
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
            REv::Restart => restart_children(),
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
            REv::Done(t) => {
                self.terminals.borrow_mut().push(t);
                noop()
            }
        }
    }
}

#[test]
fn restart_full_skips_with_parent_mailbox_full() {
    let mut rt = Runtime::new(TestShard, TestMailboxFactory);
    let incarnations = Rc::new(RefCell::new(Vec::new()));
    let parent = rt.register(
        RParent {
            incarnations: Rc::clone(&incarnations),
            terminals: Rc::new(RefCell::new(Vec::new())),
            factory: Rc::new(Cell::new(0)),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        // capacity 1: reservation holds the only slot; ChildStarted uses overflow.
        TestMailbox::new(1),
    );
    assert!(
        rt.try_send(parent, ServiceMessage::Event(REv::Start))
            .is_ok()
    );
    assert_eq!(rt.step(), 1);
    assert_eq!(rt.step(), 1, "ChildStarted via overflow");
    assert_eq!(incarnations.borrow().len(), 1);
    let first = incarnations.borrow()[0];

    // Release reservation so the mailbox can accept RestartWhenFull.
    assert!(rt.try_send(first.address, ChildEvent::StopPlain).is_ok());
    assert_eq!(rt.step(), 1);

    // RestartWhenFull: Fill packs capacity 1, then restart cannot re-reserve.
    assert!(
        rt.try_send(parent, ServiceMessage::Event(REv::RestartWhenFull))
            .is_ok()
    );
    assert_eq!(rt.step(), 1);
    let skipped: Vec<_> = rt
        .trace()
        .iter()
        .filter_map(|e| match e.kind() {
            RuntimeEventKind::RestartChildSkipped { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert!(
        skipped.contains(&RestartSkippedReason::ParentMailboxFull),
        "expected ParentMailboxFull skip, got {skipped:?}"
    );
}

#[test]
fn restart_then_late_old_incarnation_is_stale() {
    let mut rt = Runtime::new(TestShard, TestMailboxFactory);
    let incarnations = Rc::new(RefCell::new(Vec::new()));
    let terminals = Rc::new(RefCell::new(Vec::new()));
    let parent = rt.register(
        RParent {
            incarnations: Rc::clone(&incarnations),
            terminals: Rc::clone(&terminals),
            factory: Rc::new(Cell::new(0)),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        TestMailbox::new(8),
    );
    assert!(
        rt.try_send(parent, ServiceMessage::Event(REv::Start))
            .is_ok()
    );
    assert_eq!(rt.step(), 1);
    assert_eq!(rt.step(), 1);
    let first = incarnations.borrow()[0];

    // Restart while first is live: old settles StoppedWithoutResult, previous_child set.
    assert!(
        rt.try_send(parent, ServiceMessage::Event(REv::Restart))
            .is_ok()
    );
    assert_eq!(rt.step(), 1);
    let _ = rt.step();
    assert!(!incarnations.borrow().is_empty());
    assert!(rt.trace().iter().any(|e| {
        matches!(
            e.kind(),
            RuntimeEventKind::ChildTerminalDisposed {
                reason: ChildTerminalDisposedReason::StoppedWithoutResult,
                ..
            }
        )
    }));

    // Old address is stopped — send closed. No second parent delivery.
    assert!(rt.try_send(first.address, ChildEvent::Report(99)).is_err());
    assert!(terminals.borrow().is_empty());
}

#[test]
fn settle_then_restart_no_second_parent_delivery_for_old() {
    let mut rt = Runtime::new(TestShard, TestMailboxFactory);
    let incarnations = Rc::new(RefCell::new(Vec::new()));
    let terminals = Rc::new(RefCell::new(Vec::new()));
    let parent = rt.register(
        RParent {
            incarnations: Rc::clone(&incarnations),
            terminals: Rc::clone(&terminals),
            factory: Rc::new(Cell::new(0)),
            probe_drops: Arc::new(AtomicUsize::new(0)),
        },
        TestMailbox::new(8),
    );
    assert!(
        rt.try_send(parent, ServiceMessage::Event(REv::Start))
            .is_ok()
    );
    assert_eq!(rt.step(), 1);
    assert_eq!(rt.step(), 1);
    let first = incarnations.borrow()[0];
    assert!(rt.try_send(first.address, ChildEvent::Report(1)).is_ok());
    assert_eq!(rt.step(), 1);
    assert_eq!(rt.step(), 1);
    assert_eq!(terminals.borrow().as_slice(), &[ChildTerminal(1)]);

    assert!(
        rt.try_send(parent, ServiceMessage::Event(REv::Restart))
            .is_ok()
    );
    assert_eq!(rt.step(), 1);
    let _ = rt.step();
    // Late send on old is Closed; no second terminal delivery.
    let _ = rt.try_send(first.address, ChildEvent::Report(2));
    assert_eq!(terminals.borrow().len(), 1);
}
