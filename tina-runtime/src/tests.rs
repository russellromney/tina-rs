use super::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use tina::{Outbound, batch, noop, send, spawn, stop};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeverOutbound {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineageMsg {
    SpawnChild,
    SpawnGrandchild,
    Stop,
    Panic,
    Restart,
}

type RunEvidence = (
    Vec<RuntimeEvent>,
    Vec<(IsolateId, Option<IsolateId>)>,
    Vec<ChildRecordSnapshot>,
);

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(3)
    }
}

#[derive(Debug, Clone, Copy)]
struct NumberedShard(u32);

impl Shard for NumberedShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

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

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

#[derive(Debug)]
struct RootIsolate {
    child_capacity: usize,
}

#[derive(Debug)]
struct RestartableRootIsolate {
    child_capacity: usize,
    factory_calls: Rc<Cell<usize>>,
}

#[derive(Debug)]
struct ChildIsolate {
    leaf_capacity: usize,
}

#[derive(Debug)]
struct LeafIsolate;

impl Isolate for RootIsolate {
    type Message = LineageMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = tina::ChildDefinition<ChildIsolate>;
    type Call = std::convert::Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            LineageMsg::SpawnChild => Effect::Spawn(tina::ChildDefinition::new(
                ChildIsolate {
                    leaf_capacity: self.child_capacity,
                },
                self.child_capacity,
            )),
            LineageMsg::Stop => Effect::Stop,
            LineageMsg::Panic => panic!("panic inside root lineage isolate"),
            LineageMsg::Restart => Effect::RestartChildren,
            LineageMsg::SpawnGrandchild => Effect::Noop,
        }
    }
}

impl Isolate for RestartableRootIsolate {
    type Message = LineageMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = tina::RestartableChildDefinition<ChildIsolate>;
    type Call = std::convert::Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            LineageMsg::SpawnChild => {
                let child_capacity = self.child_capacity;
                let factory_calls = Rc::clone(&self.factory_calls);
                Effect::Spawn(tina::RestartableChildDefinition::new(
                    move || {
                        factory_calls.set(factory_calls.get() + 1);
                        ChildIsolate {
                            leaf_capacity: child_capacity,
                        }
                    },
                    child_capacity,
                ))
            }
            LineageMsg::Restart => Effect::RestartChildren,
            LineageMsg::Stop => Effect::Stop,
            LineageMsg::Panic => panic!("panic inside restartable root isolate"),
            LineageMsg::SpawnGrandchild => Effect::Noop,
        }
    }
}

impl Isolate for ChildIsolate {
    type Message = LineageMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = tina::ChildDefinition<LeafIsolate>;
    type Call = std::convert::Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            LineageMsg::SpawnGrandchild => {
                Effect::Spawn(tina::ChildDefinition::new(LeafIsolate, self.leaf_capacity))
            }
            LineageMsg::Stop => Effect::Stop,
            LineageMsg::Panic => panic!("panic inside child lineage isolate"),
            LineageMsg::Restart => Effect::RestartChildren,
            LineageMsg::SpawnChild => Effect::Noop,
        }
    }
}

impl Isolate for LeafIsolate {
    type Message = LineageMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = std::convert::Infallible;
    type Call = std::convert::Infallible;
    type Shard = TestShard;

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        Effect::Noop
    }
}

fn new_root() -> RootIsolate {
    RootIsolate { child_capacity: 2 }
}

fn new_restartable_root(factory_calls: Rc<Cell<usize>>) -> RestartableRootIsolate {
    RestartableRootIsolate {
        child_capacity: 3,
        factory_calls,
    }
}

fn root_mailbox() -> TestMailbox<LineageMsg> {
    TestMailbox::new(8)
}

fn assert_root_and_child_lineage(
    runtime: &Runtime<TestShard, TestMailboxFactory>,
    root: Address<LineageMsg>,
    child: IsolateId,
) {
    assert_eq!(
        runtime.lineage_snapshot(),
        vec![(root.isolate(), None), (child, Some(root.isolate()))]
    );
}

fn assert_root_child_grandchild_lineage(
    runtime: &Runtime<TestShard, TestMailboxFactory>,
    root: Address<LineageMsg>,
    child: IsolateId,
    grandchild: IsolateId,
) {
    assert_eq!(
        runtime.lineage_snapshot(),
        vec![
            (root.isolate(), None),
            (child, Some(root.isolate())),
            (grandchild, Some(child)),
        ]
    );
}

fn lineage_address(isolate: IsolateId) -> Address<LineageMsg> {
    Address::new(ShardId::new(3), isolate)
}

fn last_spawned_child(trace: &[RuntimeEvent]) -> IsolateId {
    match trace.last().expect("expected spawn event").kind() {
        RuntimeEventKind::Spawned { child_isolate } => child_isolate,
        other => panic!("expected Spawned event, found {other:?}"),
    }
}

fn child_record(
    parent: IsolateId,
    child_isolate: IsolateId,
    child_ordinal: usize,
    mailbox_capacity: usize,
    restartable: bool,
) -> ChildRecordSnapshot {
    child_record_with_generation(
        parent,
        child_isolate,
        AddressGeneration::new(0),
        child_ordinal,
        mailbox_capacity,
        restartable,
    )
}

fn child_record_with_generation(
    parent: IsolateId,
    child_isolate: IsolateId,
    child_generation: AddressGeneration,
    child_ordinal: usize,
    mailbox_capacity: usize,
    restartable: bool,
) -> ChildRecordSnapshot {
    ChildRecordSnapshot {
        parent,
        child_shard: ShardId::new(3),
        child_isolate,
        child_generation,
        child_ordinal,
        mailbox_capacity,
        restartable,
    }
}

fn replacement_address(record: &ChildRecordSnapshot) -> Address<LineageMsg> {
    Address::new_with_generation(
        record.child_shard,
        record.child_isolate,
        record.child_generation,
    )
}

fn count_events(trace: &[RuntimeEvent], matches_event: impl Fn(RuntimeEventKind) -> bool) -> usize {
    trace
        .iter()
        .filter(|event| matches_event(event.kind()))
        .count()
}

fn restart_child_events(trace: &[RuntimeEvent]) -> Vec<RuntimeEventKind> {
    trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::RestartChildAttempted { .. }
            | RuntimeEventKind::RestartChildSkipped { .. }
            | RuntimeEventKind::RestartChildCompleted { .. } => Some(event.kind()),
            _ => None,
        })
        .collect()
}

fn supervisor_events(trace: &[RuntimeEvent]) -> Vec<RuntimeEventKind> {
    trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::SupervisorRestartTriggered { .. }
            | RuntimeEventKind::SupervisorRestartRejected { .. } => Some(event.kind()),
            _ => None,
        })
        .collect()
}

#[test]
fn root_registered_isolates_have_no_parent() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);

    let first = runtime.register(new_root(), root_mailbox());
    let second = runtime.register(new_root(), root_mailbox());

    assert_eq!(
        runtime.lineage_snapshot(),
        vec![(first.isolate(), None), (second.isolate(), None)]
    );
    assert_eq!(runtime.child_record_snapshot(), Vec::new());
}

#[test]
fn one_shot_spawn_records_non_restartable_child_metadata() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());

    assert_eq!(
        runtime.child_record_snapshot(),
        vec![child_record(root.isolate(), child, 0, 2, false)]
    );
}

