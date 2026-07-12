//! Live (non-simulator) proof that a parent on one shard can spawn an observed
//! child on another shard and learn its address back.
//!
//! Uses the deterministic multi-shard runtime (`MultiShardRuntime`): real
//! `Runtime` per shard, cross-shard envelopes, stepped in global order. This is
//! the live runtime path, not the simulator.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{
    AddressGeneration, ChildDefinition, ChildRef, CrossShardRestartableChildDefinition,
    SpawnObservedError, SpawnObservedRemote, prelude::*,
};
use tina_runtime::{
    DefaultMailboxFactory, IngressSendError as TrySendError, MultiShardRuntime,
    MultiShardRuntimeConfig, RuntimeCall, RuntimeEventKind, WaitError,
};

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
    type Io = RuntimeCall<Self::Message>;
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
    SpawnTwoOnThenStop(ShardId),
    SpawnOnThenStop(ShardId),
    ChildStarted(Result<ChildRef<CrossChildMsg>, SpawnObservedError>),
    StopChildren,
    RestartChildren,
    StopNow,
}

#[derive(Debug)]
struct CrossParent {
    learned: Rc<RefCell<Option<ChildRef<CrossChildMsg>>>>,
    error: Rc<RefCell<Option<SpawnObservedError>>>,
}

#[derive(Debug)]
enum RestartParentMsg {
    SpawnRestartableOn(ShardId),
    ChildStarted(Result<ChildRef<CrossChildMsg>, SpawnObservedError>),
    RestartChildren,
    StopNow,
}

#[derive(Debug)]
struct RestartParent {
    learned: Rc<RefCell<Option<ChildRef<CrossChildMsg>>>>,
}

#[tina_runtime::isolate(
    message = RestartParentMsg,
    send = Outbound<CrossChildMsg>,
    spawn_observed_remote = SpawnObservedRemote<CrossShardRestartableChildDefinition<CrossChild>, RestartParentMsg, CrossChildMsg, ()>,
    shard = CrossShard,
)]
impl RestartParent {
    fn handle(
        &mut self,
        msg: RestartParentMsg,
        _ctx: &mut Context<'_, CrossShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RestartParentMsg::SpawnRestartableOn(shard) => {
                spawn_observed(CrossShardRestartableChildDefinition::new(|| CrossChild, 4))
                    .on_shard(shard)
                    .then(RestartParentMsg::ChildStarted)
            }
            RestartParentMsg::ChildStarted(Ok(child)) => {
                *self.learned.borrow_mut() = Some(child);
                noop()
            }
            RestartParentMsg::ChildStarted(Err(_)) => noop(),
            RestartParentMsg::RestartChildren => restart_children(),
            RestartParentMsg::StopNow => stop(),
        }
    }
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
            CrossParentMsg::SpawnTwoOnThenStop(shard) => batch(vec![
                spawn_observed(ChildDefinition::new(CrossChild, 4))
                    .on_shard(shard)
                    .then(CrossParentMsg::ChildStarted),
                spawn_observed(ChildDefinition::new(CrossChild, 4))
                    .on_shard(shard)
                    .then(CrossParentMsg::ChildStarted),
                stop(),
            ]),
            CrossParentMsg::SpawnOnThenStop(shard) => batch(vec![
                spawn_observed(ChildDefinition::new(CrossChild, 4))
                    .on_shard(shard)
                    .then(CrossParentMsg::ChildStarted),
                stop(),
            ]),
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
            CrossParentMsg::RestartChildren => restart_children(),
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
    let mut runtime = MultiShardRuntime::with_config(
        [CrossShard(11), CrossShard(22)],
        DefaultMailboxFactory,
        MultiShardRuntimeConfig {
            shard_pair_capacity: 1,
        },
    );
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

    let child = learned
        .borrow()
        .expect("owner learns the local child address");
    assert_eq!(
        child.address.shard(),
        ShardId::new(11),
        "child is on the owner's shard"
    );
    // Local owned spawn does not record the cross-shard ChildStarted fact.
    assert!(
        !runtime
            .trace()
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::ChildStarted { .. })),
        "same-shard on_shard must not emit ChildStarted"
    );

    // It is genuinely owned: StopChildren on the owner closes it.
    runtime
        .try_send(parent, CrossParentMsg::StopChildren)
        .unwrap();
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
    assert!(
        learned.borrow().is_none(),
        "stopped owner must not learn the address"
    );
    assert!(
        error.borrow().is_none(),
        "no error continuation delivered either"
    );
    // No panic reaching here is the cleanup proof; a leaked/late continuation
    // into the dead owner mailbox would otherwise surface as a rejected send.
}

