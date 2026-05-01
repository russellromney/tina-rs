use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;
use std::thread;
use std::time::Duration;

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallError, CallInput, CallKind, CallOutput, MailboxFactory, Runtime, RuntimeCall, RuntimeEvent,
    RuntimeEventKind, sleep,
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
