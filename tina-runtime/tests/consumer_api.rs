use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;
use std::thread;
use std::time::Duration;

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallInput, CallKind, CallOutcome, CallOutput,
    EffectKind, MailboxFactory, Runtime, RuntimeCall, RuntimeEvent, RuntimeEventKind, SendOutcome,
    call, send_observed, sleep,
};

#[derive(Debug, Default)]
struct ConsumerShard;

impl Shard for ConsumerShard {
    fn id(&self) -> ShardId {
        ShardId::new(88)
    }
}

struct ConsumerMailbox<T> {
    capacity: usize,
    queue: RefCell<VecDeque<T>>,
    closed: Cell<bool>,
}

impl<T> ConsumerMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        }
    }
}

impl<T> Mailbox<T> for ConsumerMailbox<T> {
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
struct ConsumerMailboxFactory;

impl MailboxFactory for ConsumerMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(ConsumerMailbox::new(capacity))
    }
}

fn drive(runtime: &mut Runtime<ConsumerShard, ConsumerMailboxFactory>) {
    for _ in 0..128 {
        let ran = runtime.step();
        if ran == 0 && !runtime.has_in_flight_calls() {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("runtime did not quiesce within 128 steps");
}

fn count_call_completed(trace: &[RuntimeEvent], kind: CallKind) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == kind
            )
        })
        .count()
}

fn count_call_failed(trace: &[RuntimeEvent], kind: CallKind, reason: CallError) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallFailed { call_kind, reason: found, .. }
                    if call_kind == kind && found == reason
            )
        })
        .count()
}

fn count_call_completion_rejected(
    trace: &[RuntimeEvent],
    kind: CallKind,
    reason: CallCompletionRejectedReason,
) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompletionRejected { call_kind, reason: found, .. }
                    if call_kind == kind && found == reason
            )
        })
        .count()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TimerEvent {
    Begin,
    DelayFinished(Result<(), CallError>),
}

#[derive(Debug)]
struct TimerWorker {
    observations: Rc<RefCell<Vec<&'static str>>>,
}

impl Isolate for TimerWorker {
    tina::isolate_types! {
        message: TimerEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<TimerEvent>,
        shard: ConsumerShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TimerEvent::Begin => sleep(Duration::from_millis(5)).reply(TimerEvent::DelayFinished),
            TimerEvent::DelayFinished(Ok(())) => {
                self.observations.borrow_mut().push("slept");
                stop()
            }
            TimerEvent::DelayFinished(Err(_)) => {
                self.observations.borrow_mut().push("failed");
                stop()
            }
        }
    }
}