#[test]
fn restartable_spawn_records_restartable_child_metadata_and_calls_factory_once() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());

    assert_eq!(factory_calls.get(), 1);
    assert_eq!(
        runtime.child_record_snapshot(),
        vec![child_record(root.isolate(), child, 0, 3, true)]
    );
}

#[test]
fn per_parent_child_ordinals_increment_by_direct_spawn_order() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let first_child = last_spawned_child(runtime.trace());

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let second_child = last_spawned_child(runtime.trace());

    assert_eq!(
        runtime.child_record_snapshot(),
        vec![
            child_record(root.isolate(), first_child, 0, 2, false),
            child_record(root.isolate(), second_child, 1, 2, false),
        ]
    );
}

#[test]
fn nested_spawns_record_direct_parent_edges() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());

    assert_root_and_child_lineage(&runtime, root, child);

    assert_eq!(
        runtime.try_send(lineage_address(child), LineageMsg::SpawnGrandchild),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);
    let grandchild = last_spawned_child(runtime.trace());

    assert_root_child_grandchild_lineage(&runtime, root, child, grandchild);
    assert_eq!(
        runtime.child_record_snapshot(),
        vec![
            child_record(root.isolate(), child, 0, 2, false),
            child_record(child, grandchild, 0, 2, false),
        ]
    );
}

#[test]
fn child_lineage_survives_when_parent_stops() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());

    assert_eq!(runtime.try_send(root, LineageMsg::Stop), Ok(()));
    assert_eq!(runtime.step(), 1);

    assert_root_and_child_lineage(&runtime, root, child);
    assert_eq!(
        runtime.child_record_snapshot(),
        vec![child_record(root.isolate(), child, 0, 2, false)]
    );
}

#[test]
fn child_lineage_survives_when_parent_panics() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());

    assert_eq!(runtime.try_send(root, LineageMsg::Panic), Ok(()));
    assert_eq!(runtime.step(), 1);
    assert!(
        runtime
            .trace()
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::HandlerPanicked))
    );

    assert_root_and_child_lineage(&runtime, root, child);
    assert_eq!(
        runtime.child_record_snapshot(),
        vec![child_record(root.isolate(), child, 0, 2, false)]
    );
}

#[test]
fn child_record_survives_when_child_stops_or_panics() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let stopping_child = last_spawned_child(runtime.trace());

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let panicking_child = last_spawned_child(runtime.trace());

    assert_eq!(
        runtime.try_send(lineage_address(stopping_child), LineageMsg::Stop),
        Ok(())
    );
    assert_eq!(
        runtime.try_send(lineage_address(panicking_child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 2);

    assert_eq!(
        runtime.child_record_snapshot(),
        vec![
            child_record(root.isolate(), stopping_child, 0, 2, false),
            child_record(root.isolate(), panicking_child, 1, 2, false),
        ]
    );
}

#[test]
fn restart_children_with_no_direct_children_emits_no_restart_subtree() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());

    assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.lineage_snapshot(), vec![(root.isolate(), None)]);
    assert_eq!(restart_child_events(runtime.trace()), Vec::new());
    assert!(runtime.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::HandlerFinished {
                effect: EffectKind::RestartChildren,
            }
        )
    }));
}

#[test]
fn restart_children_replaces_restartable_child_and_preserves_ordinal() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let old_child = last_spawned_child(runtime.trace());
    assert_eq!(factory_calls.get(), 1);

    assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
    assert_eq!(runtime.step(), 1);
    let records = runtime.child_record_snapshot();
    let replacement = records[0].child_isolate;

    assert_ne!(replacement, old_child);
    assert_eq!(factory_calls.get(), 2);
    assert_eq!(
        records,
        vec![child_record(root.isolate(), replacement, 0, 3, true)]
    );
    assert!(matches!(
        runtime.try_send(lineage_address(old_child), LineageMsg::SpawnChild),
        Err(TrySendError::Closed(LineageMsg::SpawnChild))
    ));
    assert_eq!(
        runtime.try_send(replacement_address(&records[0]), LineageMsg::SpawnChild),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    let restart_events = restart_child_events(runtime.trace());
    assert_eq!(restart_events.len(), 2);
    assert!(matches!(
        restart_events[0],
        RuntimeEventKind::RestartChildAttempted {
            child_ordinal: 0,
            old_isolate,
            old_generation,
        } if old_isolate == old_child && old_generation == AddressGeneration::new(0)
    ));
    assert!(matches!(
        restart_events[1],
        RuntimeEventKind::RestartChildCompleted {
            child_ordinal: 0,
            old_isolate,
            old_generation,
            new_isolate,
            new_generation,
        } if old_isolate == old_child
            && old_generation == AddressGeneration::new(0)
            && new_isolate == replacement
            && new_generation == AddressGeneration::new(0)
    ));

    let attempt_id = runtime
        .trace()
        .iter()
        .find(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::RestartChildAttempted {
                    old_isolate,
                    ..
                } if old_isolate == old_child
            )
        })
        .expect("expected restart attempt")
        .id();
    let direct_consequences: Vec<_> = runtime
        .trace()
        .iter()
        .filter(|event| event.cause() == Some(CauseId::new(attempt_id)))
        .map(|event| event.kind())
        .collect();
    assert!(
        direct_consequences
            .iter()
            .any(|kind| matches!(kind, RuntimeEventKind::IsolateStopped))
    );
    assert!(
        direct_consequences
            .iter()
            .any(|kind| matches!(kind, RuntimeEventKind::RestartChildCompleted { .. }))
    );
}

#[test]
fn restart_children_abandons_precollected_old_child_message() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let old_child = last_spawned_child(runtime.trace());

    assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
    assert_eq!(
        runtime.try_send(lineage_address(old_child), LineageMsg::SpawnGrandchild),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    assert_eq!(
        count_events(runtime.trace(), |kind| matches!(
            kind,
            RuntimeEventKind::MessageAbandoned
        )),
        1
    );
    assert_eq!(
        count_events(runtime.trace(), |kind| matches!(
            kind,
            RuntimeEventKind::RestartChildCompleted {
                old_isolate,
                ..
            } if old_isolate == old_child
        )),
        1
    );
    assert_eq!(runtime.child_record_snapshot().len(), 1);
}

#[test]
fn restart_children_restarts_already_stopped_or_panicked_children_without_duplicate_stop() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let stopped_child = last_spawned_child(runtime.trace());
    assert_eq!(
        runtime.try_send(lineage_address(stopped_child), LineageMsg::Stop),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let panicked_child = last_spawned_child(runtime.trace());
    assert_eq!(
        runtime.try_send(lineage_address(panicked_child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
    assert_eq!(runtime.step(), 1);

    assert_eq!(
        count_events(runtime.trace(), |kind| matches!(
            kind,
            RuntimeEventKind::IsolateStopped
        )),
        2
    );
    assert_eq!(factory_calls.get(), 4);
    let records = runtime.child_record_snapshot();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].child_ordinal, 0);
    assert_eq!(records[1].child_ordinal, 1);
    assert_ne!(records[0].child_isolate, stopped_child);
    assert_ne!(records[1].child_isolate, panicked_child);
}

#[test]
fn restart_children_visits_multiple_children_in_child_ordinal_order() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let first_old = last_spawned_child(runtime.trace());
    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let second_old = last_spawned_child(runtime.trace());

    assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
    assert_eq!(runtime.step(), 1);

    let attempted: Vec<_> = runtime
        .trace()
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::RestartChildAttempted {
                child_ordinal,
                old_isolate,
                ..
            } => Some((child_ordinal, old_isolate)),
            _ => None,
        })
        .collect();

    assert_eq!(attempted, vec![(0, first_old), (1, second_old)]);
    assert_eq!(factory_calls.get(), 4);
}

