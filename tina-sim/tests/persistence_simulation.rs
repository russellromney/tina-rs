use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tina::prelude::*;
use tina::{Address, AddressGeneration, IsolateId, RestartBudget, RestartPolicy};
use tina_runtime::{
    CallError, CallKind, JournalReplay, JournalReplayWarning, RuntimeEventKind, SnapshotImage,
    journal_append, journal_replay, snapshot_commit, snapshot_load,
};
use tina_sim::{
    DurableImage, ScriptedStorageFaultConfig, Simulator, SimulatorConfig,
    dst::{DstRun, History, InvariantSuite, assert_replays, persistence_image_replays},
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SimShard;

impl Shard for SimShard {
    fn id(&self) -> ShardId {
        ShardId::new(36)
    }
}

#[derive(Debug, Clone)]
enum PersistMsg {
    Recover,
    SnapshotLoaded(Result<Option<SnapshotImage>, CallError>),
    JournalLoaded(Result<JournalReplay, CallError>),
    Mutate(String),
    AppendAt(u64, String),
    MutationDurable(Result<(), CallError>, u64, Vec<u8>),
    CommitSnapshot,
    SnapshotCommitted(Result<(), CallError>),
    PanicAfterAppend(String),
    RecoverThenPanic,
}

#[derive(Debug)]
struct PersistService {
    snapshot_path: PathBuf,
    journal_path: PathBuf,
    values: Vec<String>,
    last_journal_index: u64,
    observed: Arc<Mutex<Vec<String>>>,
}

#[derive(Debug, Clone)]
enum PersistParentMsg {
    SpawnChild,
}

#[derive(Debug)]
struct PersistParent {
    snapshot_path: PathBuf,
    journal_path: PathBuf,
    observed: Arc<Mutex<Vec<String>>>,
}

#[tina_runtime::isolate(
    message = PersistParentMsg,
    spawn = RestartableChildDefinition<PersistService>,
    shard = SimShard
)]
impl PersistParent {
    fn handle(
        &mut self,
        msg: PersistParentMsg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PersistParentMsg::SpawnChild => {
                let snapshot_path = self.snapshot_path.clone();
                let journal_path = self.journal_path.clone();
                let observed = Arc::clone(&self.observed);
                Effect::Spawn(
                    RestartableChildDefinition::new(
                        move || {
                            PersistService::new(
                                snapshot_path.clone(),
                                journal_path.clone(),
                                Arc::clone(&observed),
                            )
                        },
                        16,
                    )
                    .with_initial_message(|| PersistMsg::Recover),
                )
            }
        }
    }
}

