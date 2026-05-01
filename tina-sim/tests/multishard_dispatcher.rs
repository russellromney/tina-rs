use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;

use tina::{Address, prelude::*};
use tina_runtime::{RuntimeCall, RuntimeEvent, RuntimeEventKind, SendRejectedReason};
use tina_sim::{
    MultiShardReplayArtifact, MultiShardSimulator, MultiShardSimulatorConfig, SimulatorConfig,
};

#[derive(Debug, Clone, Copy)]
struct WorkShard(u32);

impl Shard for WorkShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoordinatorEvent {
    Submit { job_id: u64, value: u64 },
    SubmitPair {
        first_job: u64,
        first_value: u64,
        second_job: u64,
        second_value: u64,
    },
    JobCompleted { job_id: u64, doubled: u64 },
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

#[derive(Debug, Clone)]
struct SavedMultiShardRun {
    artifact: MultiShardReplayArtifact,
    completed: Vec<(u64, u64)>,
}

fn event_id(trace: &[RuntimeEvent], predicate: impl Fn(&RuntimeEvent) -> bool) -> u64 {
    trace.iter()
        .find(|event| predicate(event))
        .unwrap_or_else(|| panic!("expected matching event in trace"))
        .id()
        .get()
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
            && event.cause().is_some_and(|cause| cause.event().get() == request_attempt)
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
            && event.cause().is_some_and(|cause| cause.event().get() == reply_attempt)
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