#[test]
fn restart_children_skips_non_restartable_children_with_trace() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());

    assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
    assert_eq!(runtime.step(), 1);

    assert_eq!(
        runtime.child_record_snapshot(),
        vec![child_record(root.isolate(), child, 0, 2, false)]
    );
    assert!(restart_child_events(runtime.trace()).iter().any(|kind| {
        matches!(
            kind,
            RuntimeEventKind::RestartChildSkipped {
                child_ordinal: 0,
                old_isolate,
                old_generation,
                reason: RestartSkippedReason::NotRestartable,
            } if *old_isolate == child && *old_generation == AddressGeneration::new(0)
        )
    }));
}

#[test]
fn restart_children_does_not_restart_grandchildren() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());
    assert_eq!(
        runtime.try_send(lineage_address(child), LineageMsg::SpawnGrandchild),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);
    let grandchild = last_spawned_child(runtime.trace());

    assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
    assert_eq!(runtime.step(), 1);

    let records = runtime.child_record_snapshot();
    assert_eq!(records.len(), 2);
    assert_ne!(records[0].child_isolate, child);
    assert_eq!(records[0].child_ordinal, 0);
    assert_eq!(records[1], child_record(child, grandchild, 0, 3, false));
    assert_eq!(
        runtime.try_send(lineage_address(grandchild), LineageMsg::SpawnChild),
        Ok(())
    );
}

#[test]
fn restart_children_can_restart_child_before_its_first_turn() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let old_child = last_spawned_child(runtime.trace());

    assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
    assert_eq!(runtime.step(), 1);

    assert_eq!(
        count_events(runtime.trace(), |kind| matches!(
            kind,
            RuntimeEventKind::HandlerStarted
        )),
        2
    );
    assert_ne!(runtime.child_record_snapshot()[0].child_isolate, old_child);
}

mod supervision;

#[test]
fn identical_runs_produce_identical_trace_and_lineage() {
    fn run_once() -> RunEvidence {
        let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(new_root(), root_mailbox());

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let child = last_spawned_child(runtime.trace());

        assert_eq!(
            runtime.try_send(lineage_address(child), LineageMsg::SpawnGrandchild),
            Ok(())
        );
        assert_eq!(runtime.step(), 1);
        let grandchild = last_spawned_child(runtime.trace());

        assert_root_child_grandchild_lineage(&runtime, root, child, grandchild);

        (
            runtime.trace().to_vec(),
            runtime.lineage_snapshot(),
            runtime.child_record_snapshot(),
        )
    }

    assert_eq!(run_once(), run_once());
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OverlapAcceptorMsg {
    Bootstrap,
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
    Accepted {
        stream: StreamId,
    },
    Failed,
}

#[derive(Debug)]
struct OverlapAcceptor {
    bind_addr: SocketAddr,
    bound_addr_slot: Arc<Mutex<Option<SocketAddr>>>,
    accepted_streams: Arc<Mutex<Vec<StreamId>>>,
    listener: Option<ListenerId>,
}

impl Isolate for OverlapAcceptor {
    type Message = OverlapAcceptorMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = RuntimeCall<OverlapAcceptorMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            OverlapAcceptorMsg::Bootstrap => {
                let addr = self.bind_addr;
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpBind { addr },
                    move |result| match result {
                        CallOutput::TcpBound {
                            listener,
                            local_addr,
                        } => OverlapAcceptorMsg::Bound {
                            listener,
                            addr: local_addr,
                        },
                        CallOutput::Failed(_) => OverlapAcceptorMsg::Failed,
                        other => panic!("unexpected bind result {other:?}"),
                    },
                ))
            }
            OverlapAcceptorMsg::Bound { listener, addr } => {
                self.listener = Some(listener);
                *self.bound_addr_slot.lock().expect("bound addr mutex") = Some(addr);
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpAccept { listener },
                    |result| match result {
                        CallOutput::TcpAccepted { stream, .. } => {
                            OverlapAcceptorMsg::Accepted { stream }
                        }
                        CallOutput::Failed(_) => OverlapAcceptorMsg::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            OverlapAcceptorMsg::Accepted { stream } => {
                let mut accepted = self.accepted_streams.lock().expect("accepted mutex");
                accepted.push(stream);
                if accepted.len() < 2 {
                    let listener = self.listener.expect("listener stored before re-arm");
                    Effect::Call(RuntimeCall::new(
                        CallInput::TcpAccept { listener },
                        |result| match result {
                            CallOutput::TcpAccepted { stream, .. } => {
                                OverlapAcceptorMsg::Accepted { stream }
                            }
                            CallOutput::Failed(_) => OverlapAcceptorMsg::Failed,
                            other => panic!("unexpected accept result {other:?}"),
                        },
                    ))
                } else {
                    Effect::Noop
                }
            }
            OverlapAcceptorMsg::Failed => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderMsg {
    Start,
    ReadCompleted,
    Failed,
}

#[derive(Debug)]
struct Reader {
    stream: StreamId,
}

impl Isolate for Reader {
    type Message = ReaderMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ReaderMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ReaderMsg::Start => Effect::Call(RuntimeCall::new(
                CallInput::TcpRead {
                    stream: self.stream,
                    max_len: 16,
                },
                |result| match result {
                    CallOutput::TcpRead { .. } => ReaderMsg::ReadCompleted,
                    CallOutput::Failed(_) => ReaderMsg::Failed,
                    other => panic!("unexpected read result {other:?}"),
                },
            )),
            ReaderMsg::ReadCompleted | ReaderMsg::Failed => Effect::Stop,
        }
    }
}

#[test]
fn two_stream_reads_can_be_pending_in_io_backend_at_once() {
    let bound_addr = Arc::new(Mutex::new(None));
    let accepted_streams = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let acceptor = runtime.register(
        OverlapAcceptor {
            bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
            bound_addr_slot: Arc::clone(&bound_addr),
            accepted_streams: Arc::clone(&accepted_streams),
            listener: None,
        },
        TestMailbox::new(8),
    );

    assert_eq!(
        runtime.try_send(acceptor, OverlapAcceptorMsg::Bootstrap),
        Ok(())
    );

    let bind_deadline = Instant::now() + Duration::from_secs(2);
    while bound_addr.lock().expect("bound addr mutex").is_none() {
        assert!(
            Instant::now() <= bind_deadline,
            "timed out waiting for bind"
        );
        runtime.step();
        thread::sleep(Duration::from_millis(2));
    }
    let local_addr = bound_addr
        .lock()
        .expect("bound addr mutex")
        .expect("listener published address");

    let release_clients = Arc::new(AtomicBool::new(false));
    let mut clients = Vec::new();
    for _ in 0..2 {
        let release_clients = Arc::clone(&release_clients);
        clients.push(thread::spawn(move || {
            let mut stream = TcpStream::connect(local_addr).expect("connect");
            stream.set_nodelay(true).ok();
            while !release_clients.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(2));
            }
            let _ = stream.write_all(b"done");
        }));
    }

    let accept_deadline = Instant::now() + Duration::from_secs(2);
    while accepted_streams.lock().expect("accepted mutex").len() < 2 {
        assert!(
            Instant::now() <= accept_deadline,
            "timed out waiting for two accepted streams"
        );
        runtime.step();
        thread::sleep(Duration::from_millis(2));
    }

    let streams = accepted_streams.lock().expect("accepted mutex").clone();
    for stream in streams {
        let reader = runtime.register(Reader { stream }, TestMailbox::new(8));
        assert_eq!(runtime.try_send(reader, ReaderMsg::Start), Ok(()));
    }

    assert_eq!(runtime.step(), 2);
    assert_eq!(runtime.io_pending_count(), 2);

    release_clients.store(true, Ordering::SeqCst);
    for client in clients {
        client.join().expect("client thread");
    }
}

