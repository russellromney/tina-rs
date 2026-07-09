//! Seeded scheduler perturbation: the run seed drives the within-round
//! isolate dispatch order, not just the fault-timing knobs.
//!
//! Determinism is the product guarantee, so these tests pin three things:
//! same seed replays byte-for-byte, different seeds diverge, and the
//! default (`SchedulerFaultMode::None`) is exactly registration order so
//! every pinned golden trace is untouched.

use std::convert::Infallible;

use tina::{Address, Context, Effect, Isolate, Outbound, Shard, ShardId, noop};
use tina_runtime::{RuntimeCall, RuntimeEvent, RuntimeEventKind, stable_trace_hash};
use tina_sim::{FaultConfig, SchedulerFaultMode, Simulator, SimulatorConfig};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(7)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Msg {
    Tick,
}

/// Minimal isolate: one message, one `Noop` effect. The trace records a
/// distinct `HandlerStarted` per incarnation, so the order they run in a
/// round is directly visible in the event stream.
#[derive(Debug)]
struct Recorder;

impl Isolate for Recorder {
    type Message = Msg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<Msg>;
    type Fact = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Msg,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            Msg::Tick => noop(),
        }
    }
}

fn config(seed: u64, mode: SchedulerFaultMode) -> SimulatorConfig {
    SimulatorConfig {
        seed,
        faults: FaultConfig {
            scheduler: mode,
            ..FaultConfig::default()
        },
        ..SimulatorConfig::default()
    }
}

/// Registers `count` recorders, sends each a `Tick`, then drains. All
/// recorders are ready in the same first round, so the round dispatch
/// order fully determines the `HandlerStarted` isolate order.
fn run(seed: u64, mode: SchedulerFaultMode, count: u64) -> Vec<RuntimeEvent> {
    let mut sim = Simulator::new(TestShard, config(seed, mode));
    let mut addrs: Vec<Address<Msg>> = Vec::new();
    for _ in 0..count {
        addrs.push(sim.register(Recorder));
    }
    for addr in &addrs {
        sim.try_send(*addr, Msg::Tick).unwrap();
    }
    sim.run_until_quiescent();
    sim.trace().to_vec()
}

/// Order in which isolates ran their handler this run.
fn handler_isolate_order(trace: &[RuntimeEvent]) -> Vec<u64> {
    trace
        .iter()
        .filter(|e| matches!(e.kind(), RuntimeEventKind::HandlerStarted))
        .map(|e| e.isolate().get())
        .collect()
}

#[test]
fn same_seed_replays_byte_for_byte_under_perturbation() {
    let first = run(0xC0FFEE, SchedulerFaultMode::PermuteReadyOrder, 8);
    let second = run(0xC0FFEE, SchedulerFaultMode::PermuteReadyOrder, 8);
    assert_eq!(
        stable_trace_hash(first.iter()),
        stable_trace_hash(second.iter()),
        "same seed + PermuteReadyOrder must replay byte-for-byte",
    );
    assert_eq!(
        handler_isolate_order(&first),
        handler_isolate_order(&second)
    );
}

#[test]
fn different_seeds_diverge_under_perturbation() {
    let a = run(0xC0FFEE, SchedulerFaultMode::PermuteReadyOrder, 8);
    let b = run(0xBADF00D, SchedulerFaultMode::PermuteReadyOrder, 8);
    assert_ne!(
        stable_trace_hash(a.iter()),
        stable_trace_hash(b.iter()),
        "different seeds must drive different interleavings",
    );
    assert_ne!(
        handler_isolate_order(&a),
        handler_isolate_order(&b),
        "the divergence must be in the dispatch order itself, not only ids",
    );
}

#[test]
fn perturbation_actually_reorders_from_registration_order() {
    // The whole point: a non-zero seed under PermuteReadyOrder must produce
    // a non-registration dispatch order for at least one seed. Registration
    // order for 8 isolates on this shard is ids 1..=8 ascending.
    let registration = run(0, SchedulerFaultMode::None, 8);
    let registration_order = handler_isolate_order(&registration);
    assert_eq!(registration_order, (1..=8).collect::<Vec<_>>());

    let reordered = (1..64u64).any(|seed| {
        handler_isolate_order(&run(seed, SchedulerFaultMode::PermuteReadyOrder, 8))
            != registration_order
    });
    assert!(
        reordered,
        "PermuteReadyOrder must reorder dispatch for some seed",
    );
}

#[test]
fn default_mode_is_exactly_registration_order_for_every_seed() {
    // Mode off must be byte-identical to the seed-0 registration-order run
    // regardless of seed. This is the guard that pinned golden traces
    // (which all run default faults) are untouched by this change.
    let baseline = run(0, SchedulerFaultMode::None, 8);
    let baseline_hash = stable_trace_hash(baseline.iter());
    for seed in [0u64, 1, 7, 0xC0FFEE, u64::MAX] {
        let off = run(seed, SchedulerFaultMode::None, 8);
        assert_eq!(
            stable_trace_hash(off.iter()),
            baseline_hash,
            "SchedulerFaultMode::None must ignore the seed's scheduler dimension",
        );
    }
}

#[test]
fn replay_artifact_reproduces_under_perturbation() {
    // Capture a replay artifact with perturbation on, then rerun the same
    // workload under the artifact's own config and require byte-identity.
    let cfg = config(0x51DE, SchedulerFaultMode::PermuteReadyOrder);

    let mut sim = Simulator::new(TestShard, cfg.clone());
    let mut addrs: Vec<Address<Msg>> = Vec::new();
    for _ in 0..8 {
        addrs.push(sim.register(Recorder));
    }
    for addr in &addrs {
        sim.try_send(*addr, Msg::Tick).unwrap();
    }
    sim.run_until_quiescent();
    let artifact = sim.replay_artifact();

    let mut replay = Simulator::new(TestShard, artifact.config().clone());
    let mut replay_addrs: Vec<Address<Msg>> = Vec::new();
    for _ in 0..8 {
        replay_addrs.push(replay.register(Recorder));
    }
    for addr in &replay_addrs {
        replay.try_send(*addr, Msg::Tick).unwrap();
    }
    replay.run_until_quiescent();

    assert_eq!(
        artifact.event_record(),
        replay.replay_artifact().event_record(),
        "a PermuteReadyOrder run must replay byte-for-byte from its config",
    );
}
