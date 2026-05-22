//! Live (non-simulator) proof that a parent on one shard can spawn an observed
//! child on another shard and learn its address back.
//!
//! Uses the deterministic multi-shard runtime (`MultiShardRuntime`): real
//! `Runtime` per shard, cross-shard envelopes, stepped in global order. This is
//! the live runtime path, not the simulator.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;

use tina::{ChildDefinition, ChildRef, SpawnObservedError, prelude::*};
use tina_runtime::{DefaultMailboxFactory, MultiShardRuntime, RuntimeCall, RuntimeEventKind};

#[derive(Debug, Clone, Copy)]
struct CrossShard(u32);

impl Shard for CrossShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrossChildMsg {
    Ping,
}

// The cross-shard child must be `Send` (its constructor crosses the shard
// boundary), so it holds no `Rc`.
#[derive(Debug)]
struct CrossChild;

impl Isolate for CrossChild {
    type Message = CrossChildMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<Self::Message>;
    type Fact = Infallible;
    type Shard = CrossShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CrossChildMsg::Ping => noop(),
        }
    }
}

#[derive(Debug)]
enum CrossParentMsg {
    SpawnOn(ShardId),
    ChildStarted(Result<ChildRef<CrossChildMsg>, SpawnObservedError>),
}

#[derive(Debug)]
struct CrossParent {
    learned: Rc<RefCell<Option<ChildRef<CrossChildMsg>>>>,
}

impl Isolate for CrossParent {
    type Message = CrossParentMsg;
    type Reply = ();
    type Send = Outbound<CrossChildMsg>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type SpawnObservedRemote =
        tina::SpawnObservedRemote<ChildDefinition<CrossChild>, Self::Message, CrossChildMsg, ()>;
    type Call = RuntimeCall<Self::Message>;
    type Fact = Infallible;
    type Shard = CrossShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CrossParentMsg::SpawnOn(shard) => spawn_observed(ChildDefinition::new(CrossChild, 4))
                .on_shard(shard)
                .then(CrossParentMsg::ChildStarted),
            CrossParentMsg::ChildStarted(Ok(child)) => {
                *self.learned.borrow_mut() = Some(child);
                // Address the cross-shard child through the learned ref.
                send(child.address, CrossChildMsg::Ping)
            }
            CrossParentMsg::ChildStarted(Err(_)) => noop(),
        }
    }
}

#[test]
fn live_multishard_parent_spawns_observed_child_on_another_shard_and_learns_address() {
    let learned = Rc::new(RefCell::new(None));
    let mut runtime =
        MultiShardRuntime::new([CrossShard(11), CrossShard(22)], DefaultMailboxFactory);
    let parent = runtime.register_with_capacity_on::<CrossParent, CrossChildMsg>(
        ShardId::new(11),
        CrossParent {
            learned: Rc::clone(&learned),
        },
        8,
    );

    runtime
        .try_send(parent, CrossParentMsg::SpawnOn(ShardId::new(22)))
        .unwrap();
    // Drive the round trip to quiescence: request 11->22, register + reply
    // 22->11, continuation on 11, ping 11->22.
    for _ in 0..12 {
        runtime.step();
    }

    // The owner learned the child's address, on the target shard.
    let child = learned
        .borrow()
        .expect("owner learns the cross-shard child address");
    assert_eq!(child.address.shard(), ShardId::new(22));

    let trace = runtime.trace();
    // The child was created on shard 22, not the owner's shard 11.
    assert!(
        trace.iter().any(|event| event.shard() == ShardId::new(22)
            && matches!(event.kind(), RuntimeEventKind::Spawned { .. })),
        "child must be spawned on the target shard"
    );
    // The owner (shard 11) recorded the ChildStarted truth naming shard 22.
    assert!(
        trace.iter().any(|event| event.shard() == ShardId::new(11)
            && matches!(
                event.kind(),
                RuntimeEventKind::ChildStarted { child_shard, .. }
                    if child_shard == ShardId::new(22)
            )),
        "owner must record ChildStarted for the cross-shard child"
    );
}
