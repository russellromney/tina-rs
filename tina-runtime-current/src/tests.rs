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
    type Send = SendMessage<NeverOutbound>;
    type Spawn = tina::SpawnSpec<ChildIsolate>;
    type Call = std::convert::Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            LineageMsg::SpawnChild => Effect::Spawn(tina::SpawnSpec::new(
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
    type Send = SendMessage<NeverOutbound>;
    type Spawn = tina::RestartableSpawnSpec<ChildIsolate>;
    type Call = std::convert::Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            LineageMsg::SpawnChild => {
                let child_capacity = self.child_capacity;
                let factory_calls = Rc::clone(&self.factory_calls);
                Effect::Spawn(tina::RestartableSpawnSpec::new(
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
    type Send = SendMessage<NeverOutbound>;
    type Spawn = tina::SpawnSpec<LeafIsolate>;
    type Call = std::convert::Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            LineageMsg::SpawnGrandchild => {
                Effect::Spawn(tina::SpawnSpec::new(LeafIsolate, self.leaf_capacity))
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
    type Send = SendMessage<NeverOutbound>;
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
    runtime: &CurrentRuntime<TestShard, TestMailboxFactory>,
    root: Address<LineageMsg>,
    child: IsolateId,
) {
    assert_eq!(
        runtime.lineage_snapshot(),
        vec![(root.isolate(), None), (child, Some(root.isolate()))]
    );
}

fn assert_root_child_grandchild_lineage(
    runtime: &CurrentRuntime<TestShard, TestMailboxFactory>,
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);

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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
    type Send = SendMessage<NeverOutbound>;
    type Spawn = Infallible;
    type Call = CurrentCall<OverlapAcceptorMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            OverlapAcceptorMsg::Bootstrap => {
                let addr = self.bind_addr;
                Effect::Call(CurrentCall::new(
                    CallRequest::TcpBind { addr },
                    move |result| match result {
                        CallResult::TcpBound {
                            listener,
                            local_addr,
                        } => OverlapAcceptorMsg::Bound {
                            listener,
                            addr: local_addr,
                        },
                        CallResult::Failed(_) => OverlapAcceptorMsg::Failed,
                        other => panic!("unexpected bind result {other:?}"),
                    },
                ))
            }
            OverlapAcceptorMsg::Bound { listener, addr } => {
                self.listener = Some(listener);
                *self.bound_addr_slot.lock().expect("bound addr mutex") = Some(addr);
                Effect::Call(CurrentCall::new(
                    CallRequest::TcpAccept { listener },
                    |result| match result {
                        CallResult::TcpAccepted { stream, .. } => {
                            OverlapAcceptorMsg::Accepted { stream }
                        }
                        CallResult::Failed(_) => OverlapAcceptorMsg::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            OverlapAcceptorMsg::Accepted { stream } => {
                let mut accepted = self.accepted_streams.lock().expect("accepted mutex");
                accepted.push(stream);
                if accepted.len() < 2 {
                    let listener = self.listener.expect("listener stored before re-arm");
                    Effect::Call(CurrentCall::new(
                        CallRequest::TcpAccept { listener },
                        |result| match result {
                            CallResult::TcpAccepted { stream, .. } => {
                                OverlapAcceptorMsg::Accepted { stream }
                            }
                            CallResult::Failed(_) => OverlapAcceptorMsg::Failed,
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
    type Send = SendMessage<NeverOutbound>;
    type Spawn = Infallible;
    type Call = CurrentCall<ReaderMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ReaderMsg::Start => Effect::Call(CurrentCall::new(
                CallRequest::TcpRead {
                    stream: self.stream,
                    max_len: 16,
                },
                |result| match result {
                    CallResult::TcpRead { .. } => ReaderMsg::ReadCompleted,
                    CallResult::Failed(_) => ReaderMsg::Failed,
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