// `batch([SpawnObservedOn(remote), stop()])` routes the request, then the same
// turn's stop cancels or stops the remote child. No continuation reaches the
// stopped owner, and the remote address (if admitted) is closed.
#[test]
fn cross_shard_child_ownership_spawn_on_remote_then_stop_in_one_turn_is_safe() {
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

    runtime
        .try_send(parent, CrossParentMsg::SpawnOnThenStop(ShardId::new(22)))
        .unwrap();
    for _ in 0..12 {
        runtime.step();
    }

    assert!(
        learned.borrow().is_none() && error.borrow().is_none(),
        "owner stopped in the same turn must not receive any continuation"
    );
    let trace = runtime.trace();
    if let Some((_, child_isolate)) = trace.iter().find_map(|event| {
        if event.shard() == ShardId::new(22) {
            if let RuntimeEventKind::Spawned { child_isolate } = event.kind() {
                return Some((event.shard(), child_isolate));
            }
        }
        None
    }) {
        let child = tina::Address::<CrossChildMsg>::new_in(
            runtime.system_incarnation(),
            ShardId::new(22),
            child_isolate,
        );
        assert_eq!(
            runtime.try_send(child, CrossChildMsg::Ping),
            Err(TrySendError::Closed(CrossChildMsg::Ping)),
            "admitted remote child must be stopped by owner cleanup"
        );
    }
    assert!(
        trace
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::RemoteChildStopped { .. }))
            || !trace.iter().any(|event| event.shard() == ShardId::new(22)
                && matches!(event.kind(), RuntimeEventKind::Spawned { .. })),
        "remote spawn is either cancelled before admission or stopped after admission"
    );
}

#[test]
fn cross_shard_child_ownership_stop_children_stops_remote_child_and_lifecycle_report_records_it() {
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
    for _ in 0..12 {
        runtime.step();
    }
    let child = learned.borrow().expect("remote child address learned");
    let report = runtime
        .child_lifecycle_report(parent)
        .expect("live lifecycle report");
    assert_eq!(report.children.len(), 1);
    assert_eq!(report.children[0].shard, ShardId::new(22));

    runtime
        .try_send(parent, CrossParentMsg::StopChildren)
        .unwrap();
    for _ in 0..12 {
        runtime.step();
    }

    assert_eq!(
        runtime.try_send(child.address, CrossChildMsg::Ping),
        Err(TrySendError::Closed(CrossChildMsg::Ping))
    );
    let report = runtime
        .child_lifecycle_report(parent)
        .expect("live lifecycle report after stop_children");
    assert!(matches!(
        report.children[0].state,
        tina_runtime::ChildLifecycleState::Stopped
    ));
    assert!(runtime.trace().iter().any(|event| matches!(
        event.kind(),
        RuntimeEventKind::RemoteChildStopRequested { child_shard, .. }
            if child_shard == ShardId::new(22)
    )));
    assert!(runtime.trace().iter().any(|event| matches!(
        event.kind(),
        RuntimeEventKind::RemoteChildStopped { child_shard, .. }
            if child_shard == ShardId::new(22)
    )));
}

#[test]
fn cross_shard_child_ownership_remote_owner_id_does_not_collide_with_local_parent() {
    let learned = Rc::new(RefCell::new(None));
    let mut runtime =
        MultiShardRuntime::new([CrossShard(11), CrossShard(22)], DefaultMailboxFactory);
    let owner = runtime.register_with_capacity_on::<CrossParent, CrossChildMsg>(
        ShardId::new(11),
        CrossParent {
            learned: Rc::clone(&learned),
            error: Rc::new(RefCell::new(None)),
        },
        8,
    );
    let local_same_numeric_parent = runtime
        .register_with_capacity_on::<CrossParent, CrossChildMsg>(
            ShardId::new(22),
            CrossParent {
                learned: Rc::new(RefCell::new(None)),
                error: Rc::new(RefCell::new(None)),
            },
            8,
        );
    assert_eq!(owner.isolate(), local_same_numeric_parent.isolate());

    runtime
        .try_send(owner, CrossParentMsg::SpawnOn(ShardId::new(22)))
        .unwrap();
    for _ in 0..12 {
        runtime.step();
    }
    let child = learned.borrow().expect("remote child address learned");

    runtime
        .try_send(local_same_numeric_parent, CrossParentMsg::StopChildren)
        .unwrap();
    for _ in 0..8 {
        runtime.step();
    }

    assert_eq!(runtime.try_send(child.address, CrossChildMsg::Ping), Ok(()));
}

