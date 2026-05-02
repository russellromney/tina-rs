use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tina::{Address, RestartBudget, RestartPolicy, prelude::*};
use tina_runtime::{
    CallKind, RuntimeCall, RuntimeEvent, RuntimeEventKind, SendRejectedReason, sleep,
};
use tina_sim::{
    Checker, CheckerDecision, FaultConfig, FaultMode, ReplayArtifact, Simulator, SimulatorConfig,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Default)]
struct HarnessShard;

impl Shard for HarnessShard {
    fn id(&self) -> ShardId {
        ShardId::new(123)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observation {
    Booted,
    TimerElapsed,
    Worked(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentMsg {
    Start,
    ChildBooted(Address<ChildMsg>),
    RetryElapsed,
    ChildWorked(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildMsg {
    Boot,
    PanicOnce,
    Work,
}

#[derive(Debug)]
struct Parent {
    observations: Rc<RefCell<Vec<Observation>>>,
    child: Option<Address<ChildMsg>>,
}

impl Isolate for Parent {
    tina::isolate_types! {
        message: ParentMsg,
        reply: (),
        send: Outbound<ChildMsg>,
        spawn: RestartableChildDefinition<Child>,
        call: RuntimeCall<ParentMsg>,
        shard: HarnessShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ParentMsg::Start => spawn(
                RestartableChildDefinition::new(|| Child { parent: None }, 4)
                    .with_initial_message(|| ChildMsg::Boot),
            ),
            ParentMsg::ChildBooted(child) => {
                self.observations.borrow_mut().push(Observation::Booted);
                self.child = Some(child);
                if self.observations.borrow().len() == 1 {
                    send(child, ChildMsg::PanicOnce)
                } else {
                    sleep(Duration::from_millis(5)).reply(|_| ParentMsg::RetryElapsed)
                }
            }
            ParentMsg::RetryElapsed => {
                self.observations
                    .borrow_mut()
                    .push(Observation::TimerElapsed);
                send(
                    self.child.expect("child booted before retry"),
                    ChildMsg::Work,
                )
            }
            ParentMsg::ChildWorked(value) => {
                self.observations
                    .borrow_mut()
                    .push(Observation::Worked(value));
                stop()
            }
        }
    }
}

#[derive(Debug)]
struct Child {
    parent: Option<Address<ParentMsg>>,
}

impl Isolate for Child {
    tina::isolate_types! {
        message: ChildMsg,
        reply: (),
        send: Outbound<ParentMsg>,
        spawn: std::convert::Infallible,
        call: RuntimeCall<ChildMsg>,
        shard: HarnessShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ChildMsg::Boot => {
                let parent = self
                    .parent
                    .unwrap_or_else(|| ctx.local_address(IsolateId::new(1)));
                self.parent = Some(parent);
                send(parent, ParentMsg::ChildBooted(ctx.me()))
            }
            ChildMsg::PanicOnce => panic!("huygens harness deliberate child panic"),
            ChildMsg::Work => send(
                self.parent.expect("parent captured during boot"),
                ParentMsg::ChildWorked(42),
            ),
        }
    }
}

struct DstHarness {
    config: SimulatorConfig,
}

impl DstHarness {
    fn new(config: SimulatorConfig) -> Self {
        Self { config }
    }

    fn run_supervised_timer(self) -> (Vec<Observation>, ReplayArtifact) {
        let observations = Rc::new(RefCell::new(Vec::new()));
        let mut sim = self.build_simulator(Rc::clone(&observations));
        sim.run_until_quiescent();
        (observations.borrow().clone(), sim.replay_artifact())
    }

    fn run_supervised_timer_checked<C: Checker>(
        self,
        checker: &mut C,
    ) -> (Vec<Observation>, ReplayArtifact) {
        let observations = Rc::new(RefCell::new(Vec::new()));
        let mut sim = self.build_simulator(Rc::clone(&observations));
        sim.run_until_quiescent_checked(checker);
        (observations.borrow().clone(), sim.replay_artifact())
    }

    fn build_simulator(
        self,
        observations: Rc<RefCell<Vec<Observation>>>,
    ) -> Simulator<HarnessShard> {
        let mut sim = Simulator::new(HarnessShard, self.config);
        let parent = sim.register_with_mailbox_capacity::<Parent, _, _>(
            Parent {
                observations,
                child: None,
            },
            8,
        );
        sim.supervise(
            parent,
            SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(2)),
        );
        sim.try_send(parent, ParentMsg::Start).unwrap();
        sim
    }
}

struct FailOnRestartChecker;

impl Checker for FailOnRestartChecker {
    fn name(&self) -> &'static str {
        "fail-on-restart"
    }

    fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision {
        if matches!(
            event.kind(),
            RuntimeEventKind::SupervisorRestartTriggered { .. }
        ) {
            CheckerDecision::Fail("supervised restart observed".to_owned())
        } else {
            CheckerDecision::Continue
        }
    }
}

fn count_event(trace: &[RuntimeEvent], predicate: impl Fn(&RuntimeEventKind) -> bool) -> usize {
    trace
        .iter()
        .filter(|event| predicate(&event.kind()))
        .count()
}

#[test]
fn dst_harness_replays_supervision_timer_and_local_send_composition() {
    let config = SimulatorConfig {
        seed: 19,
        faults: FaultConfig {
            local_send: tina_sim::LocalSendFaultMode::DelayByRounds {
                one_in: 2,
                rounds: 1,
            },
            timer_wake: FaultMode::DelayBy {
                one_in: 2,
                by: Duration::from_millis(3),
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let (observations, artifact) = DstHarness::new(config.clone()).run_supervised_timer();
    let (replayed_observations, replayed) = DstHarness::new(config).run_supervised_timer();

    assert_eq!(
        observations,
        vec![
            Observation::Booted,
            Observation::Booted,
            Observation::TimerElapsed,
            Observation::Worked(42),
        ]
    );
    assert_eq!(observations, replayed_observations);
    assert_eq!(artifact.event_record(), replayed.event_record());
    assert_eq!(artifact.final_time(), replayed.final_time());
    assert_eq!(
        count_event(artifact.event_record(), |kind| matches!(
            kind,
            RuntimeEventKind::SupervisorRestartTriggered { .. }
        )),
        1
    );
    assert_eq!(
        count_event(artifact.event_record(), |kind| matches!(
            kind,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::Sleep,
                ..
            }
        )),
        1
    );
}

#[test]
fn dst_harness_records_replayable_checker_failure_on_composed_restart() {
    let config = SimulatorConfig {
        seed: 21,
        ..Default::default()
    };

    let mut checker = FailOnRestartChecker;
    let (_, artifact) = DstHarness::new(config.clone()).run_supervised_timer_checked(&mut checker);

    let failure = artifact
        .checker_failure()
        .expect("checker should halt on supervised restart");
    assert_eq!(failure.checker_name(), "fail-on-restart");
    assert_eq!(failure.reason(), "supervised restart observed");

    let mut replay_checker = FailOnRestartChecker;
    let (_, replayed) = DstHarness::new(config).run_supervised_timer_checked(&mut replay_checker);
    assert_eq!(artifact.event_record(), replayed.event_record());
    assert_eq!(artifact.checker_failure(), replayed.checker_failure());
}

#[test]
fn dst_harness_keeps_remote_full_pressure_visible_on_oracle() {
    use std::convert::Infallible;

    use tina_sim::{MultiShardSimulator, MultiShardSimulatorConfig};

    #[derive(Debug, Clone, Copy)]
    struct RemoteShard(u32);

    impl Shard for RemoteShard {
        fn id(&self) -> ShardId {
            ShardId::new(self.0)
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum SenderMsg {
        Burst,
    }

    #[derive(Debug, Clone, Copy)]
    enum SinkMsg {
        Hit,
    }

    #[derive(Debug)]
    struct BurstSender {
        sink: Address<SinkMsg>,
    }

    impl Isolate for BurstSender {
        tina::isolate_types! {
            message: SenderMsg,
            reply: (),
            send: Outbound<SinkMsg>,
            spawn: Infallible,
            call: RuntimeCall<SenderMsg>,
            shard: RemoteShard,
        }

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            match msg {
                SenderMsg::Burst => {
                    batch([send(self.sink, SinkMsg::Hit), send(self.sink, SinkMsg::Hit)])
                }
            }
        }
    }

    #[derive(Debug)]
    struct Sink;

    impl Isolate for Sink {
        tina::isolate_types! {
            message: SinkMsg,
            reply: (),
            send: Outbound<Infallible>,
            spawn: Infallible,
            call: RuntimeCall<SinkMsg>,
            shard: RemoteShard,
        }

        fn handle(
            &mut self,
            _msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            noop()
        }
    }

    let mut sim = MultiShardSimulator::with_config(
        [RemoteShard(1), RemoteShard(2)],
        SimulatorConfig::default(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 1,
        },
    );
    let sink = sim.register_with_capacity_on::<Sink, _, _>(ShardId::new(2), Sink, 8);
    let sender = sim.register_with_capacity_on::<BurstSender, _, _>(
        ShardId::new(1),
        BurstSender { sink },
        8,
    );

    sim.try_send(sender, SenderMsg::Burst).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        count_event(&sim.trace(), |kind| matches!(
            kind,
            RuntimeEventKind::SendRejected {
                reason: SendRejectedReason::Full,
                ..
            }
        )),
        1
    );
}