// ---------------------------------------------------------------------------
// Timer semantics tests (Phase 015)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerMsg {
    StartSleep,
    Fired,
    StopNow,
}

#[derive(Debug)]
struct Sleeper {
    delay: Duration,
}

impl Isolate for Sleeper {
    type Message = TimerMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = RuntimeCall<TimerMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TimerMsg::StartSleep => Effect::Call(RuntimeCall::new(
                CallInput::Sleep { after: self.delay },
                |result| match result {
                    CallOutput::TimerFired => TimerMsg::Fired,
                    other => panic!("expected TimerFired, got {other:?}"),
                },
            )),
            TimerMsg::Fired => Effect::Noop,
            TimerMsg::StopNow => Effect::Stop,
        }
    }
}

fn new_manual_runtime() -> (Runtime<TestShard, TestMailboxFactory>, Rc<ManualClock>) {
    let clock = Rc::new(ManualClock::new());
    let runtime = Runtime::with_clock(TestShard, TestMailboxFactory, Box::new(Rc::clone(&clock)));
    (runtime, clock)
}

#[derive(Debug)]
struct NumberedSleeper<S> {
    delay: Duration,
    marker: PhantomData<S>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepEvent {
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteEvent {
    Kick,
    KickTwice,
    KickThrice,
    Arrived,
    StopNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShardLocalSupervisionEvent {
    SpawnChild,
    Panic,
    Noop,
}

#[derive(Debug)]
struct NumberedRecorder<S> {
    marker: PhantomData<S>,
}

#[derive(Debug)]
struct RemoteSender<S> {
    target: Address<RemoteEvent>,
    marker: PhantomData<S>,
}

#[derive(Debug)]
struct RemoteSink<S> {
    marker: PhantomData<S>,
}

#[derive(Debug)]
struct ShardLocalParent<S> {
    marker: PhantomData<S>,
}

#[derive(Debug)]
struct ShardLocalChild<S> {
    marker: PhantomData<S>,
}

impl<S> Isolate for NumberedRecorder<S>
where
    S: Shard + 'static,
{
    type Message = StepEvent;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = S;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            StepEvent::Tick => Effect::Noop,
        }
    }
}

impl<S> Isolate for RemoteSender<S>
where
    S: Shard + 'static,
{
    type Message = RemoteEvent;
    type Reply = ();
    type Send = Outbound<RemoteEvent>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = S;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            RemoteEvent::Kick => send(self.target, RemoteEvent::Arrived),
            RemoteEvent::KickTwice => batch([
                send(self.target, RemoteEvent::Arrived),
                send(self.target, RemoteEvent::Arrived),
            ]),
            RemoteEvent::KickThrice => batch([
                send(self.target, RemoteEvent::Arrived),
                send(self.target, RemoteEvent::Arrived),
                send(self.target, RemoteEvent::Arrived),
            ]),
            RemoteEvent::Arrived => noop(),
            RemoteEvent::StopNow => stop(),
        }
    }
}

impl<S> Isolate for RemoteSink<S>
where
    S: Shard + 'static,
{
    type Message = RemoteEvent;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = S;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            RemoteEvent::Arrived
            | RemoteEvent::Kick
            | RemoteEvent::KickTwice
            | RemoteEvent::KickThrice => noop(),
            RemoteEvent::StopNow => stop(),
        }
    }
}

impl<S> Isolate for ShardLocalParent<S>
where
    S: Shard + 'static,
{
    type Message = ShardLocalSupervisionEvent;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = tina::RestartableChildDefinition<ShardLocalChild<S>>;
    type Call = Infallible;
    type Shard = S;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ShardLocalSupervisionEvent::SpawnChild => spawn(tina::RestartableChildDefinition::new(
                || ShardLocalChild {
                    marker: PhantomData,
                },
                4,
            )),
            ShardLocalSupervisionEvent::Panic => panic!("parent should not panic in this test"),
            ShardLocalSupervisionEvent::Noop => noop(),
        }
    }
}

impl<S> Isolate for ShardLocalChild<S>
where
    S: Shard + 'static,
{
    type Message = ShardLocalSupervisionEvent;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = S;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ShardLocalSupervisionEvent::Panic => panic!("child panicked for restart test"),
            ShardLocalSupervisionEvent::SpawnChild | ShardLocalSupervisionEvent::Noop => noop(),
        }
    }
}

impl<S> Isolate for NumberedSleeper<S>
where
    S: Shard + 'static,
{
    type Message = TimerMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = RuntimeCall<TimerMsg>;
    type Shard = S;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TimerMsg::StartSleep => Effect::Call(RuntimeCall::new(
                CallInput::Sleep { after: self.delay },
                |result| match result {
                    CallOutput::TimerFired => TimerMsg::Fired,
                    other => panic!("expected TimerFired, got {other:?}"),
                },
            )),
            TimerMsg::Fired => Effect::Noop,
            TimerMsg::StopNow => Effect::Stop,
        }
    }
}

