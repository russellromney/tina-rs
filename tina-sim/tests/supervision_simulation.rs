use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::rc::Rc;

use tina::{
    Address, AddressGeneration, ChildDefinition, Context, Effect, Isolate, IsolateId, Outbound,
    RestartBudget, RestartPolicy, RestartableChildDefinition, Shard, ShardId,
};
use tina_runtime::{
    RestartSkippedReason, RuntimeCall, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
    SupervisionRejectedReason,
};
use tina_sim::{
    Checker, CheckerDecision, FaultConfig, LocalSendFaultMode, Simulator, SimulatorConfig,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(71)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Never {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerMsg {
    Boot,
    Work(u32),
    Poison,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerObservation {
    Booted(IsolateId),
    Worked(IsolateId, u32),
}

#[derive(Debug)]
struct Worker {
    observations: Rc<RefCell<Vec<WorkerObservation>>>,
}

impl Isolate for Worker {
    type Message = WorkerMsg;
    type Reply = ();
    type Send = Outbound<Never>;
    type Spawn = Infallible;
    type Call = RuntimeCall<WorkerMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Boot => {
                self.observations
                    .borrow_mut()
                    .push(WorkerObservation::Booted(ctx.isolate_id()));
                Effect::Noop
            }
            WorkerMsg::Work(value) => {
                self.observations
                    .borrow_mut()
                    .push(WorkerObservation::Worked(ctx.isolate_id(), value));
                Effect::Noop
            }
            WorkerMsg::Poison => panic!("simulated worker panic"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentMsg {
    SpawnOne,
    SpawnTwo,
    RestartChildren,
}

#[derive(Debug)]
struct RestartableParent {
    observations: Rc<RefCell<Vec<WorkerObservation>>>,
}

impl RestartableParent {
    fn spec(&self) -> RestartableChildDefinition<Worker> {
        let observations = Rc::clone(&self.observations);
        RestartableChildDefinition::new(
            move || Worker {
                observations: Rc::clone(&observations),
            },
            8,
        )
        .with_initial_message(|| WorkerMsg::Boot)
    }
}

impl Isolate for RestartableParent {
    type Message = ParentMsg;
    type Reply = ();
    type Send = Outbound<Never>;
    type Spawn = RestartableChildDefinition<Worker>;
    type Call = RuntimeCall<ParentMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ParentMsg::SpawnOne => Effect::Spawn(self.spec()),
            ParentMsg::SpawnTwo => {
                Effect::Batch(vec![Effect::Spawn(self.spec()), Effect::Spawn(self.spec())])
            }
            ParentMsg::RestartChildren => Effect::RestartChildren,
        }
    }
}

#[derive(Debug)]
struct DynamicBootstrapParent {
    observations: Rc<RefCell<Vec<WorkerObservation>>>,
    next_bootstrap: Rc<Cell<u32>>,
}

impl Isolate for DynamicBootstrapParent {
    type Message = ParentMsg;
    type Reply = ();
    type Send = Outbound<Never>;
    type Spawn = RestartableChildDefinition<Worker>;
    type Call = RuntimeCall<ParentMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ParentMsg::SpawnOne => {
                let observations = Rc::clone(&self.observations);
                let next_bootstrap = Rc::clone(&self.next_bootstrap);
                Effect::Spawn(
                    RestartableChildDefinition::new(
                        move || Worker {
                            observations: Rc::clone(&observations),
                        },
                        8,
                    )
                    .with_initial_message(move || {
                        let value = next_bootstrap.get();
                        next_bootstrap.set(value + 1);
                        WorkerMsg::Work(value)
                    }),
                )
            }
            ParentMsg::RestartChildren => Effect::RestartChildren,
            ParentMsg::SpawnTwo => unreachable!("dynamic parent only spawns one child"),
        }
    }
}

#[derive(Debug)]
struct OneShotParent {
    observations: Rc<RefCell<Vec<WorkerObservation>>>,
}

impl Isolate for OneShotParent {
    type Message = ParentMsg;
    type Reply = ();
    type Send = Outbound<Never>;
    type Spawn = ChildDefinition<Worker>;
    type Call = RuntimeCall<ParentMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ParentMsg::SpawnOne => Effect::Spawn(
                ChildDefinition::new(
                    Worker {
                        observations: Rc::clone(&self.observations),
                    },
                    8,
                )
                .with_initial_message(WorkerMsg::Boot),
            ),
            ParentMsg::RestartChildren => Effect::RestartChildren,
            ParentMsg::SpawnTwo => unreachable!("one-shot parent only spawns one child"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SenderMsg {
    SendToStale,
}

#[derive(Debug)]
struct StaleSender {
    target: Address<WorkerMsg>,
}

impl Isolate for StaleSender {
    type Message = SenderMsg;
    type Reply = ();
    type Send = Outbound<WorkerMsg>;
    type Spawn = Infallible;
    type Call = RuntimeCall<SenderMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            SenderMsg::SendToStale => Effect::Send(Outbound::new(self.target, WorkerMsg::Work(99))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildMsg {
    Boot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrandchildMsg {
    Boot,
}

#[derive(Debug)]
struct Grandchild {
    boots: Rc<RefCell<Vec<IsolateId>>>,
}

impl Isolate for Grandchild {
    type Message = GrandchildMsg;
    type Reply = ();
    type Send = Outbound<Never>;
    type Spawn = Infallible;
    type Call = RuntimeCall<GrandchildMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            GrandchildMsg::Boot => {
                self.boots.borrow_mut().push(ctx.isolate_id());
                Effect::Noop
            }
        }
    }
}

#[derive(Debug)]
struct ChildSpawner {
    grandchild_boots: Rc<RefCell<Vec<IsolateId>>>,
}

impl Isolate for ChildSpawner {
    type Message = ChildMsg;
    type Reply = ();
    type Send = Outbound<Never>;
    type Spawn = RestartableChildDefinition<Grandchild>;
    type Call = RuntimeCall<ChildMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ChildMsg::Boot => {
                let boots = Rc::clone(&self.grandchild_boots);
                Effect::Spawn(
                    RestartableChildDefinition::new(
                        move || Grandchild {
                            boots: Rc::clone(&boots),
                        },
                        8,
                    )
                    .with_initial_message(|| GrandchildMsg::Boot),
                )
            }
        }
    }
}

#[derive(Debug)]
struct NestedParent {
    grandchild_boots: Rc<RefCell<Vec<IsolateId>>>,
}

impl Isolate for NestedParent {
    type Message = ParentMsg;
    type Reply = ();
    type Send = Outbound<Never>;
    type Spawn = RestartableChildDefinition<ChildSpawner>;
    type Call = RuntimeCall<ParentMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ParentMsg::SpawnOne => {
                let grandchild_boots = Rc::clone(&self.grandchild_boots);
                Effect::Spawn(
                    RestartableChildDefinition::new(
                        move || ChildSpawner {
                            grandchild_boots: Rc::clone(&grandchild_boots),
                        },
                        8,
                    )
                    .with_initial_message(|| ChildMsg::Boot),
                )
            }
            ParentMsg::RestartChildren => Effect::RestartChildren,
            ParentMsg::SpawnTwo => unreachable!("nested parent only spawns one child"),
        }
    }
}