#[tina_runtime::isolate(
    message = PersistMsg,
    send = Outbound<PersistMsg>,
    shard = SimShard
)]
impl PersistService {
    fn handle(
        &mut self,
        msg: PersistMsg,
        ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PersistMsg::Recover => {
                snapshot_load(self.snapshot_path.clone()).reply(PersistMsg::SnapshotLoaded)
            }
            PersistMsg::SnapshotLoaded(Ok(Some(snapshot))) => {
                self.values = decode_values(&snapshot.bytes);
                self.last_journal_index = snapshot.last_journal_index;
                journal_replay(self.journal_path.clone()).reply(PersistMsg::JournalLoaded)
            }
            PersistMsg::SnapshotLoaded(Ok(None)) => {
                journal_replay(self.journal_path.clone()).reply(PersistMsg::JournalLoaded)
            }
            PersistMsg::SnapshotLoaded(Err(error)) => {
                self.observed
                    .lock()
                    .expect("observed mutex")
                    .push(format!("snapshot-error:{error:?}"));
                stop()
            }
            PersistMsg::JournalLoaded(Ok(replay)) => {
                for record in replay.records {
                    if record.index > self.last_journal_index {
                        self.values
                            .push(String::from_utf8(record.bytes).expect("utf8 record"));
                        self.last_journal_index = record.index;
                    }
                }
                if replay.warning == Some(JournalReplayWarning::TruncatedTail) {
                    self.observed
                        .lock()
                        .expect("observed mutex")
                        .push("truncated-tail".to_owned());
                }
                self.record_state();
                noop()
            }
            PersistMsg::JournalLoaded(Err(error)) => {
                self.observed
                    .lock()
                    .expect("observed mutex")
                    .push(format!("journal-error:{error:?}"));
                stop()
            }
            PersistMsg::Mutate(value) => {
                let index = self.last_journal_index + 1;
                let record = value.into_bytes();
                journal_append(self.journal_path.clone(), index, record.clone())
                    .reply(move |result| PersistMsg::MutationDurable(result, index, record))
            }
            PersistMsg::AppendAt(index, value) => {
                let record = value.into_bytes();
                journal_append(self.journal_path.clone(), index, record.clone())
                    .reply(move |result| PersistMsg::MutationDurable(result, index, record))
            }
            PersistMsg::MutationDurable(Ok(()), index, record) => {
                self.values
                    .push(String::from_utf8(record).expect("utf8 record"));
                self.last_journal_index = index;
                self.record_state();
                noop()
            }
            PersistMsg::MutationDurable(Err(error), _, _) => {
                self.observed
                    .lock()
                    .expect("observed mutex")
                    .push(format!("append-error:{error:?}"));
                noop()
            }
            PersistMsg::CommitSnapshot => snapshot_commit(
                self.snapshot_path.clone(),
                encode_values(&self.values),
                self.last_journal_index,
            )
            .reply(PersistMsg::SnapshotCommitted),
            PersistMsg::SnapshotCommitted(Ok(())) => {
                self.observed
                    .lock()
                    .expect("observed mutex")
                    .push(format!("snapshot:{}", self.last_journal_index));
                noop()
            }
            PersistMsg::SnapshotCommitted(Err(error)) => {
                self.observed
                    .lock()
                    .expect("observed mutex")
                    .push(format!("snapshot-error:{error:?}"));
                noop()
            }
            PersistMsg::PanicAfterAppend(value) => {
                let index = self.last_journal_index + 1;
                let record = value.into_bytes();
                batch([
                    journal_append(self.journal_path.clone(), index, record.clone())
                        .reply(move |result| PersistMsg::MutationDurable(result, index, record)),
                    Effect::Send(Outbound::new(ctx.me(), PersistMsg::RecoverThenPanic)),
                ])
            }
            PersistMsg::RecoverThenPanic => panic!("persist service panic after durable append"),
        }
    }
}

impl PersistService {
    fn new(
        snapshot_path: PathBuf,
        journal_path: PathBuf,
        observed: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            snapshot_path,
            journal_path,
            values: Vec::new(),
            last_journal_index: 0,
            observed,
        }
    }

    fn record_state(&self) {
        self.observed
            .lock()
            .expect("observed mutex")
            .push(self.values.join(","));
    }
}

fn encode_values(values: &[String]) -> Vec<u8> {
    values.join("\n").into_bytes()
}

fn decode_values(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    String::from_utf8(bytes.to_vec())
        .expect("utf8 snapshot")
        .split('\n')
        .map(str::to_owned)
        .collect()
}

fn snapshot_path() -> PathBuf {
    PathBuf::from("/tmp/tina-persist-snapshot")
}

fn journal_path() -> PathBuf {
    PathBuf::from("/tmp/tina-persist-journal")
}