#[test]
fn single_timer_wakes_after_deadline() {
    let (mut runtime, clock) = new_manual_runtime();
    let sleeper = runtime.register(
        Sleeper {
            delay: Duration::from_millis(10),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(sleeper, TimerMsg::StartSleep).unwrap();
    assert_eq!(runtime.step(), 1);
    assert!(runtime.has_in_flight_calls());

    clock.advance(Duration::from_millis(5));
    assert_eq!(runtime.step(), 0);
    assert!(runtime.has_in_flight_calls());

    clock.advance(Duration::from_millis(5));
    assert_eq!(runtime.step(), 1);
    assert!(!runtime.has_in_flight_calls());

    let trace = runtime.trace();
    assert!(
        trace.iter().any(|e| matches!(
            e.kind(),
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::Sleep,
                ..
            }
        )),
        "trace must show CallCompleted for Sleep"
    );
}

#[test]
fn sibling_runtimes_can_share_global_event_and_call_id_sources() {
    let ids = IdSource::new();
    let first_clock = Rc::new(ManualClock::new());
    let second_clock = Rc::new(ManualClock::new());
    let mut first = Runtime::with_clock_and_ids(
        NumberedShard(11),
        TestMailboxFactory,
        Box::new(Rc::clone(&first_clock)),
        ids.clone(),
    );
    let mut second = Runtime::with_clock_and_ids(
        NumberedShard(22),
        TestMailboxFactory,
        Box::new(Rc::clone(&second_clock)),
        ids,
    );

    let first_sleeper = first.register(
        NumberedSleeper::<NumberedShard> {
            delay: Duration::from_millis(5),
            marker: PhantomData,
        },
        TestMailbox::new(4),
    );
    let second_sleeper = second.register(
        NumberedSleeper::<NumberedShard> {
            delay: Duration::from_millis(7),
            marker: PhantomData,
        },
        TestMailbox::new(4),
    );

    first.try_send(first_sleeper, TimerMsg::StartSleep).unwrap();
    second
        .try_send(second_sleeper, TimerMsg::StartSleep)
        .unwrap();

    assert_eq!(first.step(), 1);
    assert_eq!(second.step(), 1);

    let first_event_ids: Vec<_> = first.trace().iter().map(|event| event.id().get()).collect();
    let second_event_ids: Vec<_> = second
        .trace()
        .iter()
        .map(|event| event.id().get())
        .collect();
    assert_eq!(first_event_ids, vec![1, 2, 3, 4]);
    assert_eq!(second_event_ids, vec![5, 6, 7, 8]);

    let first_call_ids: Vec<_> = first
        .trace()
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::CallDispatchAttempted { call_id, .. } => Some(call_id.get()),
            _ => None,
        })
        .collect();
    let second_call_ids: Vec<_> = second
        .trace()
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::CallDispatchAttempted { call_id, .. } => Some(call_id.get()),
            _ => None,
        })
        .collect();
    assert_eq!(first_call_ids, vec![1]);
    assert_eq!(second_call_ids, vec![2]);
}

#[test]
fn multishard_runtime_routes_ingress_and_steps_in_ascending_shard_order() {
    let mut runtime =
        MultiShardRuntime::new([NumberedShard(22), NumberedShard(11)], TestMailboxFactory);

    assert_eq!(
        runtime.shard_ids(),
        vec![ShardId::new(11), ShardId::new(22)]
    );

    let shard_twenty_two = runtime.register_with_capacity_on::<NumberedRecorder<NumberedShard>, _>(
        ShardId::new(22),
        NumberedRecorder {
            marker: PhantomData,
        },
        4,
    );
    let shard_eleven = runtime.register_with_capacity_on::<NumberedRecorder<NumberedShard>, _>(
        ShardId::new(11),
        NumberedRecorder {
            marker: PhantomData,
        },
        4,
    );

    assert_eq!(shard_twenty_two.shard(), ShardId::new(22));
    assert_eq!(shard_eleven.shard(), ShardId::new(11));

    runtime.try_send(shard_twenty_two, StepEvent::Tick).unwrap();
    runtime.try_send(shard_eleven, StepEvent::Tick).unwrap();

    assert_eq!(runtime.step(), 2);

    let handler_shards: Vec<_> = runtime
        .trace()
        .into_iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::HandlerStarted => Some(event.shard().get()),
            _ => None,
        })
        .collect();
    assert_eq!(handler_shards, vec![11, 22]);
}

#[test]
fn cross_shard_send_becomes_visible_on_the_next_global_step() {
    let mut runtime =
        MultiShardRuntime::new([NumberedShard(11), NumberedShard(22)], TestMailboxFactory);

    let sink = runtime.register_with_capacity_on::<RemoteSink<NumberedShard>, _>(
        ShardId::new(22),
        RemoteSink {
            marker: PhantomData,
        },
        4,
    );
    let sender = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(11),
        RemoteSender {
            target: sink,
            marker: PhantomData,
        },
        4,
    );

    runtime.try_send(sender, RemoteEvent::Kick).unwrap();

    assert_eq!(runtime.step(), 1);
    let step_one_trace = runtime.trace();
    let step_one_sink_handlers = step_one_trace
        .iter()
        .filter(|event| {
            event.shard() == ShardId::new(22)
                && matches!(event.kind(), RuntimeEventKind::HandlerStarted)
        })
        .count();
    assert_eq!(step_one_sink_handlers, 0);

    assert_eq!(runtime.step(), 1);
    let trace = runtime.trace();

    let accepted_shard11 = trace.iter().any(|event| {
        event.shard() == ShardId::new(11)
            && matches!(event.kind(), RuntimeEventKind::SendAccepted { .. })
    });
    assert!(accepted_shard11);

    let harvested_on_22 = trace.iter().any(|event| {
        event.shard() == ShardId::new(22)
            && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
    });
    assert!(harvested_on_22);

    let sink_handlers: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::HandlerStarted if event.shard() == ShardId::new(22) => {
                Some(event.id().get())
            }
            _ => None,
        })
        .collect();
    assert_eq!(sink_handlers.len(), 1);
}

#[test]
fn cross_shard_queue_overflow_rejects_at_source_time() {
    let mut runtime = MultiShardRuntime::with_config(
        [NumberedShard(11), NumberedShard(22)],
        TestMailboxFactory,
        MultiShardRuntimeConfig {
            shard_pair_capacity: 1,
        },
    );

    let sink = runtime.register_with_capacity_on::<RemoteSink<NumberedShard>, _>(
        ShardId::new(22),
        RemoteSink {
            marker: PhantomData,
        },
        4,
    );
    let sender = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(11),
        RemoteSender {
            target: sink,
            marker: PhantomData,
        },
        4,
    );

    runtime.try_send(sender, RemoteEvent::KickTwice).unwrap();

    assert_eq!(runtime.step(), 1);

    let trace = runtime.trace();
    let full_rejections: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::SendRejected { reason, .. }
                if event.shard() == ShardId::new(11) && reason == SendRejectedReason::Full =>
            {
                Some(event.id().get())
            }
            _ => None,
        })
        .collect();
    assert_eq!(full_rejections.len(), 1);
}

#[test]
fn cross_shard_closed_target_rejects_on_destination_harvest() {
    let mut runtime =
        MultiShardRuntime::new([NumberedShard(11), NumberedShard(22)], TestMailboxFactory);

    let sink = runtime.register_with_capacity_on::<RemoteSink<NumberedShard>, _>(
        ShardId::new(22),
        RemoteSink {
            marker: PhantomData,
        },
        4,
    );
    let sender = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(11),
        RemoteSender {
            target: sink,
            marker: PhantomData,
        },
        4,
    );

    runtime.try_send(sender, RemoteEvent::Kick).unwrap();
    runtime.try_send(sink, RemoteEvent::StopNow).unwrap();

    assert_eq!(runtime.step(), 2);
    assert_eq!(runtime.step(), 0);

    let trace = runtime.trace();
    let closed_rejections: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::SendRejected { reason, .. }
                if event.shard() == ShardId::new(22) && reason == SendRejectedReason::Closed =>
            {
                Some(event.id().get())
            }
            _ => None,
        })
        .collect();
    assert_eq!(closed_rejections.len(), 1);
}

