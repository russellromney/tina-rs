use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::convert::Infallible;
use std::rc::Rc;

use proptest::prelude::*;
use tina::{Address, Context, Effect, Isolate, Mailbox, SendMessage, Shard, ShardId, TrySendError};
use tina_runtime_current::{
    CauseId, CurrentRuntime, EventId, MailboxFactory, RuntimeEvent, RuntimeEventKind,
    SendRejectedReason,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetMsg {
    Data(u8),
    Stop,
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
    type Send = SendMessage<DriverMsg>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        message: Self::Message,
        _ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match message {
            TargetMsg::Data(_) => Effect::Noop,
            TargetMsg::Stop => Effect::Stop,
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
    type Send = SendMessage<TargetMsg>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        message: Self::Message,
        _ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match message {
            DriverMsg::Send(value) => {
                Effect::Send(SendMessage::new(self.target, TargetMsg::Data(value)))
            }
        }
    }
}

impl Isolate for RestartParent {
    type Message = ParentMsg;
    type Reply = Infallible;
    type Send = SendMessage<TargetMsg>;
    type Spawn = tina::RestartableSpawnSpec<RestartChild>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        message: Self::Message,
        _ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match message {
            ParentMsg::Spawn => Effect::Spawn(tina::RestartableSpawnSpec::new(|| RestartChild, 3)),
            ParentMsg::Restart => Effect::RestartChildren,
        }
    }
}

impl Isolate for RestartChild {
    type Message = TargetMsg;
    type Reply = Infallible;
    type Send = SendMessage<TargetMsg>;
    type Spawn = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        message: Self::Message,
        _ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match message {
            TargetMsg::Data(_) => Effect::Noop,
            TargetMsg::Stop => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    EnqueueTargetData(u8),
    EnqueueTargetStop,
    EnqueueDriverSend(u8),
    EnqueueParentRestart,
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
        Just(Operation::Step),
    ]
}

fn run_history(operations: &[Operation]) -> HistoryResult {
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
    let target = runtime.register(Target, TestMailbox::new(3));
    let driver = runtime.register(Driver { target }, TestMailbox::new(3));
    let parent = runtime.register(RestartParent, TestMailbox::new(3));

    runtime
        .try_send(parent, ParentMsg::Spawn)
        .expect("initial restartable child spawn should enqueue");
    assert_eq!(runtime.step(), 1);

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
            Operation::Step => {
                steps.push(runtime.step());
            }
        }
    }

    HistoryResult {
        ingress,
        steps,
        trace: runtime.trace().to_vec(),
    }
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
    fn generated_histories_do_not_restart_stopped_isolates_by_accident(operations in prop::collection::vec(operation_strategy(), 0..40)) {
        let result = run_history(&operations);
        assert_stopped_isolates_do_not_start_again(&result.trace);
    }
}