#[test]
fn simulator_replays_durable_recovery_from_durable_image() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut sim = Simulator::new(SimShard, SimulatorConfig::default());
    let address = sim.register_with_mailbox_capacity::<PersistService, PersistMsg, PersistMsg>(
        PersistService::new(snapshot_path(), journal_path(), Arc::clone(&observed)),
        16,
    );
    sim.try_send(address, PersistMsg::Mutate("hay".to_owned()))
        .unwrap();
    sim.run_until_quiescent();
    sim.try_send(address, PersistMsg::Mutate("oats".to_owned()))
        .unwrap();
    sim.run_until_quiescent();
    sim.try_send(address, PersistMsg::CommitSnapshot).unwrap();
    sim.run_until_quiescent();

    let artifact = sim.replay_artifact();
    assert!(artifact.durable_image().get(snapshot_path()).is_some());
    assert!(artifact.durable_image().get(journal_path()).is_some());

    let fresh_observed = Arc::new(Mutex::new(Vec::new()));
    let mut fresh = Simulator::new(SimShard, SimulatorConfig::default());
    fresh.load_durable_image(artifact.durable_image().clone());
    let fresh_address = fresh
        .register_with_mailbox_capacity::<PersistService, PersistMsg, PersistMsg>(
            PersistService::new(snapshot_path(), journal_path(), Arc::clone(&fresh_observed)),
            16,
        );
    fresh.try_send(fresh_address, PersistMsg::Recover).unwrap();
    fresh.run_until_quiescent();

    assert_eq!(
        fresh_observed.lock().expect("observed mutex").as_slice(),
        &["hay,oats"]
    );
    assert!(
        fresh
            .trace()
            .iter()
            .any(|event| { matches!(event.kind(), RuntimeEventKind::RecoveryStarted) })
    );
    assert!(
        fresh
            .trace()
            .iter()
            .any(|event| { matches!(event.kind(), RuntimeEventKind::RecoveryFinished) })
    );
    let recovery_started = fresh
        .trace()
        .iter()
        .find(|event| matches!(event.kind(), RuntimeEventKind::RecoveryStarted))
        .expect("recovery started")
        .id();
    let snapshot_completed = fresh
        .trace()
        .iter()
        .find(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::SnapshotLoad,
                    ..
                }
            )
        })
        .expect("snapshot load completed")
        .id();
    assert!(
        recovery_started < snapshot_completed,
        "recovery start should be traced before the load completes"
    );
    assert_eq!(
        sim.trace()
            .iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::JournalAppended { .. }))
            .count(),
        2
    );
    assert_eq!(
        sim.trace()
            .iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted {
                        call_kind: CallKind::JournalAppend,
                        ..
                    }
                )
            })
            .count(),
        2
    );
}

#[test]
fn simulator_visible_truncated_and_corrupt_recovery_paths() {
    let mut image = DurableImage::new();
    let good = tina_runtime::persistence::encode_journal_record(&tina_runtime::JournalRecord {
        index: 1,
        bytes: b"hay".to_vec(),
    });
    let mut truncated = good.clone();
    truncated.extend_from_slice(&[0, 1, 2]);
    image.insert(journal_path(), truncated);

    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut sim = Simulator::new(SimShard, SimulatorConfig::default());
    sim.load_durable_image(image);
    let address = sim.register_with_mailbox_capacity::<PersistService, PersistMsg, PersistMsg>(
        PersistService::new(snapshot_path(), journal_path(), Arc::clone(&observed)),
        16,
    );
    sim.try_send(address, PersistMsg::Recover).unwrap();
    sim.run_until_quiescent();
    assert_eq!(
        observed.lock().expect("observed mutex").as_slice(),
        &["truncated-tail", "hay"]
    );

    let mut corrupt = good;
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xff;
    let mut corrupt_image = DurableImage::new();
    corrupt_image.insert(journal_path(), corrupt);
    let corrupt_observed = Arc::new(Mutex::new(Vec::new()));
    let mut corrupt_sim = Simulator::new(SimShard, SimulatorConfig::default());
    corrupt_sim.load_durable_image(corrupt_image);
    let corrupt_address = corrupt_sim
        .register_with_mailbox_capacity::<PersistService, PersistMsg, PersistMsg>(
            PersistService::new(
                snapshot_path(),
                journal_path(),
                Arc::clone(&corrupt_observed),
            ),
            16,
        );
    corrupt_sim
        .try_send(corrupt_address, PersistMsg::Recover)
        .unwrap();
    corrupt_sim.run_until_quiescent();
    assert_eq!(
        corrupt_observed.lock().expect("observed mutex").as_slice(),
        &["journal-error:CorruptRecord"]
    );
    assert!(corrupt_sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::RecoveryFailed {
                reason: CallError::CorruptRecord
            }
        )
    }));
}

