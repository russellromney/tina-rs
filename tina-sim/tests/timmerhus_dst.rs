use std::cell::RefCell;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina::{Mailbox, TrySendError};
use tina_runtime::{
    LiveShardState, LocalSystem, MailboxFactory, RuntimeEvent, RuntimeEventKind,
    SendRejectedReason, TraceRetention,
};
use tina_sim::dst::{
    DstRun, History, InvariantSuite, ReplayCase, ReplayConfig, ReplayReport, ShrinkConfig,
    assert_replay_case, assert_replays, delete_shrink,
};
use tina_sim::{
    MultiShardReplayArtifact, MultiShardSimulator, MultiShardSimulatorConfig, SimulatorConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DstShard(u32);

impl Shard for DstShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

struct LiveMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> LiveMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for LiveMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed lock") {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.lock().expect("queue lock");
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue lock").pop_front()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed lock") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct LiveMailboxFactory;

impl MailboxFactory for LiveMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(LiveMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetMsg {
    Value(u64),
    Stop,
}

#[derive(Debug)]
struct Target {
    values: Rc<RefCell<Vec<u64>>>,
}

#[tina_runtime::isolate(message = TargetMsg, shard = DstShard)]
impl Target {
    fn handle(
        &mut self,
        msg: TargetMsg,
        _ctx: &mut Context<'_, DstShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TargetMsg::Value(value) => {
                self.values.borrow_mut().push(value);
                noop()
            }
            TargetMsg::Stop => stop(),
        }
    }
}

#[derive(Debug)]
struct LiveTarget {
    values: Arc<Mutex<Vec<u64>>>,
}

#[tina_runtime::isolate(message = TargetMsg, shard = DstShard)]
impl LiveTarget {
    fn handle(
        &mut self,
        msg: TargetMsg,
        _ctx: &mut Context<'_, DstShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TargetMsg::Value(value) => {
                self.values.lock().expect("live values lock").push(value);
                noop()
            }
            TargetMsg::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceMsg {
    Send(u64),
    Burst(u64),
}

#[derive(Debug)]
struct Source {
    target: Address<TargetMsg>,
}

#[tina_runtime::isolate(message = SourceMsg, send = Outbound<TargetMsg>, shard = DstShard)]
impl Source {
    fn handle(
        &mut self,
        msg: SourceMsg,
        _ctx: &mut Context<'_, DstShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SourceMsg::Send(value) => send(self.target, TargetMsg::Value(value)),
            SourceMsg::Burst(value) => batch([
                send(self.target, TargetMsg::Value(value)),
                send(self.target, TargetMsg::Value(value + 1)),
            ]),
        }
    }
}

#[derive(Debug)]
struct LiveSource {
    target: Address<TargetMsg>,
}

