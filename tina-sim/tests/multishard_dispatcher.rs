use std::cell::RefCell;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use tina::{Address, RestartBudget, RestartPolicy, prelude::*};
use tina_runtime::{
    CallInput, CallKind, CallOutput, ListenerId, RuntimeCall, RuntimeEvent, RuntimeEventKind,
    SendRejectedReason, StreamId, sleep,
};
use tina_sim::{
    FaultConfig, FaultMode, MultiShardReplayArtifact, MultiShardSimulator,
    MultiShardSimulatorConfig, ObservedPeerOutput, ScriptedListenerConfig, ScriptedPeerConfig,
    ScriptedTcpConfig, SimulatorConfig, TcpCompletionFaultMode,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Clone, Copy)]
struct WorkShard(u32);

impl Shard for WorkShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoordinatorEvent {
    Submit {
        job_id: u64,
        value: u64,
    },
    SubmitPair {
        first_job: u64,
        first_value: u64,
        second_job: u64,
        second_value: u64,
    },
    JobCompleted {
        job_id: u64,
        doubled: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerEvent {
    Run {
        job_id: u64,
        value: u64,
        reply_to: Address<CoordinatorEvent>,
    },
}

#[derive(Debug)]
struct Coordinator {
    worker: Address<WorkerEvent>,
    completed: Rc<RefCell<Vec<(u64, u64)>>>,
}

impl Isolate for Coordinator {
    tina::isolate_types! {
        message: CoordinatorEvent,
        reply: (),
        send: Outbound<WorkerEvent>,
        spawn: Infallible,
        call: RuntimeCall<CoordinatorEvent>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CoordinatorEvent::Submit { job_id, value } => send(
                self.worker,
                WorkerEvent::Run {
                    job_id,
                    value,
                    reply_to: ctx.me(),
                },
            ),
            CoordinatorEvent::SubmitPair {
                first_job,
                first_value,
                second_job,
                second_value,
            } => batch([
                send(
                    self.worker,
                    WorkerEvent::Run {
                        job_id: first_job,
                        value: first_value,
                        reply_to: ctx.me(),
                    },
                ),
                send(
                    self.worker,
                    WorkerEvent::Run {
                        job_id: second_job,
                        value: second_value,
                        reply_to: ctx.me(),
                    },
                ),
            ]),
            CoordinatorEvent::JobCompleted { job_id, doubled } => {
                self.completed.borrow_mut().push((job_id, doubled));
                noop()
            }
        }
    }
}

#[derive(Debug)]
struct Worker;

impl Isolate for Worker {
    tina::isolate_types! {
        message: WorkerEvent,
        reply: (),
        send: Outbound<CoordinatorEvent>,
        spawn: Infallible,
        call: RuntimeCall<WorkerEvent>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WorkerEvent::Run {
                job_id,
                value,
                reply_to,
            } => send(
                reply_to,
                CoordinatorEvent::JobCompleted {
                    job_id,
                    doubled: value * 2,
                },
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimedCoordinatorEvent {
    Start,
    DelayElapsed,
    JobCompleted { job_id: u64, doubled: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimedWorkerEvent {
    Run {
        job_id: u64,
        value: u64,
        reply_to: Address<TimedCoordinatorEvent>,
    },
}

#[derive(Debug)]
struct TimedCoordinator {
    worker: Address<TimedWorkerEvent>,
    job_id: u64,
    value: u64,
    backoff: Duration,
    completed: Rc<RefCell<Vec<(u64, u64)>>>,
}

impl Isolate for TimedCoordinator {
    tina::isolate_types! {
        message: TimedCoordinatorEvent,
        reply: (),
        send: Outbound<TimedWorkerEvent>,
        spawn: Infallible,
        call: RuntimeCall<TimedCoordinatorEvent>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TimedCoordinatorEvent::Start => {
                sleep(self.backoff).reply(|_| TimedCoordinatorEvent::DelayElapsed)
            }
            TimedCoordinatorEvent::DelayElapsed => send(
                self.worker,
                TimedWorkerEvent::Run {
                    job_id: self.job_id,
                    value: self.value,
                    reply_to: ctx.me(),
                },
            ),
            TimedCoordinatorEvent::JobCompleted { job_id, doubled } => {
                self.completed.borrow_mut().push((job_id, doubled));
                noop()
            }
        }
    }
}

#[derive(Debug)]
struct TimedWorker;

impl Isolate for TimedWorker {
    tina::isolate_types! {
        message: TimedWorkerEvent,
        reply: (),
        send: Outbound<TimedCoordinatorEvent>,
        spawn: Infallible,
        call: RuntimeCall<TimedWorkerEvent>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TimedWorkerEvent::Run {
                job_id,
                value,
                reply_to,
            } => send(
                reply_to,
                TimedCoordinatorEvent::JobCompleted {
                    job_id,
                    doubled: value * 2,
                },
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorObservation {
    Booted(IsolateId),
    Worked(IsolateId, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorEvent {
    SpawnOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartableWorkerEvent {
    Boot,
    Work(u32),
    Poison,
}

#[derive(Debug)]
struct SupervisorObserver {
    log: Rc<RefCell<Vec<SupervisorObservation>>>,
}

impl Isolate for SupervisorObserver {
    tina::isolate_types! {
        message: SupervisorObservation,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<SupervisorObservation>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        self.log.borrow_mut().push(msg);
        noop()
    }
}

#[derive(Debug)]
struct RestartableWorker {
    observer: Address<SupervisorObservation>,
}

impl Isolate for RestartableWorker {
    tina::isolate_types! {
        message: RestartableWorkerEvent,
        reply: (),
        send: Outbound<SupervisorObservation>,
        spawn: Infallible,
        call: RuntimeCall<RestartableWorkerEvent>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            RestartableWorkerEvent::Boot => send(
                self.observer,
                SupervisorObservation::Booted(ctx.isolate_id()),
            ),
            RestartableWorkerEvent::Work(value) => send(
                self.observer,
                SupervisorObservation::Worked(ctx.isolate_id(), value),
            ),
            RestartableWorkerEvent::Poison => panic!("simulated multi-shard worker panic"),
        }
    }
}

#[derive(Debug)]
struct SupervisedParent {
    observer: Address<SupervisorObservation>,
}

impl Isolate for SupervisedParent {
    tina::isolate_types! {
        message: SupervisorEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: RestartableChildDefinition<RestartableWorker>,
        call: RuntimeCall<SupervisorEvent>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            SupervisorEvent::SpawnOne => spawn(
                RestartableChildDefinition::new(
                    {
                        let observer = self.observer;
                        move || RestartableWorker { observer }
                    },
                    8,
                )
                .with_initial_message(|| RestartableWorkerEvent::Boot),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TcpConnectionEvent {
    Start,
    ReadCompleted(Vec<u8>),
    WriteCompleted { count: usize },
    StreamClosed,
    Failed,
}

#[derive(Debug)]
struct TcpEchoConnection {
    stream: StreamId,
    pending_write: Vec<u8>,
}

impl Isolate for TcpEchoConnection {
    tina::isolate_types! {
        message: TcpConnectionEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<TcpConnectionEvent>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TcpConnectionEvent::Start => tcp_read_call(self.stream),
            TcpConnectionEvent::ReadCompleted(bytes) => {
                if bytes.is_empty() {
                    tcp_close_stream_call(self.stream)
                } else {
                    self.pending_write = bytes;
                    tcp_write_call(self.stream, self.pending_write.clone())
                }
            }
            TcpConnectionEvent::WriteCompleted { count } => {
                if count >= self.pending_write.len() {
                    self.pending_write.clear();
                    tcp_read_call(self.stream)
                } else {
                    self.pending_write.drain(..count);
                    tcp_write_call(self.stream, self.pending_write.clone())
                }
            }
            TcpConnectionEvent::StreamClosed | TcpConnectionEvent::Failed => stop(),
        }
    }
}

fn tcp_read_call(stream: StreamId) -> Effect<TcpEchoConnection> {
    Effect::Call(RuntimeCall::new(
        CallInput::TcpRead {
            stream,
            max_len: 64,
        },
        |result| match result {
            CallOutput::TcpRead { bytes } => TcpConnectionEvent::ReadCompleted(bytes),
            CallOutput::Failed(_) => TcpConnectionEvent::Failed,
            other => panic!("unexpected read result {other:?}"),
        },
    ))
}

fn tcp_write_call(stream: StreamId, bytes: Vec<u8>) -> Effect<TcpEchoConnection> {
    Effect::Call(RuntimeCall::new(
        CallInput::TcpWrite { stream, bytes },
        |result| match result {
            CallOutput::TcpWrote { count } => TcpConnectionEvent::WriteCompleted { count },
            CallOutput::Failed(_) => TcpConnectionEvent::Failed,
            other => panic!("unexpected write result {other:?}"),
        },
    ))
}

fn tcp_close_stream_call(stream: StreamId) -> Effect<TcpEchoConnection> {
    Effect::Call(RuntimeCall::new(
        CallInput::TcpStreamClose { stream },
        |result| match result {
            CallOutput::TcpStreamClosed => TcpConnectionEvent::StreamClosed,
            CallOutput::Failed(_) => TcpConnectionEvent::Failed,
            other => panic!("unexpected stream close result {other:?}"),
        },
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TcpControlEvent {
    Bootstrap,
    Bound { listener: ListenerId },
    ReArmAccept,
    Accepted { stream: StreamId },
    CloseListener,
    ListenerClosed,
    ListenerFinished,
    Failed,
}

#[derive(Debug)]
struct TcpEchoListener {
    bind_addr: SocketAddr,
    target_accepts: usize,
    accepted: usize,
    listener: Option<ListenerId>,
    report_to: Address<TcpControlEvent>,
}

impl Isolate for TcpEchoListener {
    tina::isolate_types! {
        message: TcpControlEvent,
        reply: (),
        send: Outbound<TcpControlEvent>,
        spawn: RestartableChildDefinition<TcpEchoConnection>,
        call: RuntimeCall<TcpControlEvent>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TcpControlEvent::Bootstrap => {
                let addr = self.bind_addr;
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpBind { addr },
                    |result| match result {
                        CallOutput::TcpBound { listener, .. } => {
                            TcpControlEvent::Bound { listener }
                        }
                        CallOutput::Failed(_) => TcpControlEvent::Failed,
                        other => panic!("unexpected bind result {other:?}"),
                    },
                ))
            }
            TcpControlEvent::Bound { listener } => {
                self.listener = Some(listener);
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpAccept { listener },
                    |result| match result {
                        CallOutput::TcpAccepted { stream, .. } => {
                            TcpControlEvent::Accepted { stream }
                        }
                        CallOutput::Failed(_) => TcpControlEvent::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            TcpControlEvent::ReArmAccept => {
                let listener = self.listener.expect("listener stored before re-arm");
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpAccept { listener },
                    |result| match result {
                        CallOutput::TcpAccepted { stream, .. } => {
                            TcpControlEvent::Accepted { stream }
                        }
                        CallOutput::Failed(_) => TcpControlEvent::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            TcpControlEvent::Accepted { stream } => {
                self.accepted += 1;
                let spawn_effect = spawn(
                    RestartableChildDefinition::new(
                        move || TcpEchoConnection {
                            stream,
                            pending_write: Vec::new(),
                        },
                        8,
                    )
                    .with_initial_message(|| TcpConnectionEvent::Start),
                );
                let follow_up = if self.accepted < self.target_accepts {
                    TcpControlEvent::ReArmAccept
                } else {
                    TcpControlEvent::CloseListener
                };
                batch([spawn_effect, ctx.send_self(follow_up)])
            }
            TcpControlEvent::CloseListener => {
                let listener = self.listener.expect("listener stored before close");
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpListenerClose { listener },
                    |result| match result {
                        CallOutput::TcpListenerClosed => TcpControlEvent::ListenerClosed,
                        CallOutput::Failed(_) => TcpControlEvent::Failed,
                        other => panic!("unexpected listener close result {other:?}"),
                    },
                ))
            }
            TcpControlEvent::ListenerClosed => batch([
                send(self.report_to, TcpControlEvent::ListenerFinished),
                stop(),
            ]),
            TcpControlEvent::ListenerFinished | TcpControlEvent::Failed => stop(),
        }
    }
}

#[derive(Debug)]
struct TcpCoordinator {
    done: Rc<RefCell<usize>>,
}

impl Isolate for TcpCoordinator {
    tina::isolate_types! {
        message: TcpControlEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<TcpControlEvent>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TcpControlEvent::ListenerFinished => {
                *self.done.borrow_mut() += 1;
                noop()
            }
            _ => stop(),
        }
    }
}

#[derive(Debug, Clone)]
struct SavedMultiShardRun {
    artifact: MultiShardReplayArtifact,
    completed: Vec<(u64, u64)>,
}

fn event_id(trace: &[RuntimeEvent], predicate: impl Fn(&RuntimeEvent) -> bool) -> u64 {
    trace
        .iter()
        .find(|event| predicate(event))
        .unwrap_or_else(|| panic!("expected matching event in trace"))
        .id()
        .get()
}

fn run_timed_dispatcher_workload(
    simulator_config: SimulatorConfig,
    multishard_config: MultiShardSimulatorConfig,
) -> SavedMultiShardRun {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        simulator_config,
        multishard_config,
    );
    let completed = Rc::new(RefCell::new(Vec::new()));

    let worker = sim
        .register_with_capacity_on::<TimedWorker, TimedWorkerEvent, TimedCoordinatorEvent>(
            ShardId::new(22),
            TimedWorker,
            4,
        );
    let coordinator = sim
        .register_with_capacity_on::<TimedCoordinator, TimedCoordinatorEvent, TimedWorkerEvent>(
            ShardId::new(11),
            TimedCoordinator {
                worker,
                job_id: 9,
                value: 7,
                backoff: Duration::from_millis(25),
                completed: Rc::clone(&completed),
            },
            4,
        );

    sim.try_send(coordinator, TimedCoordinatorEvent::Start)
        .unwrap();
    sim.run_until_quiescent();

    SavedMultiShardRun {
        artifact: sim.replay_artifact(),
        completed: completed.borrow().clone(),
    }
}

fn bind_addr() -> SocketAddr {
    "127.0.0.1:0".parse().expect("loopback bind addr")
}

fn local_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}")
        .parse()
        .expect("loopback local addr")
}

fn peer_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}")
        .parse()
        .expect("loopback peer addr")
}

fn peer_script(
    accept_after_step: u64,
    peer_addr: SocketAddr,
    inbound_chunks: Vec<Vec<u8>>,
    read_chunk_cap: Option<usize>,
    write_cap: usize,
) -> ScriptedPeerConfig {
    ScriptedPeerConfig {
        accept_after_step,
        peer_addr,
        inbound_capacity: inbound_chunks.iter().map(Vec::len).sum(),
        inbound_chunks,
        read_chunk_cap,
        write_cap,
        output_capacity: 1024,
    }
}

fn spawned_children(trace: &[RuntimeEvent]) -> Vec<IsolateId> {
    trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::Spawned { child_isolate, .. } => Some(child_isolate),
            _ => None,
        })
        .collect()
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

fn run_dispatcher_workload(
    simulator_config: SimulatorConfig,
    multishard_config: MultiShardSimulatorConfig,
    event: CoordinatorEvent,
) -> SavedMultiShardRun {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        simulator_config.clone(),
        multishard_config,
    );
    let completed = Rc::new(RefCell::new(Vec::new()));

    let worker = sim.register_with_capacity_on::<Worker, WorkerEvent, CoordinatorEvent>(
        ShardId::new(22),
        Worker,
        4,
    );
    let coordinator = sim.register_with_capacity_on::<Coordinator, CoordinatorEvent, WorkerEvent>(
        ShardId::new(11),
        Coordinator {
            worker,
            completed: Rc::clone(&completed),
        },
        4,
    );

    sim.try_send(coordinator, event).unwrap();
    sim.run_until_quiescent();

    SavedMultiShardRun {
        artifact: sim.replay_artifact(),
        completed: completed.borrow().clone(),
    }
}

#[test]
fn multishard_dispatcher_workload_preserves_request_reply_causality() {
    let run = run_dispatcher_workload(
        SimulatorConfig::default(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
        CoordinatorEvent::Submit {
            job_id: 7,
            value: 21,
        },
    );

    assert_eq!(run.completed, vec![(7, 42)]);

    let request_attempt = event_id(run.artifact.event_record(), |event| {
        event.shard() == ShardId::new(11)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendDispatchAttempted {
                    target_shard,
                    ..
                } if target_shard == ShardId::new(22)
            )
    });
    let request_accept = event_id(run.artifact.event_record(), |event| {
        event.shard() == ShardId::new(11)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendAccepted {
                    target_shard,
                    ..
                } if target_shard == ShardId::new(22)
            )
    });
    let worker_mailbox = event_id(run.artifact.event_record(), |event| {
        event.shard() == ShardId::new(22)
            && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
            && event
                .cause()
                .is_some_and(|cause| cause.event().get() == request_attempt)
    });
    let reply_attempt = event_id(run.artifact.event_record(), |event| {
        event.shard() == ShardId::new(22)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendDispatchAttempted {
                    target_shard,
                    ..
                } if target_shard == ShardId::new(11)
            )
    });
    let coordinator_mailbox = event_id(run.artifact.event_record(), |event| {
        event.shard() == ShardId::new(11)
            && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
            && event
                .cause()
                .is_some_and(|cause| cause.event().get() == reply_attempt)
    });

    assert!(request_attempt < request_accept);
    assert!(request_accept < worker_mailbox);
    assert!(worker_mailbox < reply_attempt);
    assert!(reply_attempt < coordinator_mailbox);
}

#[test]
fn multishard_dispatcher_workload_surfaces_source_time_full_rejection() {
    let run = run_dispatcher_workload(
        SimulatorConfig::default(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 1,
        },
        CoordinatorEvent::SubmitPair {
            first_job: 1,
            first_value: 10,
            second_job: 2,
            second_value: 20,
        },
    );

    assert_eq!(run.completed, vec![(1, 20)]);

    let full_rejections = run
        .artifact
        .event_record()
        .iter()
        .filter(|event| {
            event.shard() == ShardId::new(11)
                && matches!(
                    event.kind(),
                    RuntimeEventKind::SendRejected {
                        reason: SendRejectedReason::Full,
                        ..
                    }
                )
        })
        .count();
    assert_eq!(full_rejections, 1);
}

#[test]
fn multishard_dispatcher_workload_replays_from_saved_config() {
    let saved = run_dispatcher_workload(
        SimulatorConfig::default(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
        CoordinatorEvent::Submit {
            job_id: 11,
            value: 5,
        },
    );

    let replayed = run_dispatcher_workload(
        saved.artifact.simulator_config().clone(),
        saved.artifact.multishard_config(),
        CoordinatorEvent::Submit {
            job_id: 11,
            value: 5,
        },
    );

    assert_eq!(replayed.completed, saved.completed);
    assert_eq!(replayed.artifact, saved.artifact);
}

#[test]
fn multishard_dispatcher_composes_with_seeded_timer_faults() {
    let run = run_timed_dispatcher_workload(
        SimulatorConfig {
            seed: 17,
            faults: FaultConfig {
                timer_wake: FaultMode::DelayBy {
                    one_in: 2,
                    by: Duration::from_millis(7),
                },
                ..Default::default()
            },
            ..Default::default()
        },
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
    );

    assert_eq!(run.completed, vec![(9, 14)]);
    assert!(
        run.artifact.event_record().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::Sleep,
                    ..
                }
            )
        }),
        "timer-gated multi-shard workload should still prove that remote work started from a runtime-owned sleep"
    );
}

#[test]
fn same_non_default_seed_faulted_multishard_dispatcher_replays_same_artifact() {
    let config = SimulatorConfig {
        seed: 17,
        faults: FaultConfig {
            timer_wake: FaultMode::DelayBy {
                one_in: 2,
                by: Duration::from_millis(7),
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let first = run_timed_dispatcher_workload(
        config.clone(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
    );
    let second = run_timed_dispatcher_workload(
        config,
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
    );

    assert_eq!(first.completed, second.completed);
    assert_eq!(first.artifact, second.artifact);
}

#[test]
fn different_seeds_can_diverge_in_faulted_multishard_dispatcher_replay() {
    let delayed = SimulatorConfig {
        seed: 17,
        faults: FaultConfig {
            timer_wake: FaultMode::DelayBy {
                one_in: 2,
                by: Duration::from_millis(7),
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let baseline = SimulatorConfig {
        seed: 18,
        faults: delayed.faults,
        ..Default::default()
    };

    let delayed_run = run_timed_dispatcher_workload(
        delayed,
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
    );
    let baseline_run = run_timed_dispatcher_workload(
        baseline,
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
    );

    assert_eq!(delayed_run.completed, baseline_run.completed);
    assert_ne!(
        delayed_run.artifact.final_time(),
        baseline_run.artifact.final_time(),
        "different non-default seeds should be able to perturb timer-gated multi-shard timing"
    );
}

#[test]
fn multishard_tcp_workload_composes_with_seeded_tcp_completion_faults() {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        SimulatorConfig {
            seed: 33,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 2,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 16,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(50000),
                    backlog_capacity: 2,
                    peers: vec![
                        peer_script(1, peer_addr(61001), vec![b"alpha".to_vec()], Some(2), 2),
                        peer_script(1, peer_addr(61002), vec![b"beta".to_vec()], Some(2), 2),
                    ],
                }],
            },
        },
        MultiShardSimulatorConfig {
            shard_pair_capacity: 8,
        },
    );
    let done = Rc::new(RefCell::new(0usize));

    let coordinator = sim.register_with_capacity_on::<TcpCoordinator, TcpControlEvent, Infallible>(
        ShardId::new(11),
        TcpCoordinator {
            done: Rc::clone(&done),
        },
        4,
    );
    let listener = sim
        .register_with_capacity_on::<TcpEchoListener, TcpControlEvent, TcpControlEvent>(
            ShardId::new(22),
            TcpEchoListener {
                bind_addr: bind_addr(),
                target_accepts: 2,
                accepted: 0,
                listener: None,
                report_to: coordinator,
            },
            8,
        );

    sim.try_send(listener, TcpControlEvent::Bootstrap).unwrap();
    sim.run_until_quiescent();

    assert_eq!(*done.borrow(), 1);
    assert_eq!(
        sim.replay_artifact()
            .observed_peer_output()
            .iter()
            .map(ObservedPeerOutput::bytes)
            .collect::<Vec<_>>(),
        vec![b"alpha".as_slice(), b"beta".as_slice()]
    );
    assert!(
        sim.trace().iter().any(|event| {
            event.shard() == ShardId::new(11)
                && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
        }),
        "listener completion should cross shards and become visible to the coordinator"
    );
}

#[test]
fn multishard_supervision_workload_composes_with_seeded_local_send_delay() {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        SimulatorConfig {
            seed: 5,
            faults: FaultConfig {
                local_send: tina_sim::LocalSendFaultMode::DelayByRounds {
                    one_in: 1,
                    rounds: 2,
                },
                ..Default::default()
            },
            ..Default::default()
        },
        MultiShardSimulatorConfig {
            shard_pair_capacity: 8,
        },
    );
    let log = Rc::new(RefCell::new(Vec::new()));

    let observer = sim
        .register_with_capacity_on::<SupervisorObserver, SupervisorObservation, Infallible>(
            ShardId::new(11),
            SupervisorObserver {
                log: Rc::clone(&log),
            },
            16,
        );
    let parent = sim.register_with_capacity_on::<SupervisedParent, SupervisorEvent, Infallible>(
        ShardId::new(22),
        SupervisedParent { observer },
        8,
    );
    sim.supervise(
        parent,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(2)),
    );

    sim.try_send(parent, SupervisorEvent::SpawnOne).unwrap();
    sim.run_until_quiescent();

    let first = spawned_children(&sim.trace())[0];
    sim.try_send(
        Address::new(ShardId::new(22), first),
        RestartableWorkerEvent::Poison,
    )
    .unwrap();
    sim.run_until_quiescent();

    let replacement = completed_restarts(&sim.trace())[0].1;
    sim.try_send(
        Address::new(ShardId::new(22), replacement),
        RestartableWorkerEvent::Work(99),
    )
    .unwrap();
    sim.run_until_quiescent();

    let observed = log.borrow().clone();
    assert!(observed.contains(&SupervisorObservation::Booted(first)));
    assert!(observed.contains(&SupervisorObservation::Booted(replacement)));
    assert!(observed.contains(&SupervisorObservation::Worked(replacement, 99)));
}