#[test]
fn cross_shard_unknown_isolate_rejects_on_destination_harvest() {
    let mut runtime =
        MultiShardRuntime::new([NumberedShard(11), NumberedShard(22)], TestMailboxFactory);

    let unknown = Address::new_with_generation(
        ShardId::new(22),
        IsolateId::new(999),
        AddressGeneration::new(0),
    );
    let sender = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(11),
        RemoteSender {
            target: unknown,
            marker: PhantomData,
        },
        4,
    );

    runtime.try_send(sender, RemoteEvent::Kick).unwrap();

    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 0);

    let trace = runtime.trace();
    let destination_closed_rejections: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::SendRejected {
                target_isolate,
                reason,
                ..
            } if event.shard() == ShardId::new(22)
                && target_isolate == unknown.isolate()
                && reason == SendRejectedReason::Closed =>
            {
                Some(event.id().get())
            }
            _ => None,
        })
        .collect();
    assert_eq!(destination_closed_rejections.len(), 1);
}

#[test]
fn cross_shard_unknown_isolate_does_not_poison_destination_shard() {
    let mut runtime =
        MultiShardRuntime::new([NumberedShard(11), NumberedShard(22)], TestMailboxFactory);

    let unknown = Address::new_with_generation(
        ShardId::new(22),
        IsolateId::new(999),
        AddressGeneration::new(0),
    );
    let live_sink = runtime.register_with_capacity_on::<RemoteSink<NumberedShard>, _>(
        ShardId::new(22),
        RemoteSink {
            marker: PhantomData,
        },
        4,
    );
    let bad_sender = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(11),
        RemoteSender {
            target: unknown,
            marker: PhantomData,
        },
        4,
    );
    let good_sender = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(11),
        RemoteSender {
            target: live_sink,
            marker: PhantomData,
        },
        4,
    );

    runtime.try_send(bad_sender, RemoteEvent::Kick).unwrap();
    runtime.try_send(good_sender, RemoteEvent::Kick).unwrap();

    assert_eq!(runtime.step(), 2);
    assert_eq!(runtime.step(), 1);

    let trace = runtime.trace();
    let unknown_rejection = trace
        .iter()
        .find(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    target_isolate,
                    reason: SendRejectedReason::Closed,
                    ..
                } if event.shard() == ShardId::new(22)
                    && target_isolate == unknown.isolate()
            )
        })
        .expect("unknown remote target should reject on destination shard");
    let live_accept = trace
        .iter()
        .find(|event| {
            event.shard() == ShardId::new(22)
                && event.isolate() == live_sink.isolate()
                && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
        })
        .expect("later valid traffic to same shard should still be accepted");

    assert!(unknown_rejection.id() < live_accept.id());

    let live_handler_count = trace
        .iter()
        .filter(|event| {
            event.shard() == ShardId::new(22)
                && event.isolate() == live_sink.isolate()
                && matches!(event.kind(), RuntimeEventKind::HandlerStarted)
        })
        .count();
    assert_eq!(live_handler_count, 1);
}

#[test]
fn cross_shard_destination_mailbox_full_rejects_on_harvest() {
    let mut runtime =
        MultiShardRuntime::new([NumberedShard(11), NumberedShard(22)], TestMailboxFactory);

    let sink = runtime.register_with_capacity_on::<RemoteSink<NumberedShard>, _>(
        ShardId::new(22),
        RemoteSink {
            marker: PhantomData,
        },
        1,
    );
    let remote_sender = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(11),
        RemoteSender {
            target: sink,
            marker: PhantomData,
        },
        4,
    );
    let local_filler = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(22),
        RemoteSender {
            target: sink,
            marker: PhantomData,
        },
        4,
    );

    runtime.try_send(remote_sender, RemoteEvent::Kick).unwrap();
    runtime.try_send(local_filler, RemoteEvent::Kick).unwrap();

    assert_eq!(runtime.step(), 2);
    assert_eq!(runtime.step(), 1);

    let trace = runtime.trace();
    let source_accepts = trace
        .iter()
        .filter(|event| {
            event.shard() == ShardId::new(11)
                && matches!(event.kind(), RuntimeEventKind::SendAccepted { .. })
        })
        .count();
    assert_eq!(source_accepts, 1);

    let destination_full_rejections = trace
        .iter()
        .filter(|event| {
            event.shard() == ShardId::new(22)
                && matches!(
                    event.kind(),
                    RuntimeEventKind::SendRejected {
                        reason: SendRejectedReason::Full,
                        ..
                    }
                )
        })
        .count();
    assert_eq!(destination_full_rejections, 1);
}

#[test]
fn cross_shard_harvest_preserves_fifo_from_one_source() {
    let mut runtime =
        MultiShardRuntime::new([NumberedShard(11), NumberedShard(22)], TestMailboxFactory);

    let sink = runtime.register_with_capacity_on::<RemoteSink<NumberedShard>, _>(
        ShardId::new(22),
        RemoteSink {
            marker: PhantomData,
        },
        8,
    );
    let sender = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(11),
        RemoteSender {
            target: sink,
            marker: PhantomData,
        },
        4,
    );

    runtime.try_send(sender, RemoteEvent::KickThrice).unwrap();

    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);

    let trace = runtime.trace();
    let source_attempts: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::SendDispatchAttempted { .. } if event.shard() == ShardId::new(11) => {
                Some(event.id().get())
            }
            _ => None,
        })
        .collect();
    let destination_causes: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::MailboxAccepted if event.shard() == ShardId::new(22) => {
                event.cause().map(|cause| cause.event().get())
            }
            _ => None,
        })
        .collect();
    assert_eq!(destination_causes, source_attempts);
}

#[test]
fn cross_shard_harvest_preserves_fifo_per_isolate_pair_with_multiple_sources_and_targets() {
    let mut runtime =
        MultiShardRuntime::new([NumberedShard(11), NumberedShard(44)], TestMailboxFactory);

    let sink_a = runtime.register_with_capacity_on::<RemoteSink<NumberedShard>, _>(
        ShardId::new(44),
        RemoteSink {
            marker: PhantomData,
        },
        8,
    );
    let sink_b = runtime.register_with_capacity_on::<RemoteSink<NumberedShard>, _>(
        ShardId::new(44),
        RemoteSink {
            marker: PhantomData,
        },
        8,
    );
    let sender_a = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(11),
        RemoteSender {
            target: sink_a,
            marker: PhantomData,
        },
        4,
    );
    let sender_b = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(11),
        RemoteSender {
            target: sink_b,
            marker: PhantomData,
        },
        4,
    );

    runtime.try_send(sender_a, RemoteEvent::KickThrice).unwrap();
    runtime.try_send(sender_b, RemoteEvent::KickTwice).unwrap();

    assert_eq!(runtime.step(), 2);
    assert_eq!(runtime.step(), 2);

    let trace = runtime.trace();
    let sender_a_attempts: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::SendDispatchAttempted { .. }
                if event.shard() == ShardId::new(11) && event.isolate() == sender_a.isolate() =>
            {
                Some(event.id().get())
            }
            _ => None,
        })
        .collect();
    let sender_b_attempts: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::SendDispatchAttempted { .. }
                if event.shard() == ShardId::new(11) && event.isolate() == sender_b.isolate() =>
            {
                Some(event.id().get())
            }
            _ => None,
        })
        .collect();
    let sink_a_causes: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::MailboxAccepted
                if event.shard() == ShardId::new(44) && event.isolate() == sink_a.isolate() =>
            {
                event.cause().map(|cause| cause.event().get())
            }
            _ => None,
        })
        .collect();
    let sink_b_causes: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::MailboxAccepted
                if event.shard() == ShardId::new(44) && event.isolate() == sink_b.isolate() =>
            {
                event.cause().map(|cause| cause.event().get())
            }
            _ => None,
        })
        .collect();

    assert_eq!(sink_a_causes, sender_a_attempts);
    assert_eq!(sink_b_causes, sender_b_attempts);
}

