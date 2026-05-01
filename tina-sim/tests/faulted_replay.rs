use std::convert::Infallible;
use tina::{Address, Context, Effect, Isolate, IsolateId, Outbound, Shard, ShardId};
use tina_runtime::{EventId, RuntimeCall, RuntimeEvent, RuntimeEventKind};
use tina_sim::{
    Checker, CheckerDecision, CheckerFailure, FaultConfig, LocalSendFaultMode, ReplayArtifact,
    Simulator, SimulatorConfig,
};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(61)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimMsg {
    Start,
    Prepare,
    ScheduleCheck,
    Ack,
    Check,
    Seen,
}

#[derive(Debug)]
struct Driver {
    protocol: Address<SimMsg>,
    trigger: Address<SimMsg>,
}

impl Isolate for Driver {
    type Message = SimMsg;
    type Reply = ();
    type Send = Outbound<SimMsg>;
    type Spawn = Infallible;
    type Call = RuntimeCall<SimMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            SimMsg::Start => Effect::Batch(vec![
                Effect::Send(Outbound::new(self.protocol, SimMsg::Prepare)),
                Effect::Send(Outbound::new(self.trigger, SimMsg::ScheduleCheck)),
            ]),
            _ => Effect::Noop,
        }
    }
}

#[derive(Debug)]
struct Protocol {
    watcher: Address<SimMsg>,
}

impl Isolate for Protocol {
    type Message = SimMsg;
    type Reply = ();
    type Send = Outbound<SimMsg>;
    type Spawn = Infallible;
    type Call = RuntimeCall<SimMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            SimMsg::Prepare => Effect::Send(Outbound::new(self.watcher, SimMsg::Ack)),
            _ => Effect::Noop,
        }
    }
}

#[derive(Debug)]
struct Trigger {
    watcher: Address<SimMsg>,
}

impl Isolate for Trigger {
    type Message = SimMsg;
    type Reply = ();
    type Send = Outbound<SimMsg>;
    type Spawn = Infallible;
    type Call = RuntimeCall<SimMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            SimMsg::ScheduleCheck => Effect::Send(Outbound::new(self.watcher, SimMsg::Check)),
            _ => Effect::Noop,
        }
    }
}

#[derive(Debug)]
struct Watcher {
    acked: bool,
    success: Address<SimMsg>,
    failure: Address<SimMsg>,
}

impl Isolate for Watcher {
    type Message = SimMsg;
    type Reply = ();
    type Send = Outbound<SimMsg>;
    type Spawn = Infallible;
    type Call = RuntimeCall<SimMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            SimMsg::Ack => {
                self.acked = true;
                Effect::Noop
            }
            SimMsg::Check => {
                let target = if self.acked {
                    self.success
                } else {
                    self.failure
                };
                Effect::Send(Outbound::new(target, SimMsg::Seen))
            }
            _ => Effect::Noop,
        }
    }
}

#[derive(Debug)]
struct Sink;

impl Isolate for Sink {
    type Message = SimMsg;
    type Reply = ();
    type Send = Outbound<SimMsg>;
    type Spawn = Infallible;
    type Call = RuntimeCall<SimMsg>;
    type Shard = TestShard;

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        Effect::Noop
    }
}

#[derive(Debug)]
struct FailureSinkChecker {
    failure_isolate: IsolateId,
}

impl Checker for FailureSinkChecker {
    fn name(&self) -> &'static str {
        "failure-sink-checker"
    }

    fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision {
        match event.kind() {
            RuntimeEventKind::SendAccepted { target_isolate, .. }
                if target_isolate == self.failure_isolate =>
            {
                CheckerDecision::Fail("watcher checked before acknowledgment arrived".into())
            }
            _ => CheckerDecision::Continue,
        }
    }
}

#[derive(Debug, Default)]
struct MonotonicEventIdChecker {
    last: Option<EventId>,
}

impl Checker for MonotonicEventIdChecker {
    fn name(&self) -> &'static str {
        "monotonic-event-id-checker"
    }

    fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision {
        if let Some(last) = self.last {
            if event.id().get() != last.get() + 1 {
                return CheckerDecision::Fail(format!(
                    "expected next event id {}, got {}",
                    last.get() + 1,
                    event.id().get()
                ));
            }
        }
        self.last = Some(event.id());
        CheckerDecision::Continue
    }
}

fn count_send_accepted(trace: &[RuntimeEvent], isolate: IsolateId) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendAccepted { target_isolate, .. } if target_isolate == isolate
            )
        })
        .count()
}