#[derive(Debug, Default)]
struct StrictlyIncreasingRestartChecker {
    last_new_child: Option<IsolateId>,
}

impl Checker for StrictlyIncreasingRestartChecker {
    fn name(&self) -> &'static str {
        "strictly-increasing-restart-checker"
    }

    fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision {
        match event.kind() {
            RuntimeEventKind::RestartChildCompleted { new_isolate, .. } => {
                if let Some(last) = self.last_new_child {
                    if new_isolate <= last {
                        return CheckerDecision::Fail(format!(
                            "restart child id {:?} did not increase after {:?}",
                            new_isolate, last
                        ));
                    }
                }
                self.last_new_child = Some(new_isolate);
                CheckerDecision::Continue
            }
            _ => CheckerDecision::Continue,
        }
    }
}

#[derive(Debug, Default)]
struct NoRestartCompletionChecker;

impl Checker for NoRestartCompletionChecker {
    fn name(&self) -> &'static str {
        "no-restart-completion-checker"
    }

    fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision {
        match event.kind() {
            RuntimeEventKind::RestartChildCompleted {
                old_isolate,
                new_isolate,
                ..
            } => CheckerDecision::Fail(format!(
                "restart completed from {:?} to {:?}",
                old_isolate, new_isolate
            )),
            _ => CheckerDecision::Continue,
        }
    }
}

