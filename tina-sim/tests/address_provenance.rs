use std::convert::Infallible;

use tina::SystemIncarnation;
use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, HOST_CALL_DISPATCHER_POOL_SIZE, ThreadedRuntime,
    ThreadedRuntimeConfig, stable_trace_hash,
};
use tina_sim::{MultiShardSimulator, Simulator, SimulatorConfig};

#[derive(Debug, Clone, Copy)]
struct SimShard(u32);

impl Shard for SimShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug)]
enum Msg {
    Tick,
}

struct Probe;

#[tina_runtime::isolate(message = Msg, shard = SimShard)]
impl Probe {
    fn handle(
        &mut self,
        _message: Msg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

#[test]
fn default_simulators_reject_foreign_coincident_tuple() {
    let mut local = Simulator::new(SimShard(4), SimulatorConfig::default());
    let mut foreign = Simulator::new(SimShard(4), SimulatorConfig::default());
    let local_address = local.register::<Probe, Msg, Infallible>(Probe);
    let foreign_address = foreign.register::<Probe, Msg, Infallible>(Probe);

    assert_eq!(local_address.shard(), foreign_address.shard());
    assert_eq!(local_address.isolate(), foreign_address.isolate());
    assert_eq!(local_address.generation(), foreign_address.generation());
    assert_ne!(local_address.system(), foreign_address.system());
    assert!(matches!(
        local.try_send(foreign_address, Msg::Tick),
        Err(tina_runtime::IngressSendError::ForeignSystem {
            expected,
            actual,
            message: Msg::Tick,
        }) if expected == local_address.system() && actual == foreign_address.system()
    ));
    assert_eq!(local.trace().len(), 0);
}

fn deterministic_trace(system: SystemIncarnation) -> (SystemIncarnation, u64) {
    let mut sim = Simulator::new(
        SimShard(4),
        SimulatorConfig {
            system_incarnation: Some(system),
            ..SimulatorConfig::default()
        },
    );
    let address = sim.register::<Probe, Msg, Infallible>(Probe);
    sim.try_send(address, Msg::Tick)
        .expect("local send admitted");
    sim.run_until_quiescent();
    (address.system(), stable_trace_hash(sim.trace()))
}

#[test]
fn multi_shard_simulator_shares_one_provenance() {
    let mut sim = MultiShardSimulator::new([SimShard(1), SimShard(2)], SimulatorConfig::default());
    let first = sim.register_on::<Probe, Msg, Infallible>(ShardId::new(1), Probe);
    let second = sim.register_on::<Probe, Msg, Infallible>(ShardId::new(2), Probe);
    assert_eq!(first.system(), second.system());
}

#[test]
fn configured_live_and_simulated_owners_issue_matching_user_identity() {
    let system = SystemIncarnation::new(0x5151);
    let live = ThreadedRuntime::with_config(
        SimShard(4),
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            system_incarnation: Some(system),
            ..ThreadedRuntimeConfig::default()
        },
    );
    let live_address = live
        .register_with_capacity::<Probe, Infallible>(Probe, 4)
        .expect("live probe registered");
    let mut sim = Simulator::new(
        SimShard(4),
        SimulatorConfig {
            system_incarnation: Some(system),
            reserved_system_isolates: HOST_CALL_DISPATCHER_POOL_SIZE,
            ..SimulatorConfig::default()
        },
    );
    let sim_address = sim.register::<Probe, Msg, Infallible>(Probe);

    assert_eq!(live_address.system(), sim_address.system());
    assert_eq!(live_address.shard(), sim_address.shard());
    assert_eq!(live_address.isolate(), sim_address.isolate());
    assert_eq!(live_address.generation(), sim_address.generation());
    live.shutdown().expect("live runtime shuts down");
}

#[test]
fn explicitly_configured_provenance_preserves_replay_determinism() {
    let system = SystemIncarnation::new(0x1234);
    assert_eq!(deterministic_trace(system), deterministic_trace(system));
}

#[test]
fn simulators_reject_the_unscoped_system_marker() {
    let single = std::panic::catch_unwind(|| {
        Simulator::new(
            SimShard(1),
            SimulatorConfig {
                system_incarnation: Some(SystemIncarnation::DEFAULT),
                ..SimulatorConfig::default()
            },
        )
    });
    assert!(single.is_err());

    let multi = std::panic::catch_unwind(|| {
        MultiShardSimulator::new(
            [SimShard(1)],
            SimulatorConfig {
                system_incarnation: Some(SystemIncarnation::DEFAULT),
                ..SimulatorConfig::default()
            },
        )
    });
    assert!(multi.is_err());
}