fn run_protocol_workload(config: SimulatorConfig) -> (ReplayArtifact, IsolateId, IsolateId) {
    let mut sim = Simulator::new(TestShard, config);
    let success = sim.register(Sink);
    let failure = sim.register(Sink);
    let watcher = sim.register(Watcher {
        acked: false,
        success,
        failure,
    });
    let protocol = sim.register(Protocol { watcher });
    let trigger = sim.register(Trigger { watcher });
    let driver = sim.register(Driver { protocol, trigger });
    sim.try_send(driver, SimMsg::Start).unwrap();
    sim.run_until_quiescent();
    (sim.replay_artifact(), success.isolate(), failure.isolate())
}

fn run_protocol_workload_checked(
    config: SimulatorConfig,
) -> (ReplayArtifact, Option<CheckerFailure>, IsolateId, IsolateId) {
    let mut sim = Simulator::new(TestShard, config);
    let success = sim.register(Sink);
    let failure = sim.register(Sink);
    let watcher = sim.register(Watcher {
        acked: false,
        success,
        failure,
    });
    let protocol = sim.register(Protocol { watcher });
    let trigger = sim.register(Trigger { watcher });
    let driver = sim.register(Driver { protocol, trigger });
    sim.try_send(driver, SimMsg::Start).unwrap();
    let mut checker = FailureSinkChecker {
        failure_isolate: failure.isolate(),
    };
    let checker_failure = sim.run_until_quiescent_checked(&mut checker);
    (
        sim.replay_artifact(),
        checker_failure,
        success.isolate(),
        failure.isolate(),
    )
}

#[test]
fn fault_disabled_protocol_preserves_success_path() {
    let (artifact, success, failure) = run_protocol_workload(SimulatorConfig {
        seed: 13,
        ..Default::default()
    });

    assert_eq!(count_send_accepted(artifact.event_record(), success), 1);
    assert_eq!(count_send_accepted(artifact.event_record(), failure), 0);
}

#[test]
fn structural_checker_observes_monotonic_event_ids() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 21,
            ..Default::default()
        },
    );
    let success = sim.register(Sink);
    let failure = sim.register(Sink);
    let watcher = sim.register(Watcher {
        acked: false,
        success,
        failure,
    });
    let protocol = sim.register(Protocol { watcher });
    let trigger = sim.register(Trigger { watcher });
    let driver = sim.register(Driver { protocol, trigger });
    sim.try_send(driver, SimMsg::Start).unwrap();

    let mut checker = MonotonicEventIdChecker::default();
    assert!(sim.run_until_quiescent_checked(&mut checker).is_none());
}

#[test]
fn same_seed_local_send_failure_replays_same_checker_failure() {
    let config = SimulatorConfig {
        seed: 0,
        faults: FaultConfig {
            local_send: LocalSendFaultMode::DelayByRounds {
                one_in: 2,
                rounds: 1,
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let (artifact, failure_result, success, failure) = run_protocol_workload_checked(config);
    let checker_failure = failure_result.expect("expected seeded local-send fault to trip checker");
    assert_eq!(
        checker_failure.reason(),
        "watcher checked before acknowledgment arrived"
    );
    assert_eq!(count_send_accepted(artifact.event_record(), success), 0);
    assert_eq!(count_send_accepted(artifact.event_record(), failure), 1);
    assert_eq!(artifact.checker_failure(), Some(&checker_failure));

    let (replayed, replayed_failure, replayed_success, replayed_failure_sink) =
        run_protocol_workload_checked(artifact.config().clone());
    assert_eq!(artifact.event_record(), replayed.event_record());
    assert_eq!(artifact.final_time(), replayed.final_time());
    assert_eq!(artifact.checker_failure(), replayed.checker_failure());
    assert_eq!(success, replayed_success);
    assert_eq!(failure, replayed_failure_sink);
    assert_eq!(Some(checker_failure), replayed_failure);
}

#[test]
fn different_seeds_diverge_on_local_send_faults() {
    let fault = LocalSendFaultMode::DelayByRounds {
        one_in: 2,
        rounds: 1,
    };

    let delayed = SimulatorConfig {
        seed: 0,
        faults: FaultConfig {
            local_send: fault,
            ..Default::default()
        },
        ..Default::default()
    };
    let preserved = SimulatorConfig {
        seed: 1,
        faults: FaultConfig {
            local_send: fault,
            ..Default::default()
        },
        ..Default::default()
    };

    let (delayed_artifact, delayed_failure, _, delayed_failure_sink) =
        run_protocol_workload_checked(delayed);
    let (preserved_artifact, preserved_failure, preserved_success, _) =
        run_protocol_workload_checked(preserved);

    assert_ne!(
        delayed_artifact.event_record(),
        preserved_artifact.event_record()
    );
    assert!(delayed_failure.is_some());
    assert!(preserved_failure.is_none());
    assert_eq!(
        count_send_accepted(delayed_artifact.event_record(), delayed_failure_sink),
        1
    );
    assert_eq!(
        count_send_accepted(preserved_artifact.event_record(), preserved_success),
        1
    );
}