#[test]
fn cross_shard_harvest_drains_sources_in_ascending_shard_order() {
    let mut runtime = MultiShardRuntime::new(
        [
            NumberedShard(33),
            NumberedShard(22),
            NumberedShard(11),
            NumberedShard(44),
        ],
        TestMailboxFactory,
    );

    let sink = runtime.register_with_capacity_on::<RemoteSink<NumberedShard>, _>(
        ShardId::new(44),
        RemoteSink {
            marker: PhantomData,
        },
        8,
    );
    let sender11 = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(11),
        RemoteSender {
            target: sink,
            marker: PhantomData,
        },
        4,
    );
    let sender22 = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(22),
        RemoteSender {
            target: sink,
            marker: PhantomData,
        },
        4,
    );
    let sender33 = runtime.register_with_capacity_on::<RemoteSender<NumberedShard>, _>(
        ShardId::new(33),
        RemoteSender {
            target: sink,
            marker: PhantomData,
        },
        4,
    );

    runtime.try_send(sender33, RemoteEvent::Kick).unwrap();
    runtime.try_send(sender22, RemoteEvent::Kick).unwrap();
    runtime.try_send(sender11, RemoteEvent::Kick).unwrap();

    assert_eq!(runtime.step(), 3);
    assert_eq!(runtime.step(), 1);

    let trace = runtime.trace();
    let source_order: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::SendDispatchAttempted { .. }
                if matches!(event.shard().get(), 11 | 22 | 33) =>
            {
                Some((event.shard().get(), event.id().get()))
            }
            _ => None,
        })
        .collect();
    let destination_causes: Vec<_> = trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::MailboxAccepted if event.shard() == ShardId::new(44) => {
                event.cause().map(|cause| cause.event().get())
            }
            _ => None,
        })
        .collect();

    let expected: Vec<_> = source_order.into_iter().map(|(_, id)| id).collect();
    assert_eq!(destination_causes, expected);
}

#[test]
fn multishard_supervision_keeps_children_on_parent_shard() {
    let mut runtime =
        MultiShardRuntime::new([NumberedShard(11), NumberedShard(22)], TestMailboxFactory);

    let parent = runtime.register_with_capacity_on::<ShardLocalParent<NumberedShard>, _>(
        ShardId::new(22),
        ShardLocalParent {
            marker: PhantomData,
        },
        4,
    );
    runtime.supervise(
        parent,
        SupervisorConfig::new(tina::RestartPolicy::OneForOne, tina::RestartBudget::new(3)),
    );

    runtime
        .try_send(parent, ShardLocalSupervisionEvent::SpawnChild)
        .unwrap();
    assert_eq!(runtime.step(), 1);

    let trace = runtime.trace();
    let child = trace
        .iter()
        .find_map(|event| match event.kind() {
            RuntimeEventKind::Spawned { child_isolate } if event.shard() == ShardId::new(22) => {
                Some(child_isolate)
            }
            _ => None,
        })
        .expect("child spawn should be recorded on the parent shard");
    assert!(
        trace.iter().all(|event| {
            !matches!(event.kind(), RuntimeEventKind::Spawned { .. })
                || event.shard() == ShardId::new(22)
        }),
        "multi-shard supervision must not create children on another shard"
    );

    let child_address = Address::new(ShardId::new(22), child);
    runtime
        .try_send(child_address, ShardLocalSupervisionEvent::Panic)
        .unwrap();
    assert_eq!(runtime.step(), 1);

    let trace = runtime.trace();
    let restart = trace
        .iter()
        .find_map(|event| match event.kind() {
            RuntimeEventKind::RestartChildCompleted {
                old_isolate,
                new_isolate,
                ..
            } if event.shard() == ShardId::new(22) && old_isolate == child => Some(new_isolate),
            _ => None,
        })
        .expect("supervised restart should complete on the parent shard");
    assert_ne!(restart, child);
    assert!(
        trace.iter().all(|event| {
            !matches!(
                event.kind(),
                RuntimeEventKind::SupervisorRestartTriggered { .. }
                    | RuntimeEventKind::RestartChildAttempted { .. }
                    | RuntimeEventKind::RestartChildCompleted { .. }
            ) || event.shard() == ShardId::new(22)
        }),
        "restart events must stay on the parent shard"
    );

    runtime
        .try_send(
            Address::new(ShardId::new(22), restart),
            ShardLocalSupervisionEvent::Noop,
        )
        .unwrap();
}

#[test]
fn timer_does_not_fire_early() {
    let (mut runtime, clock) = new_manual_runtime();
    let sleeper = runtime.register(
        Sleeper {
            delay: Duration::from_millis(100),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(sleeper, TimerMsg::StartSleep).unwrap();
    runtime.step();

    for _ in 0..5 {
        clock.advance(Duration::from_millis(10));
        runtime.step();
    }

    assert!(runtime.has_in_flight_calls());
    let fired_count = runtime
        .trace()
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::Sleep,
                    ..
                }
            )
        })
        .count();
    assert_eq!(fired_count, 0, "timer must not fire before deadline");
}

#[test]
fn timer_fires_exactly_once() {
    let (mut runtime, clock) = new_manual_runtime();
    let sleeper = runtime.register(
        Sleeper {
            delay: Duration::from_millis(10),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(sleeper, TimerMsg::StartSleep).unwrap();
    runtime.step();

    clock.advance(Duration::from_millis(10));
    runtime.step();

    clock.advance(Duration::from_millis(100));
    runtime.step();
    runtime.step();

    let fired_count = runtime
        .trace()
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::Sleep,
                    ..
                }
            )
        })
        .count();
    assert_eq!(fired_count, 1, "timer must fire exactly once");
}

#[test]
fn multiple_timers_wake_in_due_time_order() {
    let (mut runtime, clock) = new_manual_runtime();

    let short = runtime.register(
        Sleeper {
            delay: Duration::from_millis(10),
        },
        TestMailbox::new(4),
    );
    let long = runtime.register(
        Sleeper {
            delay: Duration::from_millis(30),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(short, TimerMsg::StartSleep).unwrap();
    runtime.try_send(long, TimerMsg::StartSleep).unwrap();
    runtime.step();
    runtime.step();

    clock.advance(Duration::from_millis(15));
    runtime.step();

    let fired_order: Vec<IsolateId> = runtime
        .trace()
        .iter()
        .filter_map(|e| match e.kind() {
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::Sleep,
                ..
            } => Some(e.isolate()),
            _ => None,
        })
        .collect();

    assert_eq!(fired_order, vec![short.isolate()]);

    clock.advance(Duration::from_millis(20));
    runtime.step();

    let fired_order: Vec<IsolateId> = runtime
        .trace()
        .iter()
        .filter_map(|e| match e.kind() {
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::Sleep,
                ..
            } => Some(e.isolate()),
            _ => None,
        })
        .collect();

    assert_eq!(fired_order, vec![short.isolate(), long.isolate()]);
}

