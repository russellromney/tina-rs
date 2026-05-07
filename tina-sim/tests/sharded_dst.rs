//! Phase 053 DST: sharded service primitives under seeded reorder.
//!
//! Goals (narrow, first-form):
//!
//! - byte-identical placement live vs sim: `ShardPlacement::owner_for_str`
//!   produces the same shard set on both runtimes, frozen by hashes here so
//!   future scheme/version bumps surface;
//! - reorder fanout replies: under a non-default `LocalSendFaultMode`
//!   delay, a sharded-counter workload still produces the placement-derived
//!   per-shard totals (placement is reorder-invariant under seeded delivery
//!   perturbation);
//! - saved seed for partial aggregate: the captured replay artifact carries
//!   the simulator config so a failed run can be re-run later.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{Address, prelude::*};
use tina_runtime::RuntimeCall;
use tina_runtime::sharded::{
    ReplyAdapter, ScatterGatherConfig, ScatterGatherReport, ScatterGatherTargetOutcome,
    ShardPlacement, ShardServiceTable, WrongShard,
};
use tina_runtime::sleep_then;
use tina_sim::{
    FaultConfig, LocalSendFaultMode, MultiShardSimulator, MultiShardSimulatorConfig,
    SimulatorConfig,
};

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone)]
enum CounterMsg {
    Inc {
        key: String,
        reply_to: Address<TrackerMsg>,
    },
    Get {
        reply_to: Address<TrackerMsg>,
    },
}

#[derive(Debug, Clone)]
enum TrackerMsg {
    IncResult {
        shard: ShardId,
        outcome: Result<(), WrongShard>,
    },
    GetResult {
        shard: ShardId,
        value: u64,
    },
}

struct ShardedCounter {
    placement: ShardPlacement,
    value: u64,
}

impl Isolate for ShardedCounter {
    tina::isolate_types! {
        message: CounterMsg,
        reply: (),
        send: Outbound<TrackerMsg>,
        spawn: Infallible,
        call: RuntimeCall<CounterMsg>,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CounterMsg::Inc { key, reply_to } => {
                let owner = self.placement.owner_for_str(&key);
                if owner != ctx.shard_id() {
                    return send(
                        reply_to,
                        TrackerMsg::IncResult {
                            shard: ctx.shard_id(),
                            outcome: Err(WrongShard {
                                expected: owner,
                                actual: ctx.shard_id(),
                            }),
                        },
                    );
                }
                self.value += 1;
                send(
                    reply_to,
                    TrackerMsg::IncResult {
                        shard: ctx.shard_id(),
                        outcome: Ok(()),
                    },
                )
            }
            CounterMsg::Get { reply_to } => send(
                reply_to,
                TrackerMsg::GetResult {
                    shard: ctx.shard_id(),
                    value: self.value,
                },
            ),
        }
    }
}

#[derive(Debug, Default)]
struct Tracker {
    inc_oks: Rc<RefCell<Vec<ShardId>>>,
    inc_wrong: Rc<RefCell<Vec<WrongShard>>>,
    gets: Rc<RefCell<Vec<(ShardId, u64)>>>,
}

impl Isolate for Tracker {
    tina::isolate_types! {
        message: TrackerMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<TrackerMsg>,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TrackerMsg::IncResult { shard, outcome } => match outcome {
                Ok(()) => self.inc_oks.borrow_mut().push(shard),
                Err(w) => self.inc_wrong.borrow_mut().push(w),
            },
            TrackerMsg::GetResult { shard, value } => {
                self.gets.borrow_mut().push((shard, value));
            }
        }
        noop()
    }
}

fn sim_with(config: SimulatorConfig) -> MultiShardSimulator<AppShard> {
    MultiShardSimulator::with_config(
        [AppShard(11), AppShard(22), AppShard(33)],
        config,
        MultiShardSimulatorConfig {
            shard_pair_capacity: 8,
        },
    )
}

