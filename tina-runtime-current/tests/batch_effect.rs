use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::rc::Rc;

use tina::{
    Address, ChildDefinition, Isolate, Mailbox, Outbound, Shard, ShardId, TrySendError, prelude::*,
};
use tina_runtime::{
    CallInput, CallKind, CallOutput, EffectKind, MailboxFactory, Runtime, RuntimeCall,
    RuntimeEventKind, StreamId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeverOutbound {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditEvent {
    Record(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverEvent {
    SendTwice,
    SpawnAndSend,
    StopThenSend,
    SendThenBind,
    BindObserved,
    FailReadThenSend,
    ReadFailureObserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerEvent {
    Begin,
}

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(17)
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
struct Audit {
    seen: Rc<RefCell<Vec<u8>>>,
}

impl Isolate for Audit {
    tina::isolate_types! {
        message: AuditEvent,
        reply: (),
        send: Outbound<NeverOutbound>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        let AuditEvent::Record(value) = msg;
        self.seen.borrow_mut().push(value);
        noop()
    }
}

#[derive(Debug)]
struct Worker;

impl Isolate for Worker {
    tina::isolate_types! {
        message: WorkerEvent,
        reply: (),
        send: Outbound<NeverOutbound>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        noop()
    }
}

#[derive(Debug)]
struct Driver {
    audit: Address<AuditEvent>,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverEvent,
        reply: (),
        send: Outbound<AuditEvent>,
        spawn: ChildDefinition<Worker>,
        call: RuntimeCall<DriverEvent>,
        shard: TestShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            DriverEvent::SendTwice => batch([
                send(self.audit, AuditEvent::Record(1)),
                send(self.audit, AuditEvent::Record(2)),
            ]),
            DriverEvent::SpawnAndSend => batch([
                spawn(ChildDefinition::new(Worker, 4).with_initial_message(WorkerEvent::Begin)),
                send(self.audit, AuditEvent::Record(9)),
            ]),
            DriverEvent::StopThenSend => batch([stop(), send(self.audit, AuditEvent::Record(7))]),
            DriverEvent::SendThenBind => batch([
                send(self.audit, AuditEvent::Record(3)),
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpBind {
                        addr: "127.0.0.1:0".parse::<SocketAddr>().expect("loopback parse"),
                    },
                    |result| match result {
                        CallOutput::TcpBound { .. } => DriverEvent::BindObserved,
                        other => panic!("expected successful bind result, got {other:?}"),
                    },
                )),
            ]),
            DriverEvent::BindObserved => send(self.audit, AuditEvent::Record(4)),
            DriverEvent::FailReadThenSend => batch([
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpRead {
                        stream: StreamId::new(9999),
                        max_len: 8,
                    },
                    |result| match result {
                        CallOutput::Failed(_) => DriverEvent::ReadFailureObserved,
                        other => panic!("expected invalid read failure, got {other:?}"),
                    },
                )),
                send(self.audit, AuditEvent::Record(5)),
            ]),
            DriverEvent::ReadFailureObserved => send(self.audit, AuditEvent::Record(6)),
        }
    }
}

struct Harness {
    runtime: Runtime<TestShard, TestMailboxFactory>,
    driver: Address<DriverEvent>,
    seen: Rc<RefCell<Vec<u8>>>,
}

impl Harness {
    fn new() -> Self {
        let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let driver = {
            let audit = runtime.register(
                Audit {
                    seen: Rc::clone(&seen),
                },
                TestMailbox::new(8),
            );
            runtime.register(Driver { audit }, TestMailbox::new(8))
        };

        Self {
            runtime,
            driver,
            seen,
        }
    }
}

fn count_events(
    trace: &[tina_runtime::RuntimeEvent],
    predicate: impl Fn(&RuntimeEventKind) -> bool,
) -> usize {
    trace
        .iter()
        .filter(|event| predicate(&event.kind()))
        .count()
}

fn first_event_index(
    trace: &[tina_runtime::RuntimeEvent],
    predicate: impl Fn(&RuntimeEventKind) -> bool,
) -> usize {
    trace
        .iter()
        .position(|event| predicate(&event.kind()))
        .expect("expected matching trace event")
}

#[test]
fn batch_send_effects_execute_left_to_right_and_deliver_later() {
    let mut harness = Harness::new();
    harness
        .runtime
        .try_send(harness.driver, DriverEvent::SendTwice)
        .expect("ingress accepts SendTwice");

    harness.runtime.step();
    assert_eq!(*harness.seen.borrow(), Vec::<u8>::new());

    let trace = harness.runtime.trace();
    assert_eq!(
        count_events(trace, |kind| matches!(
            kind,
            RuntimeEventKind::HandlerFinished {
                effect: EffectKind::Batch
            }
        )),
        1
    );
    assert_eq!(
        count_events(trace, |kind| matches!(
            kind,
            RuntimeEventKind::SendDispatchAttempted { .. }
        )),
        2
    );
    assert_eq!(
        count_events(trace, |kind| matches!(
            kind,
            RuntimeEventKind::SendAccepted { .. }
        )),
        2
    );

    harness.runtime.step();
    harness.runtime.step();
    assert_eq!(*harness.seen.borrow(), vec![1, 2]);
}