#[tina_runtime::isolate(message = SourceMsg, send = Outbound<TargetMsg>, shard = DstShard)]
impl LiveSource {
    fn handle(
        &mut self,
        msg: SourceMsg,
        _ctx: &mut Context<'_, DstShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SourceMsg::Send(value) => send(self.target, TargetMsg::Value(value)),
            SourceMsg::Burst(value) => batch([
                send(self.target, TargetMsg::Value(value)),
                send(self.target, TargetMsg::Value(value + 1)),
            ]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopologyOp {
    Direct(u64),
    StopTarget,
    CrossShard(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TopologyProjection {
    values: Vec<u64>,
    rejected_closed: usize,
    rejected_full: usize,
    mutation_after_first_rejection: bool,
}

fn topology_failure_history() -> History<TopologyOp> {
    History::new(
        "timmerhus topology failure",
        39,
        vec![
            TopologyOp::Direct(1),
            TopologyOp::StopTarget,
            TopologyOp::CrossShard(2),
            TopologyOp::CrossShard(3),
            TopologyOp::CrossShard(4),
        ],
    )
}

fn run_topology_history(
    history: &History<TopologyOp>,
) -> DstRun<TopologyProjection, MultiShardReplayArtifact> {
    let values = Rc::new(RefCell::new(Vec::new()));
    let mut sim = MultiShardSimulator::with_config(
        [DstShard(1), DstShard(2)],
        SimulatorConfig {
            seed: history.seed(),
            ..SimulatorConfig::default()
        },
        MultiShardSimulatorConfig {
            shard_pair_capacity: 2,
        },
    );
    let target = sim.register_with_capacity_on::<Target, TargetMsg, Infallible>(
        ShardId::new(2),
        Target {
            values: Rc::clone(&values),
        },
        8,
    );
    let source = sim.register_with_capacity_on::<Source, SourceMsg, TargetMsg>(
        ShardId::new(1),
        Source { target },
        8,
    );

    let mut first_rejected_at = None;
    let mut values_at_first_rejection = Vec::new();
    for op in history.operations() {
        match *op {
            TopologyOp::Direct(value) => {
                let _ = sim.try_send(target, TargetMsg::Value(value));
            }
            TopologyOp::StopTarget => {
                let _ = sim.try_send(target, TargetMsg::Stop);
            }
            TopologyOp::CrossShard(value) => {
                let _ = sim.try_send(source, SourceMsg::Send(value));
            }
        }
        sim.run_until_quiescent();
        let trace = sim.trace();
        if first_rejected_at.is_none()
            && trace
                .iter()
                .any(|event| matches!(event.kind(), RuntimeEventKind::SendRejected { .. }))
        {
            first_rejected_at = Some(trace.len());
            values_at_first_rejection = values.borrow().clone();
        }
    }

    let trace = sim.trace();
    InvariantSuite::standard().assert(&trace);
    let rejected_closed = trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Closed,
                    ..
                }
            )
        })
        .count();
    let rejected_full = trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Full,
                    ..
                }
            )
        })
        .count();
    let final_values = values.borrow().clone();
    let mutation_after_first_rejection =
        first_rejected_at.is_some() && final_values != values_at_first_rejection;
    let projection = TopologyProjection {
        values: final_values,
        rejected_closed,
        rejected_full,
        mutation_after_first_rejection,
    };
    DstRun::new(projection, sim.replay_artifact())
}

fn run_live_topology_history(history: &History<TopologyOp>) -> TopologyProjection {
    let values = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::<DstShard, LiveMailboxFactory>::multi_shard(LiveMailboxFactory)
        .shard(DstShard(1))
        .shard(DstShard(2))
        .ingress_capacity(8)
        .shard_pair_capacity(8)
        .trace_retention(TraceRetention::Bounded(512))
        .build();
    let target = app
        .register_root_on::<LiveTarget, Infallible>(
            ShardId::new(2),
            LiveTarget {
                values: Arc::clone(&values),
            },
            8,
        )
        .expect("register live target");
    let source = app
        .register_root_on::<LiveSource, TargetMsg>(ShardId::new(1), LiveSource { target }, 8)
        .expect("register live source");

    let mut first_rejection_values = Vec::new();
    let mut saw_rejection = false;
    for op in history.operations() {
        match *op {
            TopologyOp::Direct(value) => {
                app.try_send(target, TargetMsg::Value(value))
                    .expect("live direct handoff");
                wait_until("live direct value", || {
                    values.lock().expect("live values lock").contains(&value)
                });
            }
            TopologyOp::StopTarget => {
                app.try_send(target, TargetMsg::Stop)
                    .expect("live stop handoff");
                wait_until("live target stopped", || {
                    app.complete_trace()
                        .expect("live trace")
                        .iter()
                        .any(|event| {
                            event.shard() == ShardId::new(2)
                                && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
                        })
                });
            }
            TopologyOp::CrossShard(value) => {
                let before = send_rejected_closed(&app.complete_trace().expect("live trace"));
                app.try_send(source, SourceMsg::Send(value))
                    .expect("live source handoff");
                wait_until("live closed rejection", || {
                    send_rejected_closed(&app.complete_trace().expect("live trace")) > before
                });
                if !saw_rejection {
                    saw_rejection = true;
                    first_rejection_values = values.lock().expect("live values lock").clone();
                }
            }
        }
    }

    let terminal = app.shutdown().drain().join().expect("live shutdown");
    assert!(
        terminal
            .topology()
            .expect("terminal topology")
            .shards()
            .iter()
            .all(|shard| shard.state() == LiveShardState::Stopped)
    );
    let trace = terminal.trace();
    let final_values = values.lock().expect("live values lock").clone();
    TopologyProjection {
        values: final_values.clone(),
        rejected_closed: send_rejected_closed(trace),
        rejected_full: send_rejected_full(trace),
        mutation_after_first_rejection: saw_rejection && final_values != first_rejection_values,
    }
}

