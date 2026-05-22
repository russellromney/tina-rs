//! Live (non-simulator) proof that a parent on one shard can spawn an observed
//! child on another shard and learn its address back.
//!
//! Uses the deterministic multi-shard runtime (`MultiShardRuntime`): real
//! `Runtime` per shard, cross-shard envelopes, stepped in global order. This is
//! the live runtime path, not the simulator.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;

use tina::{ChildDefinition, ChildRef, SpawnObservedError, SpawnObservedRemote, TrySendError, prelude::*};
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
    StopChildren,
    StopNow,
}

#[derive(Debug)]
struct CrossParent {
    learned: Rc<RefCell<Option<ChildRef<CrossChildMsg>>>>,
    error: Rc<RefCell<Option<SpawnObservedError>>>,
}

// Authored through the preferred `#[tina_runtime::isolate]` macro surface,
// proving `spawn_observed_remote` is wired by the macro (not just hand-written
// impls).
#[tina_runtime::isolate(
    message = CrossParentMsg,
    send = Outbound<CrossChildMsg>,
    spawn_observed_remote = SpawnObservedRemote<ChildDefinition<CrossChild>, CrossParentMsg, CrossChildMsg, ()>,
    shard = CrossShard,
)]
impl CrossParent {
    fn handle(
        &mut self,
        msg: CrossParentMsg,
        _ctx: &mut Context<'_, CrossShard, Self::Reply>,
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
            CrossParentMsg::ChildStarted(Err(error)) => {
                *self.error.borrow_mut() = Some(error);
                noop()
            }
            CrossParentMsg::StopChildren => stop_children(),
            CrossParentMsg::StopNow => stop(),
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
            error: Rc::new(RefCell::new(None)),
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

// `.on_shard(my_own_shard)` is the degenerate local case: it must produce an
// ordinary OWNED child (reachable by StopChildren), not an unowned one, and
// must not emit the cross-shard ChildStarted fact.
#[test]
fn on_shard_to_own_shard_makes_an_owned_local_child_no_child_started() {
    let learned = Rc::new(RefCell::new(None));
    let mut runtime =
        MultiShardRuntime::new([CrossShard(11), CrossShard(22)], DefaultMailboxFactory);
    let parent = runtime.register_with_capacity_on::<CrossParent, CrossChildMsg>(
        ShardId::new(11),
        CrossParent {
            learned: Rc::clone(&learned),
            error: Rc::new(RefCell::new(None)),
        },
        8,
    );

    runtime
        .try_send(parent, CrossParentMsg::SpawnOn(ShardId::new(11)))
        .unwrap();
    for _ in 0..6 {
        runtime.step();
    }

    let child = learned.borrow().expect("owner learns the local child address");
    assert_eq!(child.address.shard(), ShardId::new(11), "child is on the owner's shard");
    // Local owned spawn does not record the cross-shard ChildStarted fact.
    assert!(
        !runtime
            .trace()
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::ChildStarted { .. })),
        "same-shard on_shard must not emit ChildStarted"
    );

    // It is genuinely owned: StopChildren on the owner closes it.
    runtime.try_send(parent, CrossParentMsg::StopChildren).unwrap();
    for _ in 0..4 {
        runtime.step();
    }
    assert_eq!(
        runtime.try_send(child.address, CrossChildMsg::Ping),
        Err(TrySendError::Closed(CrossChildMsg::Ping)),
        "owned local child must be stopped by the owner's StopChildren"
    );
}

// If the owner stops before the cross-shard reply lands, the pending spawn must
// be cleaned up: no panic, no continuation misdelivered into the dead owner.
#[test]
fn owner_stop_before_reply_cleans_up_pending_spawn_without_panic() {
    let learned = Rc::new(RefCell::new(None));
    let error = Rc::new(RefCell::new(None));
    let mut runtime =
        MultiShardRuntime::new([CrossShard(11), CrossShard(22)], DefaultMailboxFactory);
    let parent = runtime.register_with_capacity_on::<CrossParent, CrossChildMsg>(
        ShardId::new(11),
        CrossParent {
            learned: Rc::clone(&learned),
            error: Rc::clone(&error),
        },
        8,
    );

    // Step 1: route the spawn request and register the pending record.
    runtime
        .try_send(parent, CrossParentMsg::SpawnOn(ShardId::new(22)))
        .unwrap();
    runtime.step();
    // Stop the owner before the reply can come home, then drain everything.
    runtime.try_send(parent, CrossParentMsg::StopNow).unwrap();
    for _ in 0..12 {
        runtime.step();
    }

    // The continuation was never delivered to the stopped owner.
    assert!(learned.borrow().is_none(), "stopped owner must not learn the address");
    assert!(error.borrow().is_none(), "no error continuation delivered either");
    // No panic reaching here is the cleanup proof; a leaked/late continuation
    // into the dead owner mailbox would otherwise surface as a rejected send.
}