#[test]
fn simulator_journal_append_rejects_bad_next_index_before_mutation() {
    let mut image = DurableImage::new();
    image.insert(
        journal_path(),
        tina_runtime::persistence::encode_journal_record(&tina_runtime::JournalRecord {
            index: 2,
            bytes: b"existing".to_vec(),
        }),
    );
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut sim = Simulator::new(SimShard, SimulatorConfig::default());
    sim.load_durable_image(image);
    let address = sim.register_with_mailbox_capacity::<PersistService, PersistMsg, PersistMsg>(
        PersistService::new(snapshot_path(), journal_path(), Arc::clone(&observed)),
        16,
    );

    sim.try_send(address, PersistMsg::Mutate("first".to_owned()))
        .unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        observed.lock().expect("observed mutex").as_slice(),
        &["append-error:CorruptRecord"]
    );
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::JournalAppendFailed {
                record_index: 1,
                reason: CallError::CorruptRecord
            }
        )
    }));
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PersistenceOp {
    Mutate(String),
    BadAppend(u64, String),
    Snapshot,
    Recover,
    Step,
    RunUntilIdle,
}

fn xorshift64(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

fn persistence_ops(seed: u64, len: usize) -> Vec<PersistenceOp> {
    let mut state = seed ^ 0xa91f_2d1e_37bb_19c5;
    let mut ops = Vec::with_capacity(len);
    for index in 0..len {
        state = xorshift64(state);
        let value = format!("v{seed}-{index}");
        ops.push(match state % 9 {
            0..=3 => PersistenceOp::Mutate(value),
            4 => PersistenceOp::BadAppend(state % 3, value),
            5 => PersistenceOp::Snapshot,
            6 => PersistenceOp::Recover,
            7 => PersistenceOp::Step,
            _ => PersistenceOp::RunUntilIdle,
        });
    }
    ops
}

fn run_persistence_ops(seed: u64, ops: &[PersistenceOp]) -> DstRun<Vec<String>> {
    run_persistence_ops_with_storage(seed, ops, ScriptedStorageFaultConfig::default())
}

fn run_persistence_ops_with_storage(
    seed: u64,
    ops: &[PersistenceOp],
    storage: ScriptedStorageFaultConfig,
) -> DstRun<Vec<String>> {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut sim = Simulator::new(
        SimShard,
        SimulatorConfig {
            seed,
            storage,
            ..Default::default()
        },
    );
    let address = sim.register_with_mailbox_capacity::<PersistService, PersistMsg, PersistMsg>(
        PersistService::new(snapshot_path(), journal_path(), Arc::clone(&observed)),
        32,
    );

    for op in ops {
        match op {
            PersistenceOp::Mutate(value) => {
                let _ = sim.try_send(address, PersistMsg::Mutate(value.clone()));
            }
            PersistenceOp::BadAppend(index, value) => {
                let _ = sim.try_send(address, PersistMsg::AppendAt(*index, value.clone()));
            }
            PersistenceOp::Snapshot => {
                let _ = sim.try_send(address, PersistMsg::CommitSnapshot);
            }
            PersistenceOp::Recover => {
                let _ = sim.try_send(address, PersistMsg::Recover);
            }
            PersistenceOp::Step => {
                sim.step();
            }
            PersistenceOp::RunUntilIdle => {
                sim.run_until_quiescent();
            }
        }
    }
    sim.run_until_quiescent();

    DstRun::new(
        observed.lock().expect("observed mutex").clone(),
        sim.replay_artifact(),
    )
}

#[test]
fn simulator_random_persistence_matrix_replays_and_keeps_journal_recoverable() {
    for seed in 0..24 {
        let history = History::new("persistence fault matrix", seed, persistence_ops(seed, 48));
        let first = assert_replays(&history, |history| {
            run_persistence_ops(history.seed(), history.operations())
        });
        InvariantSuite::standard().assert(first.artifact().event_record());

        let mut fresh = Simulator::new(SimShard, SimulatorConfig::default());
        fresh.load_durable_image(first.artifact().durable_image().clone());
        let recovered_observed = Arc::new(Mutex::new(Vec::new()));
        let recovered = fresh
            .register_with_mailbox_capacity::<PersistService, PersistMsg, PersistMsg>(
                PersistService::new(
                    snapshot_path(),
                    journal_path(),
                    Arc::clone(&recovered_observed),
                ),
                16,
            );
        fresh.try_send(recovered, PersistMsg::Recover).unwrap();
        fresh.run_until_quiescent();

        assert!(
            !fresh.trace().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::RecoveryFailed {
                        reason: CallError::CorruptRecord
                    }
                )
            }),
            "seed {seed} produced an unrecoverable durable image"
        );
        if first
            .artifact()
            .durable_image()
            .get(journal_path())
            .is_some()
        {
            persistence_image_replays(first.artifact().durable_image(), journal_path()).unwrap();
        }
        assert!(first.artifact().event_record().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::JournalAppended { .. }
                    | RuntimeEventKind::JournalAppendFailed { .. }
                    | RuntimeEventKind::SnapshotCommitted
                    | RuntimeEventKind::RecoveryFinished
            )
        }));
    }
}

