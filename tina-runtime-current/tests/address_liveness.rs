use std::cell::RefCell;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use tina::{
    Address, AddressGeneration, Context, Effect, Isolate, Mailbox, SendMessage, Shard, ShardId,
    TrySendError,
};
use tina_runtime_current::{CurrentRuntime, MailboxFactory, RuntimeEventKind, SendRejectedReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetMsg {
    Stop,
    Data(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverMsg {
    Kick(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(3)
    }
}

#[derive(Debug, Clone)]
struct TestMailbox<T> {
    inner: Rc<RefCell<TestMailboxInner<T>>>,
}

#[derive(Debug)]
struct TestMailboxInner<T> {
    closed: bool,
    messages: VecDeque<T>,
}

impl<T> Default for TestMailbox<T> {
    fn default() -> Self {
        Self {
            inner: Rc::new(RefCell::new(TestMailboxInner {
                closed: false,
                messages: VecDeque::new(),
            })),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        usize::MAX
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        let mut inner = self.inner.borrow_mut();
        if inner.closed {
            return Err(TrySendError::Closed(message));
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
    fn create<T: 'static>(&self, _capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::<T>::default())
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
            TargetMsg::Stop => Effect::Stop,
            TargetMsg::Data(_) => Effect::Noop,
        }
    }
}

#[derive(Debug)]
struct Driver {
    target: Address<TargetMsg>,
}

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
            DriverMsg::Kick(value) => {
                Effect::Send(SendMessage::new(self.target, TargetMsg::Data(value)))
            }
        }
    }
}

fn runtime() -> CurrentRuntime<TestShard, TestMailboxFactory> {
    CurrentRuntime::new(TestShard, TestMailboxFactory)
}

#[test]
fn runtime_issued_addresses_expose_initial_generation() {
    let mut runtime = runtime();
    let address = runtime.register(Target, TestMailbox::default());

    assert_eq!(address.shard(), ShardId::new(3));
    assert_eq!(address.generation(), AddressGeneration::new(0));
    assert_eq!(
        Address::<TargetMsg>::new(address.shard(), address.isolate()).generation(),
        AddressGeneration::new(0)
    );
}

#[test]
fn runtime_ingress_to_stopped_address_returns_closed() {
    let mut runtime = runtime();
    let mailbox = TestMailbox::default();
    let address = runtime.register(Target, mailbox.clone());

    assert_eq!(mailbox.try_send(TargetMsg::Stop), Ok(()));
    assert_eq!(runtime.step(), 1);

    assert_eq!(
        runtime.try_send(address, TargetMsg::Data(1)),
        Err(TrySendError::Closed(TargetMsg::Data(1)))
    );
}

#[test]
fn runtime_ingress_to_wrong_generation_returns_closed() {
    let mut runtime = runtime();
    let address = runtime.register(Target, TestMailbox::default());
    let stale = Address::new_with_generation(
        address.shard(),
        address.isolate(),
        AddressGeneration::new(address.generation().get() + 1),
    );

    assert_eq!(
        runtime.try_send(stale, TargetMsg::Data(1)),
        Err(TrySendError::Closed(TargetMsg::Data(1)))
    );
}

#[test]
fn dispatched_send_to_wrong_generation_records_closed_rejection() {
    let mut runtime = runtime();
    let target = runtime.register(Target, TestMailbox::default());
    let stale_target = Address::new_with_generation(
        target.shard(),
        target.isolate(),
        AddressGeneration::new(target.generation().get() + 1),
    );
    let driver_mailbox = TestMailbox::default();
    let driver = runtime.register(
        Driver {
            target: stale_target,
        },
        driver_mailbox.clone(),
    );

    assert_eq!(driver_mailbox.try_send(DriverMsg::Kick(7)), Ok(()));
    assert_eq!(runtime.step(), 1);

    assert!(runtime.trace().iter().any(|event| {
        event.isolate() == driver.isolate()
            && event.kind()
                == RuntimeEventKind::SendRejected {
                    target_shard: target.shard(),
                    target_isolate: target.isolate(),
                    target_generation: stale_target.generation(),
                    reason: SendRejectedReason::Closed,
                }
    }));
    assert!(!runtime.trace().iter().any(|event| {
        event.kind()
            == RuntimeEventKind::SendAccepted {
                target_shard: target.shard(),
                target_isolate: target.isolate(),
                target_generation: stale_target.generation(),
            }
    }));
}

#[test]
fn dispatched_send_trace_includes_target_generation() {
    let mut runtime = runtime();
    let target_mailbox = TestMailbox::default();
    let target = runtime.register(Target, target_mailbox);
    let driver_mailbox = TestMailbox::default();
    let driver = runtime.register(Driver { target }, driver_mailbox.clone());

    assert_eq!(driver_mailbox.try_send(DriverMsg::Kick(7)), Ok(()));
    assert_eq!(runtime.step(), 1);

    assert!(runtime.trace().iter().any(|event| {
        event.isolate() == driver.isolate()
            && event.kind()
                == RuntimeEventKind::SendDispatchAttempted {
                    target_shard: target.shard(),
                    target_isolate: target.isolate(),
                    target_generation: target.generation(),
                }
    }));
    assert!(runtime.trace().iter().any(|event| {
        event.isolate() == driver.isolate()
            && event.kind()
                == RuntimeEventKind::SendAccepted {
                    target_shard: target.shard(),
                    target_isolate: target.isolate(),
                    target_generation: target.generation(),
                }
    }));
}

#[test]
fn unknown_isolate_id_still_panics_without_trace_event() {
    let runtime = runtime();
    let synthetic = Address::new(ShardId::new(3), tina::IsolateId::new(99));

    let result = catch_unwind(AssertUnwindSafe(|| {
        let _ = runtime.try_send(synthetic, TargetMsg::Data(1));
    }));

    assert!(result.is_err());
    assert!(runtime.trace().is_empty());
}

#[test]
fn repeated_runs_produce_identical_liveness_traces() {
    fn run_once() -> Vec<RuntimeEventKind> {
        let mut runtime = runtime();
        let target = runtime.register(Target, TestMailbox::default());
        let stale_target = Address::new_with_generation(
            target.shard(),
            target.isolate(),
            AddressGeneration::new(target.generation().get() + 1),
        );
        let driver_mailbox = TestMailbox::default();
        runtime.register(
            Driver {
                target: stale_target,
            },
            driver_mailbox.clone(),
        );

        assert_eq!(driver_mailbox.try_send(DriverMsg::Kick(7)), Ok(()));
        assert_eq!(runtime.step(), 1);

        runtime.trace().iter().map(|event| event.kind()).collect()
    }

    assert_eq!(run_once(), run_once());
}