fn send_rejected_closed(trace: &[RuntimeEvent]) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Closed,
                    ..
                }
            )
        })
        .count()
}

fn send_rejected_full(trace: &[RuntimeEvent]) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Full,
                    ..
                }
            )
        })
        .count()
}

fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::yield_now();
    }
    assert!(predicate(), "{label} did not settle before timeout");
}

#[test]
fn live_sim_projection_matches_topology_failure_history() {
    let history = topology_failure_history();
    let sim = assert_replays(&history, run_topology_history);
    let live = run_live_topology_history(&history);

    tina_sim::dst::assert_projection_eq("sim", sim.output(), "live", &live);
    assert_eq!(live.values, vec![1]);
    assert_eq!(live.rejected_closed, 3);
    assert_eq!(live.rejected_full, 0);
    assert!(!live.mutation_after_first_rejection);
}

#[test]
fn topology_failure_history_shrinks() {
    let history = topology_failure_history();
    let shrunk = delete_shrink(
        &history,
        ShrinkConfig::default(),
        "history still produces closed cross-shard rejection",
        |candidate| run_topology_history(candidate).output().rejected_closed > 0,
    );

    assert!(shrunk.shrunk().len() < shrunk.original().len());
    assert!(
        run_topology_history(shrunk.shrunk())
            .output()
            .rejected_closed
            > 0
    );
}

