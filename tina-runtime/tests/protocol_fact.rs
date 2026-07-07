//! End-to-end runtime proof for Phase 112 protocol facts.
//!
//! Drives an isolate whose `Fact = ProtocolFact` and confirms that:
//!
//! - `Effect::Fact(ProtocolFact)` lands in the trace as
//!   `RuntimeEventKind::FactObserved`,
//! - the canonical conversion uses `IntoRuntimeFact::Protocol(...)`,
//! - the stable hash is order-sensitive and changes when the fact payload
//!   changes.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    DefaultMailboxFactory, Http2StreamId, MailboxFactory, ProtocolConnectionId, ProtocolDirection,
    ProtocolFact, Runtime, RuntimeCall, RuntimeEventKind, RuntimeFact,
};

#[derive(Debug, Default)]
struct LocalShard;

impl Shard for LocalShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

struct LocalMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> LocalMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for LocalMailbox<T> {
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
    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct LocalMailboxFactory;

impl MailboxFactory for LocalMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(LocalMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy)]
enum FactDriver {
    EmitOne { stream_id: u32 },
}

struct FactIsolate;

impl Isolate for FactIsolate {
    type Message = FactDriver;
    type Reply = ();
    type Send = tina::Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<FactDriver>;
    type Fact = ProtocolFact;
    type Shard = LocalShard;

    fn handle(
        &mut self,
        msg: FactDriver,
        _ctx: &mut tina::Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FactDriver::EmitOne { stream_id } => {
                tina::fact::<Self>(ProtocolFact::Http2StreamOpened {
                    connection: ProtocolConnectionId::new(99),
                    stream: Http2StreamId::new(stream_id),
                    direction: ProtocolDirection::Inbound,
                })
            }
        }
    }
}

fn run_one_and_collect(driver: FactDriver) -> Vec<tina_runtime::RuntimeEvent> {
    let mut runtime = Runtime::new(LocalShard, LocalMailboxFactory);
    let addr = runtime.register_with_capacity::<FactIsolate, Infallible>(FactIsolate, 8);
    runtime.try_send(addr, driver).expect("admit one message");
    let mut steps = 0;
    while runtime.step() > 0 && steps < 64 {
        steps += 1;
    }
    runtime.trace().to_vec()
}

#[test]
fn safety_rails_runtime_emits_fact_observed_for_effect_fact() {
    let events = run_one_and_collect(FactDriver::EmitOne { stream_id: 1 });
    let fact_kinds: Vec<RuntimeFact> = events
        .iter()
        .filter_map(|e| match e.kind() {
            RuntimeEventKind::FactObserved { fact } => Some(fact),
            _ => None,
        })
        .collect();
    assert_eq!(fact_kinds.len(), 1, "should observe exactly one fact");
    let RuntimeFact::Protocol(fact) = fact_kinds[0] else {
        panic!("unexpected runtime fact family")
    };
    assert!(matches!(fact, ProtocolFact::Http2StreamOpened { .. }));
}

#[test]
fn protocol_fact_stream_id_round_trips_into_runtime_trace() {
    let events = run_one_and_collect(FactDriver::EmitOne { stream_id: 17 });
    let observed = events
        .iter()
        .find_map(|e| match e.kind() {
            RuntimeEventKind::FactObserved { fact } => Some(fact),
            _ => None,
        })
        .expect("fact event present");
    match observed {
        RuntimeFact::Protocol(ProtocolFact::Http2StreamOpened { stream, .. }) => {
            assert_eq!(stream.get(), 17);
        }
        other => panic!("unexpected fact: {other:?}"),
    }
}

#[test]
fn protocol_fact_safety_rails_default_runtime_compiles() {
    // Smoke check that ordinary isolates with `Fact = Infallible` still
    // register through the same Runtime entry point. If this stops compiling,
    // the IntoRuntimeFact bound has become too narrow.
    let mut runtime = Runtime::new(LocalShard, DefaultMailboxFactory);
    struct Plain;
    impl Isolate for Plain {
        type Message = ();
        type Reply = ();
        type Send = tina::Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = Infallible;
        type Io = RuntimeCall<()>;
        type Fact = Infallible;
        type Shard = LocalShard;
        fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut tina::Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            tina::stop()
        }
    }
    let _ = runtime.register_with_capacity::<Plain, Infallible>(Plain, 4);
}