#[test]
fn equal_deadline_timers_wake_in_request_order() {
    let (mut runtime, clock) = new_manual_runtime();

    let first = runtime.register(
        Sleeper {
            delay: Duration::from_millis(10),
        },
        TestMailbox::new(4),
    );
    let second = runtime.register(
        Sleeper {
            delay: Duration::from_millis(10),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(first, TimerMsg::StartSleep).unwrap();
    runtime.step();
    runtime.try_send(second, TimerMsg::StartSleep).unwrap();
    runtime.step();

    clock.advance(Duration::from_millis(10));
    runtime.step();

    let fired_order: Vec<IsolateId> = runtime
        .trace()
        .iter()
        .filter_map(|e| match e.kind() {
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::Sleep,
                ..
            } => Some(e.isolate()),
            _ => None,
        })
        .collect();

    assert_eq!(fired_order, vec![first.isolate(), second.isolate()]);
}

#[test]
fn pending_timer_completion_is_rejected_when_requester_stops() {
    let (mut runtime, clock) = new_manual_runtime();
    let sleeper = runtime.register(
        Sleeper {
            delay: Duration::from_millis(10),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(sleeper, TimerMsg::StartSleep).unwrap();
    runtime.step();
    runtime.try_send(sleeper, TimerMsg::StopNow).unwrap();
    runtime.step();

    clock.advance(Duration::from_millis(20));
    runtime.step();

    let trace = runtime.trace();
    assert!(
        trace.iter().any(|e| matches!(
            e.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::Sleep,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )),
        "trace must show CallCompletionRejected for stopped timer requester"
    );
    assert!(
        !trace.iter().any(|e| matches!(
            e.kind(),
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::Sleep,
                ..
            }
        )),
        "stopped requester must not observe CallCompleted"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualCallRequest {
    NoReply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ManualCallReply;

#[derive(Debug)]
struct ManualCallTarget;

impl Isolate for ManualCallTarget {
    type Message = ManualCallRequest;
    type Reply = ManualCallReply;
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ManualCallRequest::NoReply => noop(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ManualCallCallerMsg {
    Start(Address<ManualCallRequest>),
    Returned(CallOutcome<ManualCallReply>),
}

#[derive(Debug)]
struct ManualCallCaller {
    outcomes: Rc<RefCell<Vec<CallOutcome<ManualCallReply>>>>,
}

impl Isolate for ManualCallCaller {
    type Message = ManualCallCallerMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ManualCallCallerMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ManualCallCallerMsg::Start(target) => Effect::Call(RuntimeCall::isolate_call(
                target,
                ManualCallRequest::NoReply,
                Duration::from_millis(10),
                ManualCallCallerMsg::Returned,
            )),
            ManualCallCallerMsg::Returned(outcome) => {
                self.outcomes.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn isolate_call_timeout_uses_manual_clock_without_wall_clock_sleep() {
    let (mut runtime, clock) = new_manual_runtime();
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let target = runtime.register(ManualCallTarget, TestMailbox::new(8));
    let caller = runtime.register(
        ManualCallCaller {
            outcomes: Rc::clone(&outcomes),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(caller, ManualCallCallerMsg::Start(target))
        .unwrap();
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);

    clock.advance(Duration::from_millis(9));
    assert_eq!(runtime.step(), 0);
    assert!(outcomes.borrow().is_empty());
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|event| matches!(
                event.kind(),
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    reason: CallError::Timeout,
                    ..
                }
            ))
            .count(),
        0
    );

    clock.advance(Duration::from_millis(1));
    assert_eq!(runtime.step(), 1);
    assert_eq!(outcomes.borrow().as_slice(), [CallOutcome::Timeout]);
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|event| matches!(
                event.kind(),
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    reason: CallError::Timeout,
                    ..
                }
            ))
            .count(),
        1
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryMsg {
    Attempt,
    RetryNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryObservation {
    Attempted(usize),
    Failed(usize),
    BackoffElapsed,
    Succeeded(usize),
}

#[derive(Debug)]
struct RetryWorker {
    backoff: Duration,
    attempts: usize,
    observations: Rc<RefCell<Vec<RetryObservation>>>,
}

impl Isolate for RetryWorker {
    type Message = RetryMsg;
    type Reply = ();
    type Send = Outbound<RetryMsg>;
    type Spawn = Infallible;
    type Call = RuntimeCall<RetryMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            RetryMsg::Attempt => {
                self.attempts += 1;
                self.observations
                    .borrow_mut()
                    .push(RetryObservation::Attempted(self.attempts));
                if self.attempts == 1 {
                    self.observations
                        .borrow_mut()
                        .push(RetryObservation::Failed(self.attempts));
                    Effect::Call(RuntimeCall::new(
                        CallInput::Sleep {
                            after: self.backoff,
                        },
                        |_| RetryMsg::RetryNow,
                    ))
                } else {
                    self.observations
                        .borrow_mut()
                        .push(RetryObservation::Succeeded(self.attempts));
                    Effect::Noop
                }
            }
            RetryMsg::RetryNow => {
                self.observations
                    .borrow_mut()
                    .push(RetryObservation::BackoffElapsed);
                ctx.send_self(RetryMsg::Attempt)
            }
        }
    }
}

#[test]
fn retry_backoff_workload_uses_timer_path() {
    let (mut runtime, clock) = new_manual_runtime();
    let observations = Rc::new(RefCell::new(Vec::new()));
    let worker = runtime.register(
        RetryWorker {
            backoff: Duration::from_millis(50),
            attempts: 0,
            observations: Rc::clone(&observations),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(worker, RetryMsg::Attempt).unwrap();

    // Step 1: first attempt fails and arms one backoff timer.
    assert_eq!(runtime.step(), 1);
    assert!(runtime.has_in_flight_calls());
    assert_eq!(
        observations.borrow().as_slice(),
        [RetryObservation::Attempted(1), RetryObservation::Failed(1),]
    );

    // Not enough time elapsed
    clock.advance(Duration::from_millis(10));
    assert_eq!(runtime.step(), 0);
    assert!(runtime.has_in_flight_calls());
    assert_eq!(
        observations.borrow().as_slice(),
        [RetryObservation::Attempted(1), RetryObservation::Failed(1),]
    );

    // Now the timer fires; the translated RetryNow message is handled and
    // enqueues the real retry attempt for the next step.
    clock.advance(Duration::from_millis(40));
    assert_eq!(runtime.step(), 1);
    assert!(!runtime.has_in_flight_calls());
    assert_eq!(
        observations.borrow().as_slice(),
        [
            RetryObservation::Attempted(1),
            RetryObservation::Failed(1),
            RetryObservation::BackoffElapsed,
        ]
    );

    // Next step performs the real second attempt, which now succeeds.
    assert_eq!(runtime.step(), 1);
    assert!(!runtime.has_in_flight_calls());
    assert_eq!(
        observations.borrow().as_slice(),
        [
            RetryObservation::Attempted(1),
            RetryObservation::Failed(1),
            RetryObservation::BackoffElapsed,
            RetryObservation::Attempted(2),
            RetryObservation::Succeeded(2),
        ]
    );

    let trace = runtime.trace();
    let sleep_completions: Vec<_> = trace
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::Sleep,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        sleep_completions.len(),
        1,
        "trace must show exactly one Sleep completion for the backoff timer"
    );
}