#[test]
fn persistence_history_replays_and_recovers_after_fault_injection() {
    let cases = [
        ScriptedStorageFaultConfig {
            fail_journal_append_at: Some(0),
            ..Default::default()
        },
        ScriptedStorageFaultConfig {
            truncate_journal_tail_at: Some(0),
            ..Default::default()
        },
        ScriptedStorageFaultConfig {
            corrupt_journal_record_at: Some(0),
            ..Default::default()
        },
        ScriptedStorageFaultConfig {
            fail_snapshot_commit_at: Some(1),
            ..Default::default()
        },
        ScriptedStorageFaultConfig {
            commit_uncertain_snapshot_at: Some(1),
            ..Default::default()
        },
    ];

    for (case_index, storage) in cases.into_iter().enumerate() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut sim = Simulator::new(
            SimShard,
            SimulatorConfig {
                storage,
                ..Default::default()
            },
        );
        let service = sim.register_with_mailbox_capacity::<PersistService, PersistMsg, PersistMsg>(
            PersistService::new(snapshot_path(), journal_path(), Arc::clone(&observed)),
            16,
        );

        sim.try_send(service, PersistMsg::Mutate(format!("case-{case_index}")))
            .unwrap();
        sim.run_until_quiescent();
        sim.try_send(service, PersistMsg::CommitSnapshot).unwrap();
        sim.run_until_quiescent();
        sim.try_send(service, PersistMsg::Recover).unwrap();
        sim.run_until_quiescent();

        let artifact = sim.replay_artifact();
        InvariantSuite::standard().assert(artifact.event_record());
        match case_index {
            0 => assert!(artifact.event_record().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::JournalAppendFailed {
                        reason: CallError::Io,
                        ..
                    }
                )
            })),
            1 => assert!(
                artifact
                    .event_record()
                    .iter()
                    .any(|event| { matches!(event.kind(), RuntimeEventKind::RecoveryFinished) })
            ),
            2 => assert!(artifact.event_record().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::RecoveryFailed {
                        reason: CallError::CorruptRecord
                    }
                )
            })),
            3 => assert!(artifact.event_record().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::SnapshotCommitFailed {
                        reason: CallError::Io
                    }
                )
            })),
            _ => assert!(artifact.event_record().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::SnapshotCommitFailed {
                        reason: CallError::CommitUncertain
                    }
                )
            })),
        }
    }
}

