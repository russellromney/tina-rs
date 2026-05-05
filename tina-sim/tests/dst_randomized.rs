use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{Address, IsolateId, prelude::*};
use tina_runtime::{CallKind, RuntimeEventKind, SendRejectedReason, sleep};
use tina_sim::{
    FaultConfig, FaultMode, LocalSendFaultMode, MultiShardReplayArtifact, MultiShardSimulator,
    MultiShardSimulatorConfig, ReplayArtifact, Simulator, SimulatorConfig,
    dst::{DstRun, History, InvariantSuite, ShrinkConfig, assert_replays, delete_shrink},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DstShard;

impl Shard for DstShard {
    fn id(&self) -> ShardId {
        ShardId::new(370)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetMsg {
    Data(u8),
    Stop,
    Panic,
}

#[derive(Debug)]
struct Target {
    observed: Rc<RefCell<Vec<u8>>>,
}

#[tina_runtime::isolate(message = TargetMsg, shard = DstShard)]
impl Target {
    fn handle(&mut self, msg: TargetMsg, _ctx: &mut Context<'_, DstShard>) -> Effect<Self> {
        match msg {
            TargetMsg::Data(value) => {
                self.observed.borrow_mut().push(value);
                noop()
            }
            TargetMsg::Stop => stop(),
            TargetMsg::Panic => panic!("seeded DST target panic"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverMsg {
    Send(u8),
    Burst(u8),
    StopTarget,
    PanicTarget,
}

#[derive(Debug)]
struct Driver {
    target: Address<TargetMsg>,
}

#[tina_runtime::isolate(
    message = DriverMsg,
    send = Outbound<TargetMsg>,
    shard = DstShard
)]
impl Driver {
    fn handle(&mut self, msg: DriverMsg, _ctx: &mut Context<'_, DstShard>) -> Effect<Self> {
        match msg {
            DriverMsg::Send(value) => send(self.target, TargetMsg::Data(value)),
            DriverMsg::Burst(value) => batch([
                send(self.target, TargetMsg::Data(value)),
                send(self.target, TargetMsg::Data(value.wrapping_add(1))),
                send(self.target, TargetMsg::Data(value.wrapping_add(2))),
            ]),
            DriverMsg::StopTarget => send(self.target, TargetMsg::Stop),
            DriverMsg::PanicTarget => send(self.target, TargetMsg::Panic),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerMsg {
    After(u8),
    Elapsed(u8),
}

#[derive(Debug)]
struct TimerDriver {
    target: Address<TargetMsg>,
}

#[tina_runtime::isolate(
    message = TimerMsg,
    send = Outbound<TargetMsg>,
    shard = DstShard
)]
impl TimerDriver {
    fn handle(&mut self, msg: TimerMsg, _ctx: &mut Context<'_, DstShard>) -> Effect<Self> {
        match msg {
            TimerMsg::After(value) => sleep(Duration::from_millis(1 + u64::from(value % 5)))
                .reply(move |_| TimerMsg::Elapsed(value)),
            TimerMsg::Elapsed(value) => send(self.target, TargetMsg::Data(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RandomOp {
    DriverSend(u8),
    DriverBurst(u8),
    TimerAfter(u8),
    StopTarget,
    PanicTarget,
    Step,
    RunUntilIdle,
}

fn xorshift64(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

fn random_ops(seed: u64, len: usize) -> Vec<RandomOp> {
    let mut state = seed ^ 0x9e37_79b9_7f4a_7c15;
    let mut ops = Vec::with_capacity(len);
    for _ in 0..len {
        state = xorshift64(state);
        let value = (state >> 8) as u8;
        ops.push(match state % 10 {
            0..=2 => RandomOp::DriverSend(value),
            3 => RandomOp::DriverBurst(value),
            4 | 5 => RandomOp::TimerAfter(value),
            6 => RandomOp::StopTarget,
            7 => RandomOp::PanicTarget,
            8 => RandomOp::Step,
            _ => RandomOp::RunUntilIdle,
        });
    }
    ops
}

fn run_random_history(seed: u64, ops: &[RandomOp]) -> DstRun<Vec<u8>, ReplayArtifact> {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        DstShard,
        SimulatorConfig {
            seed,
            faults: FaultConfig {
                local_send: LocalSendFaultMode::DelayByRounds {
                    one_in: 3,
                    rounds: 2,
                },
                timer_wake: FaultMode::DelayBy {
                    one_in: 2,
                    by: Duration::from_millis(3),
                },
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let target = sim.register_with_mailbox_capacity::<Target, TargetMsg, Infallible>(
        Target {
            observed: Rc::clone(&observed),
        },
        3,
    );
    let driver =
        sim.register_with_mailbox_capacity::<Driver, DriverMsg, TargetMsg>(Driver { target }, 4);
    let timer = sim.register_with_mailbox_capacity::<TimerDriver, TimerMsg, TargetMsg>(
        TimerDriver { target },
        4,
    );

    for op in ops {
        match *op {
            RandomOp::DriverSend(value) => {
                let _ = sim.try_send(driver, DriverMsg::Send(value));
            }
            RandomOp::DriverBurst(value) => {
                let _ = sim.try_send(driver, DriverMsg::Burst(value));
            }
            RandomOp::TimerAfter(value) => {
                let _ = sim.try_send(timer, TimerMsg::After(value));
            }
            RandomOp::StopTarget => {
                let _ = sim.try_send(driver, DriverMsg::StopTarget);
            }
            RandomOp::PanicTarget => {
                let _ = sim.try_send(driver, DriverMsg::PanicTarget);
            }
            RandomOp::Step => {
                sim.step();
            }
            RandomOp::RunUntilIdle => {
                sim.run_until_quiescent();
            }
        }
    }
    sim.run_until_quiescent();

    DstRun::new(observed.borrow().clone(), sim.replay_artifact())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RandomShard(u32);

impl Shard for RandomShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteCoordinatorMsg {
    Send(u8),
    Burst(u8),
    StopWorker,
    BadRemote,
    Ack(u8),
}

#[derive(Debug)]
struct RemoteCoordinator {
    worker: Address<RemoteWorkerMsg>,
    bad_worker: Address<RemoteWorkerMsg>,
    observed: Rc<RefCell<Vec<u8>>>,
}

#[tina_runtime::isolate(
    message = RemoteCoordinatorMsg,
    send = Outbound<RemoteWorkerMsg>,
    shard = RandomShard
)]
impl RemoteCoordinator {
    fn handle(
        &mut self,
        msg: RemoteCoordinatorMsg,
        ctx: &mut Context<'_, RandomShard>,
    ) -> Effect<Self> {
        match msg {
            RemoteCoordinatorMsg::Send(value) => send(
                self.worker,
                RemoteWorkerMsg::Run {
                    value,
                    reply_to: ctx.me(),
                },
            ),
            RemoteCoordinatorMsg::Burst(value) => batch([
                send(
                    self.worker,
                    RemoteWorkerMsg::Run {
                        value,
                        reply_to: ctx.me(),
                    },
                ),
                send(
                    self.worker,
                    RemoteWorkerMsg::Run {
                        value: value.wrapping_add(1),
                        reply_to: ctx.me(),
                    },
                ),
                send(
                    self.worker,
                    RemoteWorkerMsg::Run {
                        value: value.wrapping_add(2),
                        reply_to: ctx.me(),
                    },
                ),
            ]),
            RemoteCoordinatorMsg::StopWorker => send(self.worker, RemoteWorkerMsg::Stop),
            RemoteCoordinatorMsg::BadRemote => send(
                self.bad_worker,
                RemoteWorkerMsg::Run {
                    value: 250,
                    reply_to: ctx.me(),
                },
            ),
            RemoteCoordinatorMsg::Ack(value) => {
                self.observed.borrow_mut().push(value);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteWorkerMsg {
    Run {
        value: u8,
        reply_to: Address<RemoteCoordinatorMsg>,
    },
    Stop,
}

#[derive(Debug)]
struct RemoteWorker;

#[tina_runtime::isolate(
    message = RemoteWorkerMsg,
    send = Outbound<RemoteCoordinatorMsg>,
    shard = RandomShard
)]
impl RemoteWorker {
    fn handle(
        &mut self,
        msg: RemoteWorkerMsg,
        _ctx: &mut Context<'_, RandomShard>,
    ) -> Effect<Self> {
        match msg {
            RemoteWorkerMsg::Run { value, reply_to } => {
                send(reply_to, RemoteCoordinatorMsg::Ack(value.wrapping_mul(2)))
            }
            RemoteWorkerMsg::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultiOp {
    Send(u8),
    Burst(u8),
    StopWorker,
    BadRemote,
    Step,
    RunUntilIdle,
}

fn random_multi_ops(seed: u64, len: usize) -> Vec<MultiOp> {
    let mut state = seed ^ 0xd1b5_4a32_d192_ed03;
    let mut ops = Vec::with_capacity(len);
    for _ in 0..len {
        state = xorshift64(state);
        let value = (state >> 11) as u8;
        ops.push(match state % 9 {
            0..=2 => MultiOp::Send(value),
            3 | 4 => MultiOp::Burst(value),
            5 => MultiOp::StopWorker,
            6 => MultiOp::BadRemote,
            7 => MultiOp::Step,
            _ => MultiOp::RunUntilIdle,
        });
    }
    ops
}

fn run_random_multishard_history(
    seed: u64,
    ops: &[MultiOp],
) -> DstRun<Vec<u8>, MultiShardReplayArtifact> {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = MultiShardSimulator::with_config(
        [RandomShard(371), RandomShard(372)],
        SimulatorConfig {
            seed,
            faults: FaultConfig {
                local_send: LocalSendFaultMode::DelayByRounds {
                    one_in: 2,
                    rounds: 1,
                },
                ..Default::default()
            },
            ..Default::default()
        },
        MultiShardSimulatorConfig {
            shard_pair_capacity: 2,
        },
    );
    let worker = sim
        .register_with_capacity_on::<RemoteWorker, RemoteWorkerMsg, RemoteCoordinatorMsg>(
            ShardId::new(372),
            RemoteWorker,
            2,
        );
    let coordinator = sim
        .register_with_capacity_on::<RemoteCoordinator, RemoteCoordinatorMsg, RemoteWorkerMsg>(
            ShardId::new(371),
            RemoteCoordinator {
                worker,
                bad_worker: Address::new(ShardId::new(372), IsolateId::new(999)),
                observed: Rc::clone(&observed),
            },
            8,
        );

    for op in ops {
        match *op {
            MultiOp::Send(value) => {
                let _ = sim.try_send(coordinator, RemoteCoordinatorMsg::Send(value));
            }
            MultiOp::Burst(value) => {
                let _ = sim.try_send(coordinator, RemoteCoordinatorMsg::Burst(value));
            }
            MultiOp::StopWorker => {
                let _ = sim.try_send(coordinator, RemoteCoordinatorMsg::StopWorker);
            }
            MultiOp::BadRemote => {
                let _ = sim.try_send(coordinator, RemoteCoordinatorMsg::BadRemote);
            }
            MultiOp::Step => {
                sim.step();
            }
            MultiOp::RunUntilIdle => {
                sim.run_until_quiescent();
            }
        }
    }
    sim.run_until_quiescent();

    DstRun::new(observed.borrow().clone(), sim.replay_artifact())
}

#[test]
fn seeded_random_single_shard_histories_replay_and_keep_trace_invariants() {
    let mut saw_closed = false;
    let mut saw_full = false;
    let mut saw_panic = false;
    let mut saw_timer = false;

    for seed in 0..32 {
        let history = History::new("single-shard random", seed, random_ops(seed, 80));
        let first = assert_replays(&history, |history| {
            run_random_history(history.seed(), history.operations())
        });

        let trace = first.artifact().event_record();
        InvariantSuite::standard().assert(trace);
        saw_closed |= trace.iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Closed,
                    ..
                }
            )
        });
        saw_full |= trace.iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Full,
                    ..
                }
            )
        });
        saw_panic |= trace
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::HandlerPanicked));
        saw_timer |= trace.iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::Sleep,
                    ..
                }
            )
        });
    }

    assert!(saw_closed, "random single-shard sweep should hit Closed");
    assert!(saw_full, "random single-shard sweep should hit Full");
    assert!(
        saw_panic,
        "random single-shard sweep should hit panic capture"
    );
    assert!(
        saw_timer,
        "random single-shard sweep should hit timer completion"
    );
}

#[test]
fn dst_history_shrinker_keeps_replayable_failure_but_removes_noise() {
    let noisy = History::new(
        "closed-rejection shrink",
        123,
        vec![
            RandomOp::TimerAfter(1),
            RandomOp::DriverSend(2),
            RandomOp::Step,
            RandomOp::StopTarget,
            RandomOp::RunUntilIdle,
            RandomOp::DriverSend(9),
            RandomOp::RunUntilIdle,
            RandomOp::TimerAfter(10),
        ],
    );
    fn has_closed_rejection(history: &History<RandomOp>) -> bool {
        run_random_history(history.seed(), history.operations())
            .artifact()
            .event_record()
            .iter()
            .any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::SendRejected {
                        reason: SendRejectedReason::Closed,
                        ..
                    }
                )
            })
    }
    assert!(has_closed_rejection(&noisy));

    let shrunk = delete_shrink(
        &noisy,
        ShrinkConfig::default(),
        "closed rejection survives",
        has_closed_rejection,
    );
    assert!(
        shrunk.shrunk().len() < noisy.len(),
        "shrinker should remove noise"
    );
    assert!(has_closed_rejection(shrunk.shrunk()));
    assert!(
        shrunk.shrunk().len() <= 4,
        "small closed-rejection reproducer should stay readable: {:?}",
        shrunk.shrunk().operations()
    );
}

#[test]
fn seeded_random_multishard_histories_replay_and_keep_remote_pressure_visible() {
    let mut saw_remote_full = false;
    let mut saw_remote_closed = false;

    for seed in 0..32 {
        let history = History::new("multi-shard random", seed, random_multi_ops(seed, 80));
        let first = assert_replays(&history, |history| {
            run_random_multishard_history(history.seed(), history.operations())
        });

        let trace = first.artifact().event_record();
        InvariantSuite::standard().assert(trace);
        saw_remote_full |= trace.iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Full,
                    ..
                }
            )
        });
        saw_remote_closed |= trace.iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Closed,
                    ..
                }
            )
        });
    }

    assert!(
        saw_remote_full,
        "random multi-shard sweep should hit bounded remote Full"
    );
    assert!(
        saw_remote_closed,
        "random multi-shard sweep should hit stale/closed remote target"
    );
}

#[test]
fn dst_long_seed_sweep_replays_when_enabled() {
    if std::env::var_os("TINA_DST_LONG").is_none() {
        return;
    }

    for seed in 0..512 {
        let single = History::new("long single-shard random", seed, random_ops(seed, 96));
        let single_run = assert_replays(&single, |history| {
            run_random_history(history.seed(), history.operations())
        });
        InvariantSuite::standard().assert(single_run.artifact().event_record());

        let multi = History::new("long multi-shard random", seed, random_multi_ops(seed, 96));
        let multi_run = assert_replays(&multi, |history| {
            run_random_multishard_history(history.seed(), history.operations())
        });
        InvariantSuite::standard().assert(multi_run.artifact().event_record());
    }
}