#[test]
fn direct_send_after_stop_is_known_negative_contract() {
    let values = Rc::new(RefCell::new(Vec::new()));
    let mut sim = MultiShardSimulator::with_config(
        [DstShard(1), DstShard(2)],
        SimulatorConfig::default(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 2,
        },
    );
    let target = sim.register_with_capacity_on::<Target, TargetMsg, Infallible>(
        ShardId::new(2),
        Target {
            values: Rc::clone(&values),
        },
        8,
    );

    sim.try_send(target, TargetMsg::Value(1))
        .expect("initial direct send accepted");
    sim.run_until_quiescent();
    sim.try_send(target, TargetMsg::Stop)
        .expect("stop send accepted");
    sim.run_until_quiescent();

    assert_eq!(
        sim.try_send(target, TargetMsg::Value(2)),
        Err(TrySendError::Closed(TargetMsg::Value(2)))
    );
    sim.run_until_quiescent();

    assert_eq!(values.borrow().as_slice(), &[1]);
    assert!(
        !sim.trace().iter().any(|event| {
            event.isolate() == target.isolate()
                && matches!(event.kind(), RuntimeEventKind::MessageAbandoned)
        }),
        "closed direct ingress should be rejected before it becomes abandoned queued work"
    );
    InvariantSuite::standard().assert(&sim.trace());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressureOp {
    Burst,
    StopTarget,
    Drain,
}

fn remote_full_pressure_history() -> History<PressureOp> {
    History::new(
        "timmerhus remote full pressure",
        40,
        vec![PressureOp::Burst, PressureOp::Drain],
    )
}

fn run_remote_pressure_history(
    history: &History<PressureOp>,
) -> DstRun<TopologyProjection, MultiShardReplayArtifact> {
    let values = Rc::new(RefCell::new(Vec::new()));
    let mut sim = MultiShardSimulator::with_config(
        [DstShard(1), DstShard(2)],
        SimulatorConfig {
            seed: history.seed(),
            ..SimulatorConfig::default()
        },
        MultiShardSimulatorConfig {
            shard_pair_capacity: 1,
        },
    );
    let target = sim.register_with_capacity_on::<Target, TargetMsg, Infallible>(
        ShardId::new(2),
        Target {
            values: Rc::clone(&values),
        },
        8,
    );
    let source = sim.register_with_capacity_on::<Source, SourceMsg, TargetMsg>(
        ShardId::new(1),
        Source { target },
        8,
    );

    for op in history.operations() {
        match *op {
            PressureOp::Burst => {
                let _ = sim.try_send(source, SourceMsg::Burst(10));
            }
            PressureOp::StopTarget => {
                let _ = sim.try_send(target, TargetMsg::Stop);
            }
            PressureOp::Drain => {
                sim.run_until_quiescent();
            }
        }
    }
    sim.run_until_quiescent();

    let trace = sim.trace();
    InvariantSuite::standard().assert(&trace);
    DstRun::new(
        TopologyProjection {
            values: values.borrow().clone(),
            rejected_closed: send_rejected_closed(&trace),
            rejected_full: send_rejected_full(&trace),
            mutation_after_first_rejection: false,
        },
        sim.replay_artifact(),
    )
}

// Saved-seed regression case migrated to the `ReplayCase` shape so
// the `tina_sim::dst` types are the way to write new DST tests, not
// just an alternative parallel API. Pinning event count + trace hash
// catches drift in the trace shape; pinning the projection counts
// catches drift in the pressure/lifecycle invariant.
const REMOTE_FULL_EVENT_COUNT: usize = REMOTE_FULL_EVENT_COUNT_PIN;
const REMOTE_FULL_TRACE_HASH: u64 = REMOTE_FULL_TRACE_HASH_PIN;

// The pinned constants live in two consts so refreshing them is one
// search-and-replace at the top of the file.
const REMOTE_FULL_EVENT_COUNT_PIN: usize = 12;
const REMOTE_FULL_TRACE_HASH_PIN: u64 = 0x0300_4e4d_26f6_ddf3;

fn remote_full_pressure_case() -> ReplayCase<PressureOp> {
    let history = remote_full_pressure_history();
    ReplayCase {
        name: "timmerhus remote full pressure",
        seed: history.seed(),
        config: ReplayConfig::default(),
        scenario: "one cross-shard burst into a capacity-1 shard pair under default config",
        history,
        expected_event_count: REMOTE_FULL_EVENT_COUNT,
        expected_trace_hash: REMOTE_FULL_TRACE_HASH,
        invariant: "remote-shard pair `Full` shows up exactly once and target keeps the first value",
    }
}

fn run_remote_full_pressure_case(
    case: &ReplayCase<PressureOp>,
) -> ReplayReport<TopologyProjection> {
    let run = run_remote_pressure_history(&case.history);
    let (projection, artifact) = run.into_parts();
    ReplayReport::from_case_and_events(case, artifact.event_record(), projection)
}

#[test]
fn remote_full_burst_is_known_edge_contract_and_replays() {
    let report = assert_replay_case(&remote_full_pressure_case(), run_remote_full_pressure_case);
    assert_eq!(report.output.rejected_full, 1);
    assert_eq!(report.output.rejected_closed, 0);
    assert_eq!(report.output.values, vec![10]);
}

#[test]
fn remote_full_burst_history_shrinks() {
    let history = History::new(
        "noisy remote full pressure",
        41,
        vec![
            PressureOp::StopTarget,
            PressureOp::Drain,
            PressureOp::Burst,
            PressureOp::Burst,
            PressureOp::Drain,
        ],
    );
    let has_remote_full = |candidate: &History<PressureOp>| {
        run_remote_pressure_history(candidate)
            .output()
            .rejected_full
            > 0
    };
    assert!(has_remote_full(&history));

    let shrunk = delete_shrink(
        &history,
        ShrinkConfig::default(),
        "remote full pressure survives",
        has_remote_full,
    );

    assert!(shrunk.shrunk().len() < history.len());
    assert!(has_remote_full(shrunk.shrunk()));
}