fn spawned_children(trace: &[RuntimeEvent]) -> Vec<IsolateId> {
    trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::Spawned { child_isolate } => Some(child_isolate),
            _ => None,
        })
        .collect()
}

fn run_restart_checker_failure() -> tina_sim::ReplayArtifact {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let parent = sim.register(RestartableParent { observations });
    sim.supervise(
        parent,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(1)),
    );
    sim.try_send(parent, ParentMsg::SpawnOne).unwrap();
    sim.run_until_quiescent();

    let child = spawned_children(sim.trace())[0];
    sim.try_send(address_for_worker(child), WorkerMsg::Poison)
        .unwrap();
    let mut checker = NoRestartCompletionChecker;
    let failure = sim
        .run_until_quiescent_checked(&mut checker)
        .expect("restart completion should trip the adversarial checker");
    assert_eq!(failure.checker_name(), "no-restart-completion-checker");
    assert!(
        failure.reason().contains("restart completed"),
        "failure reason should describe the restart-sensitive invariant"
    );

    sim.replay_artifact()
}

fn completed_restarts(trace: &[RuntimeEvent]) -> Vec<(IsolateId, IsolateId)> {
    trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::RestartChildCompleted {
                old_isolate,
                new_isolate,
                ..
            } => Some((old_isolate, new_isolate)),
            _ => None,
        })
        .collect()
}

fn address_for_worker(isolate: IsolateId) -> Address<WorkerMsg> {
    Address::new_with_generation(ShardId::new(71), isolate, AddressGeneration::new(0))
}

fn run_restarted_parent(
    policy: RestartPolicy,
    poisoned_child_index: usize,
) -> (Vec<RuntimeEvent>, Vec<WorkerObservation>) {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let parent = sim.register(RestartableParent {
        observations: Rc::clone(&observations),
    });
    sim.supervise(
        parent,
        SupervisorConfig::new(policy, RestartBudget::new(10)),
    );
    sim.try_send(parent, ParentMsg::SpawnTwo).unwrap();
    sim.run_until_quiescent();

    let child = spawned_children(sim.trace())[poisoned_child_index];
    sim.try_send(address_for_worker(child), WorkerMsg::Poison)
        .unwrap();
    sim.run_until_quiescent();

    (sim.trace().to_vec(), observations.borrow().clone())
}

fn run_restart_then_delayed_send() -> (Vec<RuntimeEvent>, Vec<WorkerObservation>) {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 5,
            faults: FaultConfig {
                local_send: LocalSendFaultMode::DelayByRounds {
                    one_in: 1,
                    rounds: 2,
                },
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let parent = sim.register(RestartableParent {
        observations: Rc::clone(&observations),
    });
    sim.supervise(
        parent,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(2)),
    );
    sim.try_send(parent, ParentMsg::SpawnOne).unwrap();
    sim.run_until_quiescent();

    let first = spawned_children(sim.trace())[0];
    sim.try_send(address_for_worker(first), WorkerMsg::Poison)
        .unwrap();
    sim.run_until_quiescent();
    let replacement = completed_restarts(sim.trace())[0].1;

    let sender = sim.register(StaleSender {
        target: address_for_worker(replacement),
    });
    sim.try_send(sender, SenderMsg::SendToStale).unwrap();
    sim.run_until_quiescent();

    (sim.trace().to_vec(), observations.borrow().clone())
}

#[test]
fn spawned_child_runs_on_later_step_and_batch_spawn_order_is_deterministic() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let parent = sim.register(RestartableParent {
        observations: Rc::clone(&observations),
    });
    sim.try_send(parent, ParentMsg::SpawnTwo).unwrap();

    assert_eq!(sim.step(), 1);
    assert!(observations.borrow().is_empty());

    assert_eq!(sim.step(), 2);
    let children = spawned_children(sim.trace());
    assert_eq!(children.len(), 2);
    assert_eq!(
        observations.borrow().as_slice(),
        [
            WorkerObservation::Booted(children[0]),
            WorkerObservation::Booted(children[1]),
        ]
    );
}