const SHARD_LIST: [u32; 3] = [11, 22, 33];
const KEYS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    "lambda", "mu",
];

fn placement() -> ShardPlacement {
    ShardPlacement::new(
        "dst-fanout",
        SHARD_LIST.iter().copied().map(ShardId::new).collect(),
    )
    .unwrap()
}

struct Run {
    gets: Vec<(ShardId, u64)>,
    inc_oks: Vec<ShardId>,
    inc_wrong: Vec<WrongShard>,
    trace: Vec<tina_runtime::RuntimeEvent>,
}

fn run_increments(config: SimulatorConfig) -> Run {
    let mut sim = sim_with(config);
    let placement = placement();

    let tracker_state = Tracker::default();
    let inc_oks = Rc::clone(&tracker_state.inc_oks);
    let inc_wrong = Rc::clone(&tracker_state.inc_wrong);
    let gets = Rc::clone(&tracker_state.gets);
    let tracker = sim.register_with_capacity_on::<Tracker, TrackerMsg, Infallible>(
        ShardId::new(11),
        tracker_state,
        128,
    );

    let mut entries = Vec::new();
    for shard in placement.shards() {
        let address = sim.register_with_capacity_on::<ShardedCounter, CounterMsg, TrackerMsg>(
            *shard,
            ShardedCounter {
                placement: placement.clone(),
                value: 0,
            },
            32,
        );
        entries.push((*shard, address));
    }
    let table = ShardServiceTable::new(placement.clone(), entries).unwrap();

    for key in KEYS {
        sim.try_send(
            table.address_for_str(key),
            CounterMsg::Inc {
                key: (*key).to_string(),
                reply_to: tracker,
            },
        )
        .unwrap();
    }
    sim.run_until_quiescent();

    for address in table.addresses() {
        sim.try_send(*address, CounterMsg::Get { reply_to: tracker })
            .unwrap();
    }
    sim.run_until_quiescent();

    let mut snapshot: Vec<(ShardId, u64)> = gets.borrow().clone();
    snapshot.sort_by_key(|(s, _)| s.get());
    Run {
        gets: snapshot,
        inc_oks: inc_oks.borrow().clone(),
        inc_wrong: inc_wrong.borrow().clone(),
        trace: sim.trace(),
    }
}

#[test]
fn placement_owner_for_str_is_byte_identical_across_runs() {
    // Frozen — must match `sharded::tests::FROZEN_DST_*` in
    // `tina-runtime/src/sharded.rs` and `FROZEN_OWNERS` in
    // `tina-runtime/tests/sharded_primitives.rs`. The runtime crate's unit
    // test is the canonical source of truth; this duplicate is a
    // belt-and-suspenders check that the sim crate sees the same mapping.
    const FROZEN_OWNERS: [u32; 12] = [11, 33, 33, 22, 33, 33, 11, 11, 11, 33, 33, 11];

    let p = placement();
    let owners: Vec<u32> = KEYS.iter().map(|k| p.owner_for_str(k).get()).collect();
    assert_eq!(owners, FROZEN_OWNERS.to_vec());

    let report = p.report();
    assert_eq!(
        report.scheme(),
        tina_runtime::sharded::ShardHashScheme::Fnv1a64ModCount
    );
    assert_eq!(
        report.version(),
        tina_runtime::sharded::ShardHashScheme::VERSION
    );

    // Two `ShardPlacement` values with the same shard list must produce
    // the same owners — placement carries no hidden state.
    let p2 = placement();
    let owners2: Vec<u32> = KEYS.iter().map(|k| p2.owner_for_str(k).get()).collect();
    assert_eq!(owners, owners2);
}