#[test]
fn downstream_consumer_can_use_runtime_timer_helper_end_to_end() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut runtime = Runtime::new(ConsumerShard, ConsumerMailboxFactory);
    let worker = runtime.register_with_capacity(
        TimerWorker {
            observations: Rc::clone(&observations),
        },
        8,
    );

    runtime.try_send(worker, TimerEvent::Begin).unwrap();
    drive(&mut runtime);

    assert_eq!(observations.borrow().as_slice(), ["slept"]);
    assert_eq!(count_call_completed(runtime.trace(), CallKind::Sleep), 1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservedTargetEvent {
    Work,
    Stop,
}

#[derive(Debug)]
struct ObservedTarget;

impl Isolate for ObservedTarget {
    tina::isolate_types! {
        message: ObservedTargetEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: ConsumerShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ObservedTargetEvent::Work => noop(),
            ObservedTargetEvent::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ObservedSenderEvent {
    Start(Address<ObservedTargetEvent>),
    SendFinished(SendOutcome),
}

#[derive(Debug)]
struct ObservedSender {
    outcomes: Rc<RefCell<Vec<SendOutcome>>>,
}

impl Isolate for ObservedSender {
    tina::isolate_types! {
        message: ObservedSenderEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<ObservedSenderEvent>,
        shard: ConsumerShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ObservedSenderEvent::Start(target) => send_observed(target, ObservedTargetEvent::Work)
                .reply(ObservedSenderEvent::SendFinished),
            ObservedSenderEvent::SendFinished(outcome) => {
                self.outcomes.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn downstream_consumer_can_observe_send_accepted_full_and_closed() {
    for (target_capacity, stop_first, expected) in [
        (1, false, SendOutcome::Accepted),
        (0, false, SendOutcome::Full),
        (1, true, SendOutcome::Closed),
    ] {
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let mut runtime = Runtime::new(ConsumerShard, ConsumerMailboxFactory);
        let target = runtime.register_with_capacity(ObservedTarget, target_capacity);
        let sender = runtime.register_with_capacity(
            ObservedSender {
                outcomes: Rc::clone(&outcomes),
            },
            8,
        );

        if stop_first {
            runtime
                .try_send(target, ObservedTargetEvent::Stop)
                .expect("stop target");
            drive(&mut runtime);
        }

        runtime
            .try_send(sender, ObservedSenderEvent::Start(target))
            .expect("start observed send");
        drive(&mut runtime);

        assert_eq!(outcomes.borrow().as_slice(), [expected]);
        assert_eq!(
            count_call_completed(runtime.trace(), CallKind::ObservedSend),
            1
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerReply(&'static str);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerRequest {
    ReplyNow,
    DoNotReply,
    Stop,
}

#[derive(Debug)]
struct ReplyWorker;

impl Isolate for ReplyWorker {
    tina::isolate_types! {
        message: WorkerRequest,
        reply: WorkerReply,
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: ConsumerShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WorkerRequest::ReplyNow => reply(WorkerReply("pong")),
            WorkerRequest::DoNotReply => noop(),
            WorkerRequest::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CallerEvent {
    Start(Address<WorkerRequest>, WorkerRequest, Duration),
    StartAndStop(Address<WorkerRequest>),
    Filler,
    Returned(CallOutcome<WorkerReply>),
}

#[derive(Debug)]
struct CallerWorker {
    outcomes: Rc<RefCell<Vec<CallOutcome<WorkerReply>>>>,
}

impl Isolate for CallerWorker {
    tina::isolate_types! {
        message: CallerEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<CallerEvent>,
        shard: ConsumerShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CallerEvent::Start(target, request, timeout) => {
                call::<WorkerRequest, WorkerReply>(target, request, timeout)
                    .reply(CallerEvent::Returned)
            }
            CallerEvent::StartAndStop(target) => batch(vec![
                call::<WorkerRequest, WorkerReply>(
                    target,
                    WorkerRequest::ReplyNow,
                    Duration::from_millis(10),
                )
                .reply(CallerEvent::Returned),
                stop(),
            ]),
            CallerEvent::Filler => noop(),
            CallerEvent::Returned(outcome) => {
                self.outcomes.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FillerEvent {
    Fill(Address<CallerEvent>),
}

#[derive(Debug)]
struct FillerWorker;

impl Isolate for FillerWorker {
    tina::isolate_types! {
        message: FillerEvent,
        reply: (),
        send: Outbound<CallerEvent>,
        spawn: Infallible,
        call: Infallible,
        shard: ConsumerShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            FillerEvent::Fill(caller) => send(caller, CallerEvent::Filler),
        }
    }
}

#[test]
fn downstream_consumer_can_call_isolate_and_observe_reply_full_closed_timeout() {
    for (target_capacity, setup_stop, request, expected, failed) in [
        (
            1,
            false,
            WorkerRequest::ReplyNow,
            CallOutcome::Replied(WorkerReply("pong")),
            None,
        ),
        (
            0,
            false,
            WorkerRequest::ReplyNow,
            CallOutcome::Full,
            Some(CallError::TargetFull),
        ),
        (
            1,
            true,
            WorkerRequest::ReplyNow,
            CallOutcome::Closed,
            Some(CallError::TargetClosed),
        ),
        (
            1,
            false,
            WorkerRequest::DoNotReply,
            CallOutcome::Timeout,
            Some(CallError::Timeout),
        ),
    ] {
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let mut runtime = Runtime::new(ConsumerShard, ConsumerMailboxFactory);
        let target = runtime.register_with_capacity(ReplyWorker, target_capacity);
        let caller = runtime.register_with_capacity(
            CallerWorker {
                outcomes: Rc::clone(&outcomes),
            },
            8,
        );

        if setup_stop {
            runtime
                .try_send(target, WorkerRequest::Stop)
                .expect("stop target");
            drive(&mut runtime);
        }

        runtime
            .try_send(
                caller,
                CallerEvent::Start(target, request, Duration::from_millis(2)),
            )
            .expect("start isolate call");
        drive(&mut runtime);

        assert_eq!(outcomes.borrow().as_slice(), [expected]);
        if failed.is_none() {
            assert_eq!(
                count_call_completed(runtime.trace(), CallKind::IsolateCall),
                1
            );
        }
        if let Some(reason) = failed {
            assert_eq!(
                count_call_failed(runtime.trace(), CallKind::IsolateCall, reason),
                1
            );
        }
    }
}

#[test]
fn downstream_consumer_sees_late_isolate_call_reply_as_plain_reply_after_timeout() {
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let mut runtime = Runtime::new(ConsumerShard, ConsumerMailboxFactory);
    let target = runtime.register_with_capacity(ReplyWorker, 8);
    let caller = runtime.register_with_capacity(
        CallerWorker {
            outcomes: Rc::clone(&outcomes),
        },
        8,
    );

    runtime
        .try_send(
            caller,
            CallerEvent::Start(target, WorkerRequest::ReplyNow, Duration::ZERO),
        )
        .expect("start zero-timeout isolate call");
    drive(&mut runtime);

    assert_eq!(outcomes.borrow().as_slice(), [CallOutcome::Timeout]);
    assert_eq!(
        count_call_failed(runtime.trace(), CallKind::IsolateCall, CallError::Timeout),
        1
    );
    assert_eq!(
        count_call_completed(runtime.trace(), CallKind::IsolateCall),
        0
    );
    assert!(
        runtime.trace().iter().any(|event| matches!(
            event.kind(),
            RuntimeEventKind::EffectObserved {
                effect: EffectKind::Reply
            }
        )),
        "late reply after timeout should fall back to ordinary reply observation"
    );
}

#[test]
fn downstream_consumer_sees_isolate_call_completion_rejected_when_requester_mailbox_full() {
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let mut runtime = Runtime::new(ConsumerShard, ConsumerMailboxFactory);
    let filler = runtime.register_with_capacity(FillerWorker, 8);
    let target = runtime.register_with_capacity(ReplyWorker, 8);
    let caller = runtime.register_with_capacity(
        CallerWorker {
            outcomes: Rc::clone(&outcomes),
        },
        1,
    );

    runtime
        .try_send(
            caller,
            CallerEvent::Start(target, WorkerRequest::ReplyNow, Duration::from_millis(10)),
        )
        .expect("start isolate call");
    assert_eq!(runtime.step(), 1);
    runtime
        .try_send(filler, FillerEvent::Fill(caller))
        .expect("ask filler to fill requester mailbox before reply");
    drive(&mut runtime);

    assert!(outcomes.borrow().is_empty());
    assert_eq!(
        count_call_completion_rejected(
            runtime.trace(),
            CallKind::IsolateCall,
            CallCompletionRejectedReason::MailboxFull,
        ),
        1
    );
}

#[test]
fn downstream_consumer_sees_isolate_call_completion_rejected_when_requester_stops() {
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let mut runtime = Runtime::new(ConsumerShard, ConsumerMailboxFactory);
    let target = runtime.register_with_capacity(ReplyWorker, 8);
    let caller = runtime.register_with_capacity(
        CallerWorker {
            outcomes: Rc::clone(&outcomes),
        },
        8,
    );

    runtime
        .try_send(caller, CallerEvent::StartAndStop(target))
        .expect("start call then stop");
    drive(&mut runtime);

    assert!(outcomes.borrow().is_empty());
    assert_eq!(
        count_call_completion_rejected(
            runtime.trace(),
            CallKind::IsolateCall,
            CallCompletionRejectedReason::RequesterClosed,
        ),
        1
    );
}

#[derive(Debug, Clone)]
enum LowLevelEvent {
    Start,
    Completed(Result<CallOutput, CallError>),
}

#[derive(Debug)]
struct LowLevelWorker {
    observations: Rc<RefCell<Vec<&'static str>>>,
}

impl Isolate for LowLevelWorker {
    tina::isolate_types! {
        message: LowLevelEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<LowLevelEvent>,
        shard: ConsumerShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            LowLevelEvent::Start => Effect::Call(RuntimeCall::map_result(
                CallInput::Sleep {
                    after: Duration::from_millis(3),
                },
                LowLevelEvent::Completed,
            )),
            LowLevelEvent::Completed(Ok(CallOutput::TimerFired)) => {
                self.observations.borrow_mut().push("timer-fired");
                stop()
            }
            LowLevelEvent::Completed(Ok(other)) => panic!("unexpected low-level output {other:?}"),
            LowLevelEvent::Completed(Err(_)) => {
                self.observations.borrow_mut().push("failed");
                stop()
            }
        }
    }
}

#[test]
fn downstream_consumer_can_use_low_level_call_renames_end_to_end() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut runtime = Runtime::new(ConsumerShard, ConsumerMailboxFactory);
    let worker = runtime.register_with_capacity(
        LowLevelWorker {
            observations: Rc::clone(&observations),
        },
        8,
    );

    runtime.try_send(worker, LowLevelEvent::Start).unwrap();
    drive(&mut runtime);

    assert_eq!(observations.borrow().as_slice(), ["timer-fired"]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildEvent {
    Begin,
}

#[derive(Debug)]
struct ChildWorker {
    starts: Rc<Cell<u32>>,
}

impl Isolate for ChildWorker {
    tina::isolate_types! {
        message: ChildEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: ConsumerShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ChildEvent::Begin => {
                self.starts.set(self.starts.get() + 1);
                stop()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentEvent {
    Begin,
}

#[derive(Debug)]
struct ParentWorker {
    starts: Rc<Cell<u32>>,
}

impl Isolate for ParentWorker {
    tina::isolate_types! {
        message: ParentEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: RestartableChildDefinition<ChildWorker>,
        call: Infallible,
        shard: ConsumerShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ParentEvent::Begin => spawn(
                RestartableChildDefinition::new(
                    {
                        let starts = Rc::clone(&self.starts);
                        move || ChildWorker {
                            starts: Rc::clone(&starts),
                        }
                    },
                    4,
                )
                .with_initial_message(|| ChildEvent::Begin),
            ),
        }
    }
}

#[test]
fn downstream_consumer_can_spawn_restartable_child_with_initial_message() {
    let starts = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(ConsumerShard, ConsumerMailboxFactory);
    let parent = runtime.register_with_capacity(
        ParentWorker {
            starts: Rc::clone(&starts),
        },
        8,
    );

    runtime.try_send(parent, ParentEvent::Begin).unwrap();
    drive(&mut runtime);

    assert_eq!(starts.get(), 1);
    assert!(
        runtime
            .trace()
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::Spawned { .. })),
        "runtime trace should show the child spawn"
    );
}