#[test]
fn cross_shard_child_ownership_cancel_pressure_does_not_orphan_admitted_children() {
    let learned = Rc::new(RefCell::new(None));
    let mut runtime = MultiShardRuntime::with_config(
        [CrossShard(11), CrossShard(22)],
        DefaultMailboxFactory,
        MultiShardRuntimeConfig {
            shard_pair_capacity: 2,
        },
    );
    let parent = runtime.register_with_capacity_on::<CrossParent, CrossChildMsg>(
        ShardId::new(11),
        CrossParent {
            learned: Rc::clone(&learned),
            error: Rc::new(RefCell::new(None)),
        },
        8,
    );

    runtime
        .try_send(parent, CrossParentMsg::SpawnTwoOnThenStop(ShardId::new(22)))
        .unwrap();
    for _ in 0..20 {
        runtime.step();
    }

    assert!(learned.borrow().is_none());
    let spawned_children: Vec<_> = runtime
        .trace()
        .iter()
        .filter_map(|event| {
            if event.shard() == ShardId::new(22) {
                if let RuntimeEventKind::Spawned { child_isolate } = event.kind() {
                    return Some(child_isolate);
                }
            }
            None
        })
        .collect();
    for child_isolate in spawned_children {
        let child = tina::Address::<CrossChildMsg>::new_in(
            runtime.system_incarnation(),
            ShardId::new(22),
            child_isolate,
        );
        assert_eq!(
            runtime.try_send(child, CrossChildMsg::Ping),
            Err(TrySendError::Closed(CrossChildMsg::Ping))
        );
    }
    assert!(!runtime.trace().iter().any(|event| matches!(
        event.kind(),
        RuntimeEventKind::RemoteChildControlRejected {
            reason: tina_runtime::SendRejectedReason::Full,
            ..
        }
    )));
}

#[test]
fn cross_shard_restartable_child_restarts_on_remote_shard_and_reports_replacement_address() {
    let learned = Rc::new(RefCell::new(None));
    let mut runtime =
        MultiShardRuntime::new([CrossShard(11), CrossShard(22)], DefaultMailboxFactory);
    let parent = runtime.register_with_capacity_on::<RestartParent, CrossChildMsg>(
        ShardId::new(11),
        RestartParent {
            learned: Rc::clone(&learned),
        },
        8,
    );
    let collision_parent = runtime.register_with_capacity_on::<RestartParent, CrossChildMsg>(
        ShardId::new(22),
        RestartParent {
            learned: Rc::new(RefCell::new(None)),
        },
        8,
    );
    assert_eq!(
        parent.isolate(),
        collision_parent.isolate(),
        "the fixture must exercise equal isolate ids on different shards"
    );

    runtime
        .try_send(
            parent,
            RestartParentMsg::SpawnRestartableOn(ShardId::new(22)),
        )
        .unwrap();
    for _ in 0..12 {
        runtime.step();
    }
    let old_child = learned.borrow().expect("remote child address learned");
    let stale_parent = Address::<RestartParentMsg>::new_with_generation_in(
        parent.system(),
        parent.shard(),
        parent.isolate(),
        AddressGeneration::new(parent.generation().get() + 1),
    );
    let foreign_parent = Address::<RestartParentMsg>::new_with_generation_in(
        parent.system(),
        ShardId::new(22),
        parent.isolate(),
        parent.generation(),
    );
    let stale_waiter = runtime.observe_child_restarted(stale_parent);
    let foreign_waiter = runtime.observe_child_restarted(foreign_parent);
    let collision_waiter = runtime.observe_child_restarted(collision_parent);
    let restart_waiter = runtime.observe_child_restarted(parent);

    runtime
        .try_send(parent, RestartParentMsg::RestartChildren)
        .unwrap();
    for _ in 0..12 {
        runtime.step();
    }

    let restarted = restart_waiter
        .wait(Duration::from_secs(1))
        .expect("remote restart waiter resolves");
    assert_eq!(restarted.child_ordinal, 0);
    assert_eq!(restarted.new_shard, ShardId::new(22));
    assert_ne!(restarted.new_isolate, old_child.address.isolate());
    assert_eq!(
        stale_waiter.wait(Duration::from_millis(10)),
        Err(WaitError::AlreadyStopped),
        "a stale owner generation must be rejected before claiming a cross-shard restart"
    );
    assert_eq!(
        foreign_waiter.wait(Duration::from_millis(10)),
        Err(WaitError::Timeout),
        "the live same-id isolate on another owner shard must not claim the restart"
    );
    assert_eq!(
        collision_waiter.wait(Duration::from_millis(10)),
        Err(WaitError::Timeout),
        "another live parent with the same isolate id must retain its waiter"
    );
    assert_eq!(
        runtime
            .observe_child_restarted(parent)
            .wait(Duration::from_millis(10)),
        Err(WaitError::Timeout),
        "cross-shard restart facts are not replayed"
    );

    let replacement = tina::Address::<CrossChildMsg>::new_with_generation_in(
        parent.system(),
        restarted.new_shard,
        restarted.new_isolate,
        restarted.new_generation,
    );
    assert_eq!(
        runtime.try_send(old_child.address, CrossChildMsg::Ping),
        Err(TrySendError::Closed(CrossChildMsg::Ping)),
        "old remote child address must be stale after restart"
    );
    assert_eq!(
        runtime.try_send(replacement, CrossChildMsg::Ping),
        Ok(()),
        "replacement address from typed waiter must be live"
    );

    let report = runtime
        .child_lifecycle_report(parent)
        .expect("remote restart lifecycle report");
    assert_eq!(report.children.len(), 1);
    assert_eq!(report.children[0].shard, ShardId::new(22));
    assert_eq!(report.children[0].isolate, restarted.new_isolate);
    assert_eq!(report.children[0].generation, restarted.new_generation);
    assert!(matches!(
        report.children[0].state,
        tina_runtime::ChildLifecycleState::Restarted
    ));
    assert!(runtime.trace().iter().any(|event| matches!(
        event.kind(),
        RuntimeEventKind::RestartChildCompleted {
            new_isolate,
            new_generation,
            ..
        } if new_isolate == restarted.new_isolate
            && new_generation == restarted.new_generation
            && event.shard() == ShardId::new(11)
    )));
}