#[test]
fn sharded_counter_totals_are_reorder_invariant_under_seeded_delivery_delay() {
    let placement = placement();

    // Expected per-shard counts, computed straight from placement.
    let mut expected: BTreeMap<ShardId, u64> = BTreeMap::new();
    for k in KEYS {
        *expected.entry(placement.owner_for_str(k)).or_default() += 1;
    }

    let baseline = run_increments(SimulatorConfig {
        seed: 0,
        ..SimulatorConfig::default()
    });
    assert!(baseline.inc_wrong.is_empty());
    assert_eq!(baseline.inc_oks.len(), KEYS.len());
    let baseline_map: BTreeMap<ShardId, u64> = baseline.gets.iter().copied().collect();
    assert_eq!(baseline_map, expected);

    // Reordered config: delay one in three local sends by two delivery
    // rounds. Replies arrive in a different order; per-shard totals
    // should not move because placement is reorder-invariant.
    let reordered = run_increments(SimulatorConfig {
        seed: 7,
        faults: FaultConfig {
            local_send: LocalSendFaultMode::DelayByRounds {
                one_in: 1,
                rounds: 1,
            },
            ..FaultConfig::default()
        },
        ..SimulatorConfig::default()
    });
    assert!(reordered.inc_wrong.is_empty());
    assert_eq!(reordered.inc_oks.len(), KEYS.len());
    let reordered_map: BTreeMap<ShardId, u64> = reordered.gets.iter().copied().collect();
    assert_eq!(reordered_map, expected);
    assert_eq!(reordered_map, baseline_map);

    // Strong claim: the seeded perturbation actually moved event order.
    // The two runs share the same workload but the delay fault must
    // produce a different event sequence (otherwise the "reorder
    // invariance" claim is vacuous — there is nothing to be invariant
    // against).
    //
    // Maintenance note: if a future runtime change makes
    // `LocalSendFaultMode::DelayByRounds { one_in: 1, rounds: 1 }` no
    // longer perturb this workload, this assertion will fail. Fix is to
    // pick a stronger perturbation (e.g., a different fault mode or
    // tuning that still applies to the workload), not to delete the
    // assert — the placement-invariance claim relies on actually
    // perturbing event order.
    assert_ne!(
        baseline.trace, reordered.trace,
        "seeded delay must produce a different event ordering"
    );
}

#[test]
fn replay_artifact_captures_simulator_config_for_partial_aggregate_seed_recovery() {
    let config = SimulatorConfig {
        seed: 42,
        faults: FaultConfig {
            local_send: LocalSendFaultMode::DelayByRounds {
                one_in: 5,
                rounds: 1,
            },
            ..FaultConfig::default()
        },
        ..SimulatorConfig::default()
    };

    let mut sim = sim_with(config.clone());
    let placement = placement();
    let tracker_state = Tracker::default();
    let tracker = sim.register_with_capacity_on::<Tracker, TrackerMsg, Infallible>(
        ShardId::new(11),
        tracker_state,
        64,
    );
    let mut entries = Vec::new();
    for shard in placement.shards() {
        let address = sim.register_with_capacity_on::<ShardedCounter, CounterMsg, TrackerMsg>(
            *shard,
            ShardedCounter {
                placement: placement.clone(),
                value: 0,
            },
            16,
        );
        entries.push((*shard, address));
    }
    let table = ShardServiceTable::new(placement.clone(), entries).unwrap();
    for key in KEYS {
        sim.try_send(
            table.address_for_str(key),
            CounterMsg::Inc {
                key: (*key).to_string(),
                reply_to: tracker,
            },
        )
        .unwrap();
    }
    sim.run_until_quiescent();

    let artifact = sim.replay_artifact();
    assert_eq!(artifact.simulator_config(), &config);
    assert!(artifact.final_time() >= Duration::ZERO);
}

// ---------- Aggregate-timeout DST ----------
//
// Sim-only: register a `QuietCounter` on one shard that absorbs `Get`
// without replying. The coord schedules `sleep_then(aggregate_timeout)`
// at fanout time. After two replies arrive from the live counters, the
// coord still has one pending target. We advance virtual time past the
// aggregate deadline; the timer fires; the coord marks the still-pending
// target as `AggregateTimeout` and emits the report.

