use std::cell::Cell;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina::{AddressGeneration, SystemIncarnation, TrySendError};
use tina_runtime::{
    CallOutcome, DefaultMailboxFactory, DefaultThreadedMailboxFactory, LocalSystem,
    MultiShardRuntime, ResultWaitError, Runtime, ThreadedMultiShardRuntime, ThreadedRuntimeConfig,
    ThreadedRuntimeError,
};

#[derive(Debug, Clone, Copy)]
struct TestShard(u32);

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug)]
enum ProbeMsg {
    Set(u32),
}

struct Probe {
    value: u32,
    observed: Option<Rc<Cell<u32>>>,
}

#[tina_runtime::isolate(message = ProbeMsg, reply = u32, shard = TestShard)]
impl Probe {
    fn handle(
        &mut self,
        message: ProbeMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        let ProbeMsg::Set(value) = message;
        self.value = value;
        if let Some(observed) = &self.observed {
            observed.set(value);
        }
        noop()
    }

    fn handle_call(&mut self, message: ProbeMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match message {
            ProbeMsg::Set(value) => {
                self.value = value;
                call.reply(value)
            }
        }
    }
}

#[derive(Debug)]
struct DropMsg(Arc<AtomicU32>);

impl Drop for DropMsg {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

struct DropProbe;

#[tina_runtime::isolate(message = DropMsg, reply = (), shard = TestShard)]
impl DropProbe {
    fn handle(
        &mut self,
        _message: DropMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _message: DropMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(())
    }
}

#[test]
fn separately_constructed_explicit_runtimes_reject_coincident_tuple() {
    let local_seen = Rc::new(Cell::new(0));
    let mut local = Runtime::new(TestShard(7), DefaultMailboxFactory);
    let mut foreign = Runtime::new(TestShard(7), DefaultMailboxFactory);
    let local_address = local.register_with_capacity::<Probe, Infallible>(
        Probe {
            value: 11,
            observed: Some(Rc::clone(&local_seen)),
        },
        4,
    );
    let foreign_address = foreign.register_with_capacity::<Probe, Infallible>(
        Probe {
            value: 99,
            observed: None,
        },
        4,
    );

    assert_eq!(local_address.shard(), foreign_address.shard());
    assert_eq!(local_address.isolate(), foreign_address.isolate());
    assert_eq!(local_address.generation(), foreign_address.generation());
    assert_ne!(local_address.system(), foreign_address.system());
    assert!(matches!(
        local.try_send(foreign_address, ProbeMsg::Set(42)),
        Err(TrySendError::Closed(ProbeMsg::Set(42)))
    ));
    local.step();
    assert_eq!(
        local_seen.get(),
        0,
        "foreign tuple reached the local isolate"
    );
}

#[test]
fn address_capability_wrappers_preserve_provenance() {
    let system = SystemIncarnation::new(0xfeed);
    let raw = Address::<ProbeMsg, u32>::new_with_generation_in(
        system,
        ShardId::new(3),
        IsolateId::new(5),
        AddressGeneration::new(8),
    );
    assert_eq!(raw.with_reply::<()>().system(), system);
    assert_eq!(raw.send_only().system(), system);
    assert_eq!(raw.callable().system(), system);
}

#[test]
fn one_multi_shard_owner_stamps_one_shared_provenance() {
    let mut runtime = MultiShardRuntime::new([TestShard(1), TestShard(2)], DefaultMailboxFactory);
    let first = runtime.register_with_capacity_on::<Probe, Infallible>(
        ShardId::new(1),
        Probe {
            value: 1,
            observed: None,
        },
        4,
    );
    let second = runtime.register_with_capacity_on::<Probe, Infallible>(
        ShardId::new(2),
        Probe {
            value: 2,
            observed: None,
        },
        4,
    );
    assert_eq!(first.system(), runtime.system_incarnation());
    assert_eq!(first.system(), second.system());
}

#[test]
fn threaded_multi_rejects_foreign_coincident_tuple_before_authority_claim() {
    let config = ThreadedRuntimeConfig {
        system_incarnation: Some(SystemIncarnation::new(100)),
        ..ThreadedRuntimeConfig::default()
    };
    let foreign_config = ThreadedRuntimeConfig {
        system_incarnation: Some(SystemIncarnation::new(200)),
        ..ThreadedRuntimeConfig::default()
    };
    let local = ThreadedMultiShardRuntime::with_config(
        [TestShard(9)],
        DefaultThreadedMailboxFactory,
        config,
    );
    let foreign = ThreadedMultiShardRuntime::with_config(
        [TestShard(9)],
        DefaultThreadedMailboxFactory,
        foreign_config,
    );
    let local_address = local
        .register_with_capacity_on::<DropProbe, Infallible>(ShardId::new(9), DropProbe, 4)
        .expect("local registration");
    let foreign_address = foreign
        .register_with_capacity_on::<DropProbe, Infallible>(ShardId::new(9), DropProbe, 4)
        .expect("foreign registration");
    assert_eq!(local_address.isolate(), foreign_address.isolate());
    assert_eq!(local_address.generation(), foreign_address.generation());

    let drops = Arc::new(AtomicU32::new(0));
    assert!(matches!(
        local.call_blocking(
            foreign_address,
            DropMsg(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == SystemIncarnation::new(100)
                && actual == SystemIncarnation::new(200)
    ));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(matches!(
        local.observe_result::<u32, _, _>(foreign_address),
        Err(ResultWaitError::ForeignSystem { expected, actual })
            if expected == SystemIncarnation::new(100)
                && actual == SystemIncarnation::new(200)
    ));
    assert_eq!(
        local
            .call_blocking(
                local_address,
                DropMsg(Arc::clone(&drops)),
                Duration::from_secs(1)
            )
            .expect("local tuple remains callable"),
        CallOutcome::Replied(())
    );
    assert_eq!(drops.load(Ordering::Acquire), 2);

    local.shutdown().expect("local shutdown");
    foreign.shutdown().expect("foreign shutdown");
}

#[test]
fn local_system_facade_rejects_foreign_coincident_tuple() {
    let local = LocalSystem::single_shard(TestShard(6), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("local system starts");
    let foreign = LocalSystem::single_shard(TestShard(6), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("foreign system starts");
    let local_address = local
        .register_root::<DropProbe, Infallible>(DropProbe, 4)
        .expect("local registration");
    let foreign_address = foreign
        .register_root::<DropProbe, Infallible>(DropProbe, 4)
        .expect("foreign registration");
    assert_eq!(local_address.isolate(), foreign_address.isolate());
    assert_ne!(local_address.system(), foreign_address.system());

    let drops = Arc::new(AtomicU32::new(0));
    assert!(matches!(
        local.call_blocking(
            foreign_address,
            DropMsg(Arc::clone(&drops)),
            Duration::from_millis(50),
        ),
        Err(ThreadedRuntimeError::ForeignSystem { expected, actual })
            if expected == local_address.system() && actual == foreign_address.system()
    ));
    assert_eq!(drops.load(Ordering::Acquire), 1);

    local.shutdown().join().expect("local shutdown");
    foreign.shutdown().join().expect("foreign shutdown");
}