#[test]
fn same_step_spawns_from_different_parents_follow_registration_order() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let first_parent = sim.register(RestartableParent {
        observations: Rc::clone(&observations),
    });
    let second_parent = sim.register(RestartableParent {
        observations: Rc::clone(&observations),
    });
    sim.try_send(first_parent, ParentMsg::SpawnOne).unwrap();
    sim.try_send(second_parent, ParentMsg::SpawnOne).unwrap();

    assert_eq!(sim.step(), 2);
    assert!(observations.borrow().is_empty());

    let children = spawned_children(sim.trace());
    assert_eq!(children.len(), 2);
    assert!(children[0] < children[1]);

    assert_eq!(sim.step(), 2);
    assert_eq!(
        observations.borrow().as_slice(),
        [
            WorkerObservation::Booted(children[0]),
            WorkerObservation::Booted(children[1]),
        ]
    );
}

#[test]
fn supervisor_policy_variants_select_the_same_children_as_the_live_runtime() {
    let (trace, observations) = run_restarted_parent(RestartPolicy::OneForOne, 0);
    assert_eq!(completed_restarts(&trace).len(), 1);
    assert_eq!(
        observations
            .iter()
            .filter(|event| matches!(event, WorkerObservation::Booted(_)))
            .count(),
        3
    );

    let (trace, _) = run_restarted_parent(RestartPolicy::OneForAll, 0);
    assert_eq!(completed_restarts(&trace).len(), 2);

    let (trace, _) = run_restarted_parent(RestartPolicy::RestForOne, 0);
    assert_eq!(completed_restarts(&trace).len(), 2);

    let (trace, _) = run_restarted_parent(RestartPolicy::RestForOne, 1);
    assert_eq!(completed_restarts(&trace).len(), 1);
}

#[test]
fn restartable_bootstrap_is_redelivered_and_repeated_restarts_replay() {
    fn run() -> (Vec<RuntimeEvent>, Vec<WorkerObservation>) {
        let observations = Rc::new(RefCell::new(Vec::new()));
        let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
        let parent = sim.register(RestartableParent {
            observations: Rc::clone(&observations),
        });
        sim.supervise(
            parent,
            SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(3)),
        );
        sim.try_send(parent, ParentMsg::SpawnOne).unwrap();
        sim.run_until_quiescent();

        let first = spawned_children(sim.trace())[0];
        sim.try_send(address_for_worker(first), WorkerMsg::Poison)
            .unwrap();
        let mut checker = StrictlyIncreasingRestartChecker::default();
        assert!(sim.run_until_quiescent_checked(&mut checker).is_none());

        let first_replacement = completed_restarts(sim.trace())[0].1;
        sim.try_send(address_for_worker(first_replacement), WorkerMsg::Poison)
            .unwrap();
        let mut checker = StrictlyIncreasingRestartChecker::default();
        assert!(sim.run_until_quiescent_checked(&mut checker).is_none());

        (sim.trace().to_vec(), observations.borrow().clone())
    }

    let (trace, observations) = run();
    let (replayed_trace, replayed_observations) = run();
    assert_eq!(trace, replayed_trace);
    assert_eq!(observations, replayed_observations);
    assert_eq!(completed_restarts(&trace).len(), 2);
    assert_eq!(
        observations
            .iter()
            .filter(|event| matches!(event, WorkerObservation::Booted(_)))
            .count(),
        3
    );
}

#[test]
fn restartable_bootstrap_factory_runs_fresh_for_each_replacement() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let next_bootstrap = Rc::new(Cell::new(1));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let parent = sim.register(DynamicBootstrapParent {
        observations: Rc::clone(&observations),
        next_bootstrap,
    });
    sim.supervise(
        parent,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(2)),
    );
    sim.try_send(parent, ParentMsg::SpawnOne).unwrap();
    sim.run_until_quiescent();

    let first = spawned_children(sim.trace())[0];
    sim.try_send(address_for_worker(first), WorkerMsg::Poison)
        .unwrap();
    sim.run_until_quiescent();
    let replacement = completed_restarts(sim.trace())[0].1;

    assert_eq!(
        observations.borrow().as_slice(),
        [
            WorkerObservation::Worked(first, 1),
            WorkerObservation::Worked(replacement, 2),
        ]
    );
}