/// Counter that ignores messages — simulates a stuck/slow shard for the
/// aggregate-timeout test.
struct QuietCounter;

impl Isolate for QuietCounter {
    tina::isolate_types! {
        message: CounterMsg,
        reply: (),
        send: Outbound<TrackerMsg>,
        spawn: Infallible,
        call: RuntimeCall<CounterMsg>,
        shard: AppShard,
    }

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        // Drop the message on the floor. No reply, no error — the
        // coord's aggregate-timeout timer is the only thing that fires.
        noop()
    }
}

#[derive(Debug, Clone)]
enum ScatterCoordMsg {
    Bind { bridge: Address<TrackerMsg> },
    Start,
    AggregateExpired,
    Reply(TrackerMsg),
}

// Wires `ReplyAdapter<TrackerMsg, ScatterCoordMsg, AppShard>`. The adapter
// translates the counter's reply type into the coord's wider message type.
impl From<TrackerMsg> for ScatterCoordMsg {
    fn from(msg: TrackerMsg) -> Self {
        ScatterCoordMsg::Reply(msg)
    }
}

struct ScatterCoord {
    table: ShardServiceTable<CounterMsg>,
    bridge: Option<Address<TrackerMsg>>,
    config: ScatterGatherConfig,
    targets: Vec<ShardId>,
    pending_replies: Vec<ShardId>,
    outcomes: Vec<(ShardId, ScatterGatherTargetOutcome<u64>)>,
    report_into: Rc<RefCell<Option<ScatterGatherReport<u64>>>>,
}

impl Isolate for ScatterCoord {
    tina::isolate_types! {
        message: ScatterCoordMsg,
        reply: (),
        send: Outbound<CounterMsg>,
        spawn: Infallible,
        call: RuntimeCall<ScatterCoordMsg>,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ScatterCoordMsg::Bind { bridge } => {
                self.bridge = Some(bridge);
                noop()
            }
            ScatterCoordMsg::Start => {
                let bridge = self.bridge.expect("bind before start");
                let mut effects: Vec<Effect<Self>> = Vec::new();
                let limit = self.config.max_targets.min(self.targets.len());
                for shard in self.targets.iter().copied().take(limit) {
                    if let Ok(address) = self.table.address_for(shard) {
                        self.pending_replies.push(shard);
                        effects.push(send(address, CounterMsg::Get { reply_to: bridge }));
                    } else {
                        self.outcomes
                            .push((shard, ScatterGatherTargetOutcome::MissingShard));
                    }
                }
                // Schedule the aggregate timeout as a self-message.
                effects.push(sleep_then(
                    self.config.aggregate_timeout,
                    ScatterCoordMsg::AggregateExpired,
                ));
                batch(effects)
            }
            ScatterCoordMsg::Reply(reply) => {
                if let TrackerMsg::GetResult { shard, value } = reply {
                    if let Some(pos) = self.pending_replies.iter().position(|s| *s == shard) {
                        self.pending_replies.swap_remove(pos);
                        self.outcomes
                            .push((shard, ScatterGatherTargetOutcome::Replied(value)));
                    }
                }
                self.maybe_emit_report();
                noop()
            }
            ScatterCoordMsg::AggregateExpired => {
                // Convert any still-pending targets to AggregateTimeout
                // and emit the report.
                let pending = std::mem::take(&mut self.pending_replies);
                for shard in pending {
                    self.outcomes
                        .push((shard, ScatterGatherTargetOutcome::AggregateTimeout));
                }
                self.maybe_emit_report();
                noop()
            }
        }
    }
}

impl ScatterCoord {
    fn maybe_emit_report(&mut self) {
        if !self.pending_replies.is_empty() {
            return;
        }
        if self.report_into.borrow().is_some() {
            return;
        }
        // Preserve caller-supplied target order in the report (the
        // public ordering contract).
        let mut outcomes = std::mem::take(&mut self.outcomes);
        outcomes.sort_by_key(|(s, _)| {
            self.targets
                .iter()
                .position(|t| *t == *s)
                .unwrap_or(usize::MAX)
        });
        *self.report_into.borrow_mut() = Some(ScatterGatherReport {
            config: self.config,
            outcomes,
        });
    }
}