#[test]
fn baobab_dst_persistence_restart_truncate_and_corrupt_saved_histories() {
    let cases = [
        (
            "baobab persistence restart clean",
            46_100,
            ScriptedStorageFaultConfig::default(),
            "finished",
        ),
        (
            "baobab persistence restart truncated",
            46_101,
            ScriptedStorageFaultConfig {
                truncate_journal_tail_at: Some(0),
                ..Default::default()
            },
            "finished",
        ),
        (
            "baobab persistence restart corrupt",
            46_102,
            ScriptedStorageFaultConfig {
                corrupt_journal_record_at: Some(0),
                ..Default::default()
            },
            "corrupt",
        ),
    ];

    for (name, seed, storage, expected) in cases {
        let history = History::new(
            name,
            seed,
            vec![
                PersistenceOp::Mutate("hay".to_owned()),
                PersistenceOp::RunUntilIdle,
                PersistenceOp::Recover,
                PersistenceOp::RunUntilIdle,
            ],
        );
        let run = assert_replays(&history, |candidate| {
            run_persistence_ops_with_storage(candidate.seed(), candidate.operations(), storage)
        });
        InvariantSuite::standard().assert(run.artifact().event_record());
        match expected {
            "finished" => assert!(
                run.artifact()
                    .event_record()
                    .iter()
                    .any(|event| { matches!(event.kind(), RuntimeEventKind::RecoveryFinished) }),
                "{name} should finish recovery visibly"
            ),
            "corrupt" => assert!(
                run.artifact().event_record().iter().any(|event| {
                    matches!(
                        event.kind(),
                        RuntimeEventKind::RecoveryFailed {
                            reason: CallError::CorruptRecord
                        }
                    )
                }),
                "{name} should fail corrupt recovery visibly"
            ),
            _ => unreachable!("unknown expected persistence result"),
        }
    }
}

#[test]
fn simulator_supervision_persistence_recovers_after_durable_append_then_panic() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut sim = Simulator::new(SimShard, SimulatorConfig::default());
    let parent = sim.register_with_mailbox_capacity::<PersistParent, PersistParentMsg, Infallible>(
        PersistParent {
            snapshot_path: snapshot_path(),
            journal_path: journal_path(),
            observed: Arc::clone(&observed),
        },
        8,
    );
    sim.supervise(
        parent,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(1)),
    );
    sim.try_send(parent, PersistParentMsg::SpawnChild).unwrap();
    sim.run_until_quiescent();

    let child = first_spawned_child(sim.trace()).expect("spawned persist child");
    let child_address = persist_address(child);
    sim.try_send(child_address, PersistMsg::Mutate("before".to_owned()))
        .unwrap();
    sim.run_until_quiescent();
    sim.try_send(
        child_address,
        PersistMsg::PanicAfterAppend("durable".to_owned()),
    )
    .unwrap();
    sim.run_until_quiescent();

    let artifact = sim.replay_artifact();

    assert_eq!(
        observed.lock().expect("observed mutex").last(),
        Some(&"before,durable".to_owned())
    );
    assert!(
        artifact
            .event_record()
            .iter()
            .any(|event| { matches!(event.kind(), RuntimeEventKind::HandlerPanicked) })
    );
    assert!(
        artifact.event_record().iter().any(|event| {
            matches!(event.kind(), RuntimeEventKind::RestartChildCompleted { .. })
        })
    );
    assert!(artifact.event_record().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::JournalAppended { record_index: 2 }
        )
    }));
}

fn first_spawned_child(trace: &[tina_runtime::RuntimeEvent]) -> Option<IsolateId> {
    trace.iter().find_map(|event| match event.kind() {
        RuntimeEventKind::Spawned { child_isolate } => Some(child_isolate),
        _ => None,
    })
}

fn persist_address(isolate: IsolateId) -> Address<PersistMsg> {
    Address::new_with_generation(SimShard.id(), isolate, AddressGeneration::new(0))
}