#[test]
fn restart_workload_composes_with_seeded_local_send_delay() {
    let (trace, observations) = run_restart_then_delayed_send();
    let (replayed_trace, replayed_observations) = run_restart_then_delayed_send();
    let replacement = completed_restarts(&trace)[0].1;

    assert_eq!(trace, replayed_trace);
    assert_eq!(observations, replayed_observations);
    assert!(observations.contains(&WorkerObservation::Worked(replacement, 99)));
}

#[test]
fn restart_sensitive_checker_failure_is_replayable() {
    let artifact = run_restart_checker_failure();
    let replayed = run_restart_checker_failure();

    assert_eq!(artifact.event_record(), replayed.event_record());
    assert_eq!(artifact.final_time(), replayed.final_time());
    assert_eq!(artifact.checker_failure(), replayed.checker_failure());
    assert!(artifact.checker_failure().is_some());
}

#[test]
fn restart_budget_exhaustion_is_visible_and_reproducible() {
    fn run() -> Vec<RuntimeEvent> {
        let observations = Rc::new(RefCell::new(Vec::new()));
        let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
        let parent = sim.register(RestartableParent { observations });
        sim.supervise(
            parent,
            SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(1)),
        );
        sim.try_send(parent, ParentMsg::SpawnOne).unwrap();
        sim.run_until_quiescent();

        let first = spawned_children(sim.trace())[0];
        sim.try_send(address_for_worker(first), WorkerMsg::Poison)
            .unwrap();
        sim.run_until_quiescent();

        let replacement = completed_restarts(sim.trace())[0].1;
        sim.try_send(address_for_worker(replacement), WorkerMsg::Poison)
            .unwrap();
        sim.run_until_quiescent();
        sim.trace().to_vec()
    }

    let trace = run();
    let replayed = run();
    assert_eq!(trace, replayed);
    assert_eq!(completed_restarts(&trace).len(), 1);
    assert!(trace.iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SupervisorRestartRejected {
                reason: SupervisionRejectedReason::BudgetExceeded {
                    attempted_restart: 2,
                    max_restarts: 1,
                },
                ..
            }
        )
    }));
}

#[test]
fn non_restartable_child_is_skipped_visibly() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let parent = sim.register(OneShotParent { observations });
    sim.try_send(parent, ParentMsg::SpawnOne).unwrap();
    sim.run_until_quiescent();

    sim.try_send(parent, ParentMsg::RestartChildren).unwrap();
    sim.run_until_quiescent();

    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::RestartChildSkipped {
                reason: RestartSkippedReason::NotRestartable,
                ..
            }
        )
    }));
}

#[test]
fn stale_pre_restart_identity_fails_through_send_rejected_closed_event() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let parent = sim.register(RestartableParent { observations });
    sim.try_send(parent, ParentMsg::SpawnOne).unwrap();
    sim.run_until_quiescent();

    let stale_child = spawned_children(sim.trace())[0];
    sim.try_send(parent, ParentMsg::RestartChildren).unwrap();
    sim.run_until_quiescent();

    let sender = sim.register(StaleSender {
        target: address_for_worker(stale_child),
    });
    sim.try_send(sender, SenderMsg::SendToStale).unwrap();
    sim.run_until_quiescent();

    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                target_isolate,
                reason: SendRejectedReason::Closed,
                ..
            } if target_isolate == stale_child
        )
    }));
}

#[test]
fn restart_children_keeps_direct_child_scope() {
    let grandchild_boots = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let parent = sim.register(NestedParent {
        grandchild_boots: Rc::clone(&grandchild_boots),
    });
    sim.try_send(parent, ParentMsg::SpawnOne).unwrap();
    sim.run_until_quiescent();

    let attempts_before = sim
        .trace()
        .iter()
        .filter(|event| matches!(event.kind(), RuntimeEventKind::RestartChildAttempted { .. }))
        .count();
    sim.try_send(parent, ParentMsg::RestartChildren).unwrap();
    sim.run_until_quiescent();
    let attempts_after = sim
        .trace()
        .iter()
        .filter(|event| matches!(event.kind(), RuntimeEventKind::RestartChildAttempted { .. }))
        .count();

    assert_eq!(attempts_after - attempts_before, 1);
    assert_eq!(grandchild_boots.borrow().len(), 2);
}
