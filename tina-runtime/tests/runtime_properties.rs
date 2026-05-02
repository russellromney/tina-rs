use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::convert::Infallible;
use std::rc::Rc;

use proptest::prelude::*;
use tina::{
    Address, Context, Effect, Isolate, IsolateId, Mailbox, Outbound, RestartBudget, RestartPolicy,
    Shard, ShardId, TrySendError,
};
use tina_runtime::{
    CauseId, EffectKind, EventId, MailboxFactory, Runtime, RuntimeEvent, RuntimeEventKind,
    SendRejectedReason,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetMsg {
    Data(u8),
    Stop,
    Panic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverMsg {
    Send(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentMsg {
    Spawn,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(7)
    }
}

#[derive(Debug, Clone)]
struct TestMailbox<T> {
    inner: Rc<RefCell<TestMailboxInner<T>>>,
}

#[derive(Debug)]
struct TestMailboxInner<T> {
    capacity: usize,
    closed: bool,
    messages: VecDeque<T>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Rc::new(RefCell::new(TestMailboxInner {
                capacity,
                closed: false,
                messages: VecDeque::new(),
            })),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.inner.borrow().capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        let mut inner = self.inner.borrow_mut();
        if inner.closed {
            return Err(TrySendError::Closed(message));
        }
        if inner.messages.len() >= inner.capacity {
            return Err(TrySendError::Full(message));
        }
        inner.messages.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.inner.borrow_mut().messages.pop_front()
    }

    fn close(&self) {
        self.inner.borrow_mut().closed = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

#[derive(Debug, Default)]
struct Target;

impl Isolate for Target {
    type Message = TargetMsg;
    type Reply = Infallible;
    type Send = Outbound<DriverMsg>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        message: Self::Message,
        _ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match message {
            TargetMsg::Data(_) => Effect::Noop,
            TargetMsg::Stop => Effect::Stop,
            TargetMsg::Panic => panic!("target panic"),
        }
    }
}

#[derive(Debug)]
struct Driver {
    target: Address<TargetMsg>,
}

#[derive(Debug)]
struct RestartParent;

#[derive(Debug)]
struct RestartChild;

impl Isolate for Driver {
    type Message = DriverMsg;
    type Reply = Infallible;
    type Send = Outbound<TargetMsg>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        message: Self::Message,
        _ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match message {
            DriverMsg::Send(value) => {
                Effect::Send(Outbound::new(self.target, TargetMsg::Data(value)))
            }
        }
    }
}

impl Isolate for RestartParent {
    type Message = ParentMsg;
    type Reply = Infallible;
    type Send = Outbound<TargetMsg>;
    type Spawn = tina::RestartableChildDefinition<RestartChild>;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        message: Self::Message,
        _ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match message {
            ParentMsg::Spawn => {
                Effect::Spawn(tina::RestartableChildDefinition::new(|| RestartChild, 3))
            }
            ParentMsg::Restart => Effect::RestartChildren,
        }
    }
}

impl Isolate for RestartChild {
    type Message = TargetMsg;
    type Reply = Infallible;
    type Send = Outbound<TargetMsg>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        message: Self::Message,
        _ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match message {
            TargetMsg::Data(_) => Effect::Noop,
            TargetMsg::Stop => Effect::Stop,
            TargetMsg::Panic => panic!("restart child panic"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    EnqueueTargetData(u8),
    EnqueueTargetStop,
    EnqueueDriverSend(u8),
    EnqueueParentRestart,
    EnqueueChildPanic,
    Step,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngressOutcome {
    Ok,
    Full,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoryResult {
    ingress: Vec<IngressOutcome>,
    steps: Vec<usize>,
    trace: Vec<RuntimeEvent>,
}

fn operation_strategy() -> impl Strategy<Value = Operation> {
    prop_oneof![
        (0_u8..8).prop_map(Operation::EnqueueTargetData),
        Just(Operation::EnqueueTargetStop),
        (0_u8..8).prop_map(Operation::EnqueueDriverSend),
        Just(Operation::EnqueueParentRestart),
        Just(Operation::EnqueueChildPanic),
        Just(Operation::Step),
    ]
}

fn run_history(operations: &[Operation]) -> HistoryResult {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let target = runtime.register(Target, TestMailbox::new(3));
    let driver = runtime.register(Driver { target }, TestMailbox::new(3));
    let parent = runtime.register(RestartParent, TestMailbox::new(3));
    runtime.supervise(
        parent,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(100)),
    );

    runtime
        .try_send(parent, ParentMsg::Spawn)
        .expect("initial restartable child spawn should enqueue");
    assert_eq!(runtime.step(), 1);
    let mut current_child = latest_restart_child_address(runtime.trace())
        .expect("initial restartable child should have spawned");

    let mut ingress = Vec::new();
    let mut steps = Vec::new();

    for operation in operations {
        match *operation {
            Operation::EnqueueTargetData(value) => {
                ingress.push(outcome(runtime.try_send(target, TargetMsg::Data(value))));
            }
            Operation::EnqueueTargetStop => {
                ingress.push(outcome(runtime.try_send(target, TargetMsg::Stop)));
            }
            Operation::EnqueueDriverSend(value) => {
                ingress.push(outcome(runtime.try_send(driver, DriverMsg::Send(value))));
            }
            Operation::EnqueueParentRestart => {
                ingress.push(outcome(runtime.try_send(parent, ParentMsg::Restart)));
            }
            Operation::EnqueueChildPanic => {
                ingress.push(outcome(runtime.try_send(current_child, TargetMsg::Panic)));
            }
            Operation::Step => {
                steps.push(runtime.step());
                if let Some(replacement) = latest_restart_child_address(runtime.trace()) {
                    current_child = replacement;
                }
            }
        }
    }

    HistoryResult {
        ingress,
        steps,
        trace: runtime.trace().to_vec(),
    }
}

fn latest_restart_child_address(trace: &[RuntimeEvent]) -> Option<Address<TargetMsg>> {
    trace.iter().rev().find_map(|event| match event.kind() {
        RuntimeEventKind::RestartChildCompleted {
            new_isolate,
            new_generation,
            ..
        } => Some(Address::new_with_generation(
            event.shard(),
            new_isolate,
            new_generation,
        )),
        RuntimeEventKind::Spawned { child_isolate } => {
            Some(Address::new(event.shard(), child_isolate))
        }
        _ => None,
    })
}

fn outcome<T>(result: Result<(), TrySendError<T>>) -> IngressOutcome {
    match result {
        Ok(()) => IngressOutcome::Ok,
        Err(TrySendError::Full(_)) => IngressOutcome::Full,
        Err(TrySendError::Closed(_)) => IngressOutcome::Closed,
    }
}

fn assert_trace_is_causally_well_formed(trace: &[RuntimeEvent]) {
    let mut seen = BTreeSet::new();

    for (index, event) in trace.iter().copied().enumerate() {
        let expected_id = EventId::new((index + 1) as u64);
        assert_eq!(event.id(), expected_id);

        if let Some(cause) = event.cause() {
            assert!(
                cause.event() < event.id(),
                "cause {:?} must point before event {:?}",
                cause,
                event.id()
            );
            assert!(
                seen.contains(&cause.event()),
                "cause {:?} must point at an existing event",
                cause
            );
        }

        seen.insert(event.id());
    }
}

fn assert_send_attempts_have_visible_outcomes(trace: &[RuntimeEvent]) {
    for event in trace {
        let RuntimeEventKind::SendDispatchAttempted {
            target_shard,
            target_isolate,
            target_generation,
        } = event.kind()
        else {
            continue;
        };

        assert!(
            trace.iter().any(|candidate| {
                candidate.cause() == Some(CauseId::new(event.id()))
                    && matches!(
                        candidate.kind(),
                        RuntimeEventKind::SendAccepted {
                            target_shard: accepted_shard,
                            target_isolate: accepted_isolate,
                            target_generation: accepted_generation,
                        } if accepted_shard == target_shard
                            && accepted_isolate == target_isolate
                            && accepted_generation == target_generation
                    )
                    || candidate.cause() == Some(CauseId::new(event.id()))
                        && matches!(
                            candidate.kind(),
                            RuntimeEventKind::SendRejected {
                                target_shard: rejected_shard,
                                target_isolate: rejected_isolate,
                                target_generation: rejected_generation,
                                reason: SendRejectedReason::Full | SendRejectedReason::Closed,
                            } if rejected_shard == target_shard
                                && rejected_isolate == target_isolate
                                && rejected_generation == target_generation
                        )
            }),
            "send attempt {:?} must have a visible accepted/rejected outcome",
            event.id()
        );
    }
}

fn assert_restart_attempts_have_visible_outcomes(trace: &[RuntimeEvent]) {
    for event in trace {
        let RuntimeEventKind::RestartChildAttempted {
            child_ordinal,
            old_isolate,
            old_generation,
        } = event.kind()
        else {
            continue;
        };

        assert!(
            trace.iter().any(|candidate| {
                candidate.cause() == Some(CauseId::new(event.id()))
                    && matches!(
                        candidate.kind(),
                        RuntimeEventKind::RestartChildSkipped {
                            child_ordinal: skipped_ordinal,
                            old_isolate: skipped_isolate,
                            old_generation: skipped_generation,
                            ..
                        } if skipped_ordinal == child_ordinal
                            && skipped_isolate == old_isolate
                            && skipped_generation == old_generation
                    )
                    || candidate.cause() == Some(CauseId::new(event.id()))
                        && matches!(
                            candidate.kind(),
                            RuntimeEventKind::RestartChildCompleted {
                                child_ordinal: completed_ordinal,
                                old_isolate: completed_old_isolate,
                                old_generation: completed_old_generation,
                                ..
                            } if completed_ordinal == child_ordinal
                                && completed_old_isolate == old_isolate
                                && completed_old_generation == old_generation
                        )
            }),
            "restart attempt {:?} must have a visible skipped/completed outcome",
            event.id()
        );
    }
}

fn assert_supervisor_triggers_have_child_attempts(trace: &[RuntimeEvent]) {
    for event in trace {
        let RuntimeEventKind::SupervisorRestartTriggered { .. } = event.kind() else {
            continue;
        };

        assert!(
            trace.iter().any(|candidate| {
                candidate.cause() == Some(CauseId::new(event.id()))
                    && matches!(
                        candidate.kind(),
                        RuntimeEventKind::RestartChildAttempted { .. }
                    )
            }),
            "supervisor trigger {:?} must cause at least one child restart attempt",
            event.id()
        );
    }
}

fn assert_stopped_isolates_do_not_start_again(trace: &[RuntimeEvent]) {
    let mut stopped = BTreeSet::new();

    for event in trace {
        if matches!(event.kind(), RuntimeEventKind::HandlerStarted) {
            assert!(
                !stopped.contains(&event.isolate()),
                "stopped isolate {:?} must not handle another message",
                event.isolate()
            );
        }

        if matches!(event.kind(), RuntimeEventKind::IsolateStopped) {
            stopped.insert(event.isolate());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatcherTask {
    Normal,
    Poison,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatcherWorkerMsg {
    Run(DispatcherTask),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatcherMsg {
    SpawnWorker,
    Submit { slot: u8, task: DispatcherTask },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryMsg {
    Register {
        slot: u8,
        address: Address<DispatcherWorkerMsg>,
    },
    Forward {
        slot: u8,
        task: DispatcherTask,
    },
}

#[derive(Debug)]
struct DispatcherWorker {
    completed: Rc<RefCell<Vec<IsolateId>>>,
}

#[derive(Debug)]
struct DispatcherParent {
    registry: Address<RegistryMsg>,
    completed: Rc<RefCell<Vec<IsolateId>>>,
}

#[derive(Debug, Default)]
struct DispatcherRegistry {
    addresses: HashMap<u8, Address<DispatcherWorkerMsg>>,
}

impl Isolate for DispatcherWorker {
    type Message = DispatcherWorkerMsg;
    type Reply = Infallible;
    type Send = Outbound<DriverMsg>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        message: Self::Message,
        ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match message {
            DispatcherWorkerMsg::Run(DispatcherTask::Normal) => {
                self.completed.borrow_mut().push(ctx.isolate_id());
                Effect::Noop
            }
            DispatcherWorkerMsg::Run(DispatcherTask::Poison) => panic!("dispatcher worker panic"),
        }
    }
}

impl Isolate for DispatcherParent {
    type Message = DispatcherMsg;
    type Reply = Infallible;
    type Send = Outbound<RegistryMsg>;
    type Spawn = tina::RestartableChildDefinition<DispatcherWorker>;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        message: Self::Message,
        _ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match message {
            DispatcherMsg::SpawnWorker => {
                let completed = Rc::clone(&self.completed);
                Effect::Spawn(tina::RestartableChildDefinition::new(
                    move || DispatcherWorker {
                        completed: Rc::clone(&completed),
                    },
                    3,
                ))
            }
            DispatcherMsg::Submit { slot, task } => Effect::Send(Outbound::new(
                self.registry,
                RegistryMsg::Forward { slot, task },
            )),
        }
    }
}

impl Isolate for DispatcherRegistry {
    type Message = RegistryMsg;
    type Reply = Infallible;
    type Send = Outbound<DispatcherWorkerMsg>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        message: Self::Message,
        _ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match message {
            RegistryMsg::Register { slot, address } => {
                self.addresses.insert(slot, address);
                Effect::Noop
            }
            RegistryMsg::Forward { slot, task } => {
                let address =
                    self.addresses.get(&slot).copied().unwrap_or_else(|| {
                        panic!("dispatcher registry slot {slot} is not registered")
                    });
                Effect::Send(Outbound::new(address, DispatcherWorkerMsg::Run(task)))
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DispatcherOperation {
    Submit { slot: u8, task: DispatcherTask },
    RefreshSlot(u8),
    Step,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatcherHistoryResult {
    ingress: Vec<IngressOutcome>,
    refreshes: Vec<bool>,
    steps: Vec<usize>,
    completed: Vec<IsolateId>,
    trace: Vec<RuntimeEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatcherReplaySummary {
    completed: Vec<IsolateId>,
    panicked: BTreeSet<IsolateId>,
    stopped: BTreeSet<IsolateId>,
    replacements: Vec<(IsolateId, IsolateId)>,
}

fn dispatcher_operation_strategy() -> impl Strategy<Value = DispatcherOperation> {
    prop_oneof![
        (0_u8..2).prop_map(|slot| DispatcherOperation::Submit {
            slot,
            task: DispatcherTask::Normal,
        }),
        (0_u8..2).prop_map(|slot| DispatcherOperation::Submit {
            slot,
            task: DispatcherTask::Poison,
        }),
        (0_u8..2).prop_map(DispatcherOperation::RefreshSlot),
        Just(DispatcherOperation::Step),
    ]
}

fn run_dispatcher_history(operations: &[DispatcherOperation]) -> DispatcherHistoryResult {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let completed = Rc::new(RefCell::new(Vec::new()));

    let registry = runtime.register(DispatcherRegistry::default(), TestMailbox::new(8));
    let dispatcher = runtime.register(
        DispatcherParent {
            registry,
            completed: Rc::clone(&completed),
        },
        TestMailbox::new(8),
    );
    runtime.supervise(
        dispatcher,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(64)),
    );

    let mut slot_addresses = Vec::new();
    for slot in 0_u8..2 {
        let trace_len_before = runtime.trace().len();
        runtime
            .try_send(dispatcher, DispatcherMsg::SpawnWorker)
            .expect("initial worker spawn should enqueue");
        assert_eq!(runtime.step(), 1);
        let child = latest_dispatcher_worker_address(&runtime.trace()[trace_len_before..])
            .expect("spawned worker address");
        runtime
            .try_send(
                registry,
                RegistryMsg::Register {
                    slot,
                    address: child,
                },
            )
            .expect("initial registry seed should enqueue");
        assert_eq!(runtime.step(), 1);
        slot_addresses.push(child);
    }

    let mut ingress = Vec::new();
    let mut refreshes = Vec::new();
    let mut steps = Vec::new();

    for operation in operations {
        match *operation {
            DispatcherOperation::Submit { slot, task } => {
                ingress.push(outcome(
                    runtime.try_send(dispatcher, DispatcherMsg::Submit { slot, task }),
                ));
            }
            DispatcherOperation::RefreshSlot(slot) => {
                let refreshed = latest_dispatcher_replacement_for(
                    slot_addresses[slot as usize].isolate(),
                    runtime.trace(),
                )
                .filter(|address| *address != slot_addresses[slot as usize]);

                if let Some(address) = refreshed {
                    runtime
                        .try_send(registry, RegistryMsg::Register { slot, address })
                        .expect("registry refresh should enqueue");
                    assert!(
                        runtime.step() >= 1,
                        "registry refresh should at least run the registry handler"
                    );
                    slot_addresses[slot as usize] = address;
                    refreshes.push(true);
                } else {
                    refreshes.push(false);
                }
            }
            DispatcherOperation::Step => {
                steps.push(runtime.step());
            }
        }
    }

    DispatcherHistoryResult {
        ingress,
        refreshes,
        steps,
        completed: completed.borrow().clone(),
        trace: runtime.trace().to_vec(),
    }
}

fn latest_dispatcher_worker_address(
    trace: &[RuntimeEvent],
) -> Option<Address<DispatcherWorkerMsg>> {
    trace.iter().rev().find_map(|event| match event.kind() {
        RuntimeEventKind::RestartChildCompleted {
            new_isolate,
            new_generation,
            ..
        } => Some(Address::new_with_generation(
            event.shard(),
            new_isolate,
            new_generation,
        )),
        RuntimeEventKind::Spawned { child_isolate } => {
            Some(Address::new(event.shard(), child_isolate))
        }
        _ => None,
    })
}

fn latest_dispatcher_replacement_for(
    failed_isolate: IsolateId,
    trace: &[RuntimeEvent],
) -> Option<Address<DispatcherWorkerMsg>> {
    trace.iter().rev().find_map(|event| match event.kind() {
        RuntimeEventKind::RestartChildCompleted {
            old_isolate,
            new_isolate,
            new_generation,
            ..
        } if old_isolate == failed_isolate => Some(Address::new_with_generation(
            event.shard(),
            new_isolate,
            new_generation,
        )),
        _ => None,
    })
}

fn replay_dispatcher_trace(trace: &[RuntimeEvent]) -> DispatcherReplaySummary {
    let mut known_workers = BTreeSet::new();
    let mut completed = Vec::new();
    let mut panicked = BTreeSet::new();
    let mut stopped = BTreeSet::new();
    let mut replacements = Vec::new();

    for event in trace {
        match event.kind() {
            RuntimeEventKind::Spawned { child_isolate } => {
                known_workers.insert(child_isolate);
            }
            RuntimeEventKind::RestartChildCompleted {
                old_isolate,
                new_isolate,
                ..
            } => {
                known_workers.insert(old_isolate);
                known_workers.insert(new_isolate);
                replacements.push((old_isolate, new_isolate));
            }
            RuntimeEventKind::HandlerPanicked if known_workers.contains(&event.isolate()) => {
                panicked.insert(event.isolate());
            }
            RuntimeEventKind::HandlerFinished {
                effect: EffectKind::Noop,
            } if known_workers.contains(&event.isolate()) => {
                completed.push(event.isolate());
            }
            RuntimeEventKind::IsolateStopped if known_workers.contains(&event.isolate()) => {
                stopped.insert(event.isolate());
            }
            _ => {}
        }
    }

    DispatcherReplaySummary {
        completed,
        panicked,
        stopped,
        replacements,
    }
}

fn assert_dispatcher_replay_matches_live(result: &DispatcherHistoryResult) {
    let replay = replay_dispatcher_trace(&result.trace);

    assert_eq!(
        replay.completed, result.completed,
        "trace replay should recover the same worker completion sequence as live state"
    );
    assert!(
        replay
            .panicked
            .iter()
            .all(|isolate| replay.stopped.contains(isolate)),
        "every replayed worker panic must lead to a visible worker stop"
    );
    assert_eq!(
        replay.replacements.len(),
        replay.panicked.len(),
        "one-for-one dispatcher workload should replace each panicked worker exactly once"
    );
}

proptest! {
    #[test]
    fn generated_histories_are_deterministic(operations in prop::collection::vec(operation_strategy(), 0..40)) {
        prop_assert_eq!(run_history(&operations), run_history(&operations));
    }

    #[test]
    fn generated_histories_keep_causal_links_well_formed(operations in prop::collection::vec(operation_strategy(), 0..40)) {
        let result = run_history(&operations);
        assert_trace_is_causally_well_formed(&result.trace);
    }

    #[test]
    fn generated_histories_give_every_send_attempt_a_visible_outcome(operations in prop::collection::vec(operation_strategy(), 0..40)) {
        let result = run_history(&operations);
        assert_send_attempts_have_visible_outcomes(&result.trace);
    }

    #[test]
    fn generated_histories_give_every_restart_attempt_a_visible_outcome(operations in prop::collection::vec(operation_strategy(), 0..40)) {
        let result = run_history(&operations);
        assert_restart_attempts_have_visible_outcomes(&result.trace);
    }

    #[test]
    fn generated_histories_give_every_supervisor_trigger_child_attempts(operations in prop::collection::vec(operation_strategy(), 0..40)) {
        let result = run_history(&operations);
        assert_supervisor_triggers_have_child_attempts(&result.trace);
    }

    #[test]
    fn generated_histories_do_not_restart_stopped_isolates_by_accident(operations in prop::collection::vec(operation_strategy(), 0..40)) {
        let result = run_history(&operations);
        assert_stopped_isolates_do_not_start_again(&result.trace);
    }

    #[test]
    fn generated_dispatcher_histories_are_deterministic(
        operations in prop::collection::vec(dispatcher_operation_strategy(), 0..40)
    ) {
        prop_assert_eq!(run_dispatcher_history(&operations), run_dispatcher_history(&operations));
    }

    #[test]
    fn generated_dispatcher_histories_can_be_replayed_from_trace(
        operations in prop::collection::vec(dispatcher_operation_strategy(), 0..40)
    ) {
        let result = run_dispatcher_history(&operations);
        assert_trace_is_causally_well_formed(&result.trace);
        assert_send_attempts_have_visible_outcomes(&result.trace);
        assert_dispatcher_replay_matches_live(&result);
    }
}