#[test]
fn batch_can_spawn_then_send_in_one_handler_turn() {
    let mut harness = Harness::new();
    harness
        .runtime
        .try_send(harness.driver, DriverEvent::SpawnAndSend)
        .expect("ingress accepts SpawnAndSend");

    harness.runtime.step();

    let trace = harness.runtime.trace();
    assert_eq!(
        count_events(trace, |kind| matches!(
            kind,
            RuntimeEventKind::HandlerFinished {
                effect: EffectKind::Batch
            }
        )),
        1
    );
    assert_eq!(
        count_events(trace, |kind| matches!(
            kind,
            RuntimeEventKind::Spawned { .. }
        )),
        1
    );
    assert_eq!(
        count_events(trace, |kind| matches!(
            kind,
            RuntimeEventKind::SendAccepted { .. }
        )),
        1
    );

    harness.runtime.step();
    assert_eq!(*harness.seen.borrow(), vec![9]);
}

#[test]
fn stop_short_circuits_later_effects_in_the_same_batch() {
    let mut harness = Harness::new();
    harness
        .runtime
        .try_send(harness.driver, DriverEvent::StopThenSend)
        .expect("ingress accepts StopThenSend");

    harness.runtime.step();
    harness.runtime.step();

    assert!(harness.seen.borrow().is_empty());

    let trace = harness.runtime.trace();
    assert_eq!(
        count_events(trace, |kind| matches!(
            kind,
            RuntimeEventKind::HandlerFinished {
                effect: EffectKind::Batch
            }
        )),
        1
    );
    assert_eq!(
        count_events(trace, |kind| matches!(
            kind,
            RuntimeEventKind::IsolateStopped
        )),
        1
    );
    assert_eq!(
        count_events(trace, |kind| matches!(
            kind,
            RuntimeEventKind::SendDispatchAttempted { .. }
        )),
        0
    );
}

#[test]
fn batch_send_then_synchronous_call_keeps_left_to_right_order() {
    let mut harness = Harness::new();
    harness
        .runtime
        .try_send(harness.driver, DriverEvent::SendThenBind)
        .expect("ingress accepts SendThenBind");

    harness.runtime.step();

    let trace = harness.runtime.trace();
    let send_attempt = first_event_index(trace, |kind| {
        matches!(kind, RuntimeEventKind::SendDispatchAttempted { .. })
    });
    let send_accepted = first_event_index(trace, |kind| {
        matches!(kind, RuntimeEventKind::SendAccepted { .. })
    });
    let call_attempt = first_event_index(trace, |kind| {
        matches!(
            kind,
            RuntimeEventKind::CallDispatchAttempted {
                call_kind: CallKind::TcpBind,
                ..
            }
        )
    });
    let call_completed = first_event_index(trace, |kind| {
        matches!(
            kind,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpBind,
                ..
            }
        )
    });

    assert!(send_attempt < send_accepted);
    assert!(send_accepted < call_attempt);
    assert!(call_attempt < call_completed);

    harness.runtime.step();
    assert_eq!(*harness.seen.borrow(), vec![3]);

    harness.runtime.step();
    assert_eq!(*harness.seen.borrow(), vec![3, 4]);
}

#[test]
fn batch_failing_call_still_runs_later_effects() {
    let mut harness = Harness::new();
    harness
        .runtime
        .try_send(harness.driver, DriverEvent::FailReadThenSend)
        .expect("ingress accepts FailReadThenSend");

    harness.runtime.step();

    let trace = harness.runtime.trace();
    let call_attempt = first_event_index(trace, |kind| {
        matches!(
            kind,
            RuntimeEventKind::CallDispatchAttempted {
                call_kind: CallKind::TcpRead,
                ..
            }
        )
    });
    let call_failed = first_event_index(trace, |kind| {
        matches!(
            kind,
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpRead,
                ..
            }
        )
    });
    let send_attempt = first_event_index(trace, |kind| {
        matches!(kind, RuntimeEventKind::SendDispatchAttempted { .. })
    });
    let send_accepted = first_event_index(trace, |kind| {
        matches!(kind, RuntimeEventKind::SendAccepted { .. })
    });

    assert!(call_attempt < call_failed);
    assert!(call_failed < send_attempt);
    assert!(send_attempt < send_accepted);

    harness.runtime.step();
    assert_eq!(*harness.seen.borrow(), vec![5]);

    harness.runtime.step();
    assert_eq!(*harness.seen.borrow(), vec![5, 6]);
}