#[test]
fn cross_shard_restart_children_skips_non_restartable_remote_child_without_faking_local_restart() {
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
    for _ in 0..12 {
        runtime.step();
    }
    let child = learned.borrow().expect("remote child address learned");

    runtime
        .try_send(parent, CrossParentMsg::RestartChildren)
        .unwrap();
    for _ in 0..8 {
        runtime.step();
    }

    assert_eq!(
        runtime.try_send(child.address, CrossChildMsg::Ping),
        Ok(()),
        "non-restartable remote child remains live at its original address"
    );
    assert!(runtime.trace().iter().any(|event| matches!(
        event.kind(),
        RuntimeEventKind::RestartChildSkipped {
            reason: tina_runtime::RestartSkippedReason::RemoteNotRestartable,
            ..
        } if event.shard() == ShardId::new(11)
    )));
    assert!(
        !runtime
            .trace()
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::RestartChildCompleted { .. }))
    );
}

#[test]
fn owner_stop_racing_remote_restart_stops_replacement_child_too() {
    let learned = Rc::new(RefCell::new(None));
    let mut runtime =
        MultiShardRuntime::new([CrossShard(11), CrossShard(22)], DefaultMailboxFactory);
    let parent = runtime.register_with_capacity_on::<RestartParent, CrossChildMsg>(
        ShardId::new(11),
        RestartParent {
            learned: Rc::clone(&learned),
        },
        8,
    );

    runtime
        .try_send(
            parent,
            RestartParentMsg::SpawnRestartableOn(ShardId::new(22)),
        )
        .unwrap();
    for _ in 0..12 {
        runtime.step();
    }

    runtime
        .try_send(parent, RestartParentMsg::RestartChildren)
        .unwrap();
    runtime.step();
    runtime.try_send(parent, RestartParentMsg::StopNow).unwrap();
    for _ in 0..12 {
        runtime.step();
    }

    let old_child = learned
        .borrow()
        .expect("initial child address learned")
        .address;
    let mut remote_children = vec![old_child];
    remote_children.extend(runtime.trace().iter().filter_map(|event| {
        if let RuntimeEventKind::RestartChildCompleted { new_isolate, .. } = event.kind() {
            return Some(tina::Address::<CrossChildMsg>::new_in(
                runtime.system_incarnation(),
                ShardId::new(22),
                new_isolate,
            ));
        }
        None
    }));
    assert!(
        remote_children.len() >= 2,
        "restart race should create an initial child and a replacement"
    );
    for child in remote_children {
        assert_eq!(
            runtime.try_send(child, CrossChildMsg::Ping),
            Err(TrySendError::Closed(CrossChildMsg::Ping)),
            "owner stop must not orphan any remote restart incarnation"
        );
    }
}