// `ReplyBridge` was previously hand-written; it is now the shipped
// `ReplyAdapter<TrackerMsg, ScatterCoordMsg, AppShard>` primitive (see
// the `register_with_capacity_on::<ReplyAdapter<_, _, _>, _, _>(...)`
// call below).

#[test]
fn scatter_gather_records_aggregate_timeout_when_target_does_not_reply() {
    let mut sim = sim_with(SimulatorConfig::default());
    let placement = placement();

    // Place real counters on shards 11 and 33; place a Quiet counter on
    // shard 22 that absorbs Get without replying.
    let mut entries = Vec::new();
    for shard in placement.shards() {
        let shard = *shard;
        let address = if shard == ShardId::new(22) {
            sim.register_with_capacity_on::<QuietCounter, CounterMsg, TrackerMsg>(
                shard,
                QuietCounter,
                16,
            )
        } else {
            sim.register_with_capacity_on::<ShardedCounter, CounterMsg, TrackerMsg>(
                shard,
                ShardedCounter {
                    placement: placement.clone(),
                    value: 0,
                },
                16,
            )
        };
        entries.push((shard, address));
    }
    let table = ShardServiceTable::new(placement.clone(), entries).unwrap();

    let aggregate_timeout = Duration::from_millis(200);
    let config = ScatterGatherConfig {
        max_targets: placement.shards().len(),
        collector_capacity: placement.shards().len(),
        per_target_timeout: Duration::from_millis(50),
        aggregate_timeout,
    };
    config.validate().unwrap();

    let report_slot: Rc<RefCell<Option<ScatterGatherReport<u64>>>> = Rc::new(RefCell::new(None));
    let coord = sim.register_with_capacity_on::<ScatterCoord, ScatterCoordMsg, CounterMsg>(
        ShardId::new(11),
        ScatterCoord {
            table: table.clone(),
            bridge: None,
            config,
            targets: placement.shards().to_vec(),
            pending_replies: Vec::new(),
            outcomes: Vec::with_capacity(config.max_targets),
            report_into: Rc::clone(&report_slot),
        },
        16,
    );
    let bridge = sim.register_with_capacity_on::<
        ReplyAdapter<TrackerMsg, ScatterCoordMsg, AppShard>,
        TrackerMsg,
        ScatterCoordMsg,
    >(ShardId::new(11), ReplyAdapter::new(coord), 16);

    sim.try_send(coord, ScatterCoordMsg::Bind { bridge })
        .unwrap();
    sim.try_send(coord, ScatterCoordMsg::Start).unwrap();

    // `run_until_quiescent` auto-advances virtual time to the next due
    // timer when no other work is pending, so the aggregate-timeout
    // timer fires inside this call. The QuietCounter never replies; the
    // coord marks shard 22 as AggregateTimeout when the timer arrives.
    sim.run_until_quiescent();
    assert!(
        sim.now() >= aggregate_timeout,
        "virtual time must have advanced past aggregate_timeout, was {:?}",
        sim.now()
    );

    let report = report_slot
        .borrow_mut()
        .take()
        .expect("aggregate timer must fire and emit report");
    assert_eq!(report.replied_count(), 2, "report = {report:?}");
    assert_eq!(report.aggregate_timeout_count(), 1);
    assert_eq!(report.full_count(), 0);
    assert_eq!(report.closed_count(), 0);
    assert_eq!(report.timeout_count(), 0);
    assert_eq!(report.missing_shard_count(), 0);
    assert!(
        report.outcomes.iter().any(|(s, o)| *s == ShardId::new(22)
            && matches!(o, ScatterGatherTargetOutcome::AggregateTimeout)),
        "expected AggregateTimeout for shard 22, got {:?}",
        report.outcomes
    );
}
