use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallError, CallKind, JournalReplay, JournalReplayWarning, LOCAL_PERSISTENCE_SUPPORT, LocalApp,
    LocalAppState, MailboxFactory, PersistenceSupportLevel, Runtime, RuntimeEventKind,
    SnapshotImage, TraceRetention, journal_append, journal_replay, snapshot_commit, snapshot_load,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PersistShard;

impl Shard for PersistShard {
    fn id(&self) -> ShardId {
        ShardId::new(36)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed mutex") {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.lock().expect("queue mutex");
        if queue.len() == self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue mutex").pop_front()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed mutex") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

#[derive(Debug, Clone)]
enum PersistMsg {
    Recover,
    SnapshotLoaded(Result<Option<SnapshotImage>, CallError>),
    JournalLoaded(Result<JournalReplay, CallError>),
    Mutate(String),
    MutationDurable(Result<(), CallError>, u64, Vec<u8>),
    CommitSnapshot,
    SnapshotCommitted(Result<(), CallError>),
}

#[derive(Debug)]
struct PersistService {
    snapshot_path: PathBuf,
    journal_path: PathBuf,
    values: Vec<String>,
    last_journal_index: u64,
    observed: Arc<Mutex<Vec<String>>>,
}

#[tina_runtime::isolate(message = PersistMsg, shard = PersistShard)]
impl PersistService {
    fn handle(&mut self, msg: PersistMsg, _ctx: &mut Context<'_, PersistShard>) -> Effect<Self> {
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
            PersistMsg::CommitSnapshot => {
                let bytes = encode_values(&self.values);
                snapshot_commit(self.snapshot_path.clone(), bytes, self.last_journal_index)
                    .reply(PersistMsg::SnapshotCommitted)
            }
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

fn run_until_idle(runtime: &mut Runtime<PersistShard, TestMailboxFactory>) {
    for _ in 0..512 {
        let delivered = runtime.step();
        if delivered == 0 && !runtime.has_in_flight_calls() {
            return;
        }
    }
    panic!("runtime did not quiesce");
}

fn unique_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tina-{name}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(predicate(), "condition did not become true before timeout");
}

#[test]
fn local_persistence_support_table_names_platform_claims() {
    assert_eq!(
        LOCAL_PERSISTENCE_SUPPORT.temp_write_before_rename,
        PersistenceSupportLevel::Supported
    );
    assert_eq!(
        LOCAL_PERSISTENCE_SUPPORT.rename_commit,
        PersistenceSupportLevel::Supported
    );
    assert_eq!(
        LOCAL_PERSISTENCE_SUPPORT.file_fsync,
        PersistenceSupportLevel::Supported
    );
    assert_eq!(
        LOCAL_PERSISTENCE_SUPPORT.truncated_tail_warning,
        PersistenceSupportLevel::Supported
    );
    assert_eq!(
        LOCAL_PERSISTENCE_SUPPORT.checksum_validation,
        PersistenceSupportLevel::Supported
    );
    #[cfg(unix)]
    {
        assert_eq!(
            LOCAL_PERSISTENCE_SUPPORT.rename_commit,
            PersistenceSupportLevel::Supported
        );
        assert_eq!(
            LOCAL_PERSISTENCE_SUPPORT.directory_fsync_after_rename,
            PersistenceSupportLevel::Supported
        );
    }
    #[cfg(not(unix))]
    {
        assert_eq!(
            LOCAL_PERSISTENCE_SUPPORT.rename_commit,
            PersistenceSupportLevel::NotClaimed
        );
        assert_eq!(
            LOCAL_PERSISTENCE_SUPPORT.directory_fsync_after_rename,
            PersistenceSupportLevel::NotClaimed
        );
    }
}

#[test]
fn stale_snapshot_temp_file_does_not_replace_current_snapshot() {
    let dir = unique_dir("stale-temp-snapshot");
    let snapshot = dir.join("state.snapshot");
    tina_runtime::persistence::commit_snapshot(&snapshot, b"good".to_vec(), 7).unwrap();
    fs::write(
        dir.join(".state.snapshot.stale.tmp"),
        tina_runtime::persistence::encode_snapshot(&SnapshotImage {
            bytes: b"bad".to_vec(),
            last_journal_index: 99,
        }),
    )
    .expect("write stale temp snapshot");

    let loaded = tina_runtime::persistence::load_snapshot(&snapshot)
        .unwrap()
        .expect("current snapshot");
    assert_eq!(loaded.bytes, b"good");
    assert_eq!(loaded.last_journal_index, 7);
}

#[test]
fn local_runtime_persists_recovers_and_continues() {
    let dir = unique_dir("persistence-recover");
    let snapshot = dir.join("state.snapshot");
    let journal = dir.join("state.journal");
    let observed = Arc::new(Mutex::new(Vec::new()));

    let mut runtime = Runtime::new(PersistShard, TestMailboxFactory);
    let address = runtime.register_with_capacity::<PersistService, Infallible>(
        PersistService::new(snapshot.clone(), journal.clone(), Arc::clone(&observed)),
        16,
    );
    runtime
        .try_send(address, PersistMsg::Mutate("hay".to_owned()))
        .unwrap();
    run_until_idle(&mut runtime);
    runtime
        .try_send(address, PersistMsg::Mutate("oats".to_owned()))
        .unwrap();
    run_until_idle(&mut runtime);
    runtime
        .try_send(address, PersistMsg::CommitSnapshot)
        .unwrap();
    run_until_idle(&mut runtime);

    let mut fresh = Runtime::new(PersistShard, TestMailboxFactory);
    let fresh_address = fresh.register_with_capacity::<PersistService, Infallible>(
        PersistService::new(snapshot, journal, Arc::clone(&observed)),
        16,
    );
    fresh.try_send(fresh_address, PersistMsg::Recover).unwrap();
    run_until_idle(&mut fresh);
    fresh
        .try_send(fresh_address, PersistMsg::Mutate("apple".to_owned()))
        .unwrap();
    run_until_idle(&mut fresh);

    let events = observed.lock().expect("observed mutex").clone();
    assert!(events.contains(&"hay".to_owned()));
    assert!(events.contains(&"hay,oats".to_owned()));
    assert!(events.contains(&"snapshot:2".to_owned()));
    assert_eq!(events.last(), Some(&"hay,oats,apple".to_owned()));
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
    assert!(
        runtime
            .trace()
            .iter()
            .any(|event| { matches!(event.kind(), RuntimeEventKind::SnapshotCommitted) })
    );
    assert!(runtime.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::JournalAppended { record_index: 2 }
        )
    }));
}

#[test]
fn journal_replay_reports_truncated_tail_but_rejects_bad_checksum() {
    let good = tina_runtime::persistence::encode_journal_record(&tina_runtime::JournalRecord {
        index: 1,
        bytes: b"good".to_vec(),
    });
    let mut truncated = good.clone();
    truncated.extend_from_slice(&[1, 2, 3]);
    let replay = tina_runtime::persistence::replay_journal_bytes(&truncated).unwrap();
    assert_eq!(replay.records.len(), 1);
    assert_eq!(replay.warning, Some(JournalReplayWarning::TruncatedTail));

    let mut corrupt = good;
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xff;
    assert_eq!(
        tina_runtime::persistence::replay_journal_bytes(&corrupt),
        Err(CallError::CorruptRecord)
    );

    let mut out_of_order =
        tina_runtime::persistence::encode_journal_record(&tina_runtime::JournalRecord {
            index: 2,
            bytes: b"second".to_vec(),
        });
    out_of_order.extend_from_slice(&tina_runtime::persistence::encode_journal_record(
        &tina_runtime::JournalRecord {
            index: 1,
            bytes: b"first".to_vec(),
        },
    ));
    assert_eq!(
        tina_runtime::persistence::replay_journal_bytes(&out_of_order),
        Err(CallError::CorruptRecord)
    );
}

#[test]
fn journal_append_rejects_unreplayable_index_order_before_appending() {
    let dir = unique_dir("append-order-visible");
    let journal = dir.join("state.journal");
    tina_runtime::persistence::append_journal_record(&journal, 2, b"second".to_vec()).unwrap();

    assert_eq!(
        tina_runtime::persistence::append_journal_record(&journal, 1, b"first".to_vec()),
        Err(CallError::CorruptRecord)
    );
    assert_eq!(
        tina_runtime::persistence::append_journal_record(&journal, 2, b"duplicate".to_vec()),
        Err(CallError::CorruptRecord)
    );
    let replay = tina_runtime::persistence::replay_journal(&journal).unwrap();
    assert_eq!(replay.records.len(), 1);
    assert_eq!(replay.records[0].index, 2);
    assert_eq!(replay.records[0].bytes.as_slice(), b"second");

    fs::write(
        &journal,
        [
            tina_runtime::persistence::encode_journal_record(&tina_runtime::JournalRecord {
                index: 1,
                bytes: b"good".to_vec(),
            }),
            vec![1, 2, 3],
        ]
        .concat(),
    )
    .expect("write truncated journal");
    assert_eq!(
        tina_runtime::persistence::append_journal_record(&journal, 3, b"third".to_vec()),
        Err(CallError::CorruptRecord)
    );
}

#[test]
fn runtime_journal_append_rejects_bad_next_index_with_trace() {
    let dir = unique_dir("runtime-append-order-visible");
    let journal = dir.join("state.journal");
    tina_runtime::persistence::append_journal_record(&journal, 2, b"existing".to_vec()).unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = Runtime::new(PersistShard, TestMailboxFactory);
    let address = runtime.register_with_capacity::<PersistService, Infallible>(
        PersistService::new(dir.join("state.snapshot"), journal, Arc::clone(&observed)),
        16,
    );

    runtime
        .try_send(address, PersistMsg::Mutate("first".to_owned()))
        .unwrap();
    run_until_idle(&mut runtime);

    assert_eq!(
        observed.lock().expect("observed mutex").as_slice(),
        &["append-error:CorruptRecord"]
    );
    assert!(runtime.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::JournalAppendFailed {
                record_index: 1,
                reason: CallError::CorruptRecord
            }
        )
    }));
}

#[test]
fn failed_journal_append_is_visible_and_does_not_mutate_state() {
    let dir = unique_dir("append-failure-visible");
    let blocked_parent = dir.join("not-a-directory");
    fs::write(&blocked_parent, b"stone").expect("write blocking file");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = Runtime::new(PersistShard, TestMailboxFactory);
    let address = runtime.register_with_capacity::<PersistService, Infallible>(
        PersistService::new(
            dir.join("state.snapshot"),
            blocked_parent.join("state.journal"),
            Arc::clone(&observed),
        ),
        16,
    );

    runtime
        .try_send(address, PersistMsg::Mutate("hay".to_owned()))
        .unwrap();
    run_until_idle(&mut runtime);

    assert_eq!(
        observed.lock().expect("observed mutex").as_slice(),
        &["append-error:Io"]
    );
    assert!(runtime.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::JournalAppendFailed {
                record_index: 1,
                reason: CallError::Io
            }
        )
    }));
    assert!(
        !runtime
            .trace()
            .iter()
            .any(|event| { matches!(event.kind(), RuntimeEventKind::JournalAppended { .. }) })
    );
}

#[test]
fn local_app_persists_recovers_and_exposes_persistence_trace() {
    let dir = unique_dir("local-app-persistence");
    let snapshot = dir.join("state.snapshot");
    let journal = dir.join("state.journal");
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app = LocalApp::single_shard(PersistShard, TestMailboxFactory)
        .ingress_capacity(16)
        .trace_retention(TraceRetention::Bounded(512))
        .build();
    let address = app
        .register_root::<PersistService, Infallible>(
            PersistService::new(snapshot.clone(), journal.clone(), Arc::clone(&observed)),
            16,
        )
        .expect("register persistent root");
    app.try_send(address, PersistMsg::Mutate("hay".to_owned()))
        .expect("send hay");
    wait_until(|| {
        observed
            .lock()
            .expect("observed mutex")
            .contains(&"hay".to_owned())
    });
    app.try_send(address, PersistMsg::Mutate("oats".to_owned()))
        .expect("send oats");
    wait_until(|| {
        observed
            .lock()
            .expect("observed mutex")
            .contains(&"hay,oats".to_owned())
    });
    let terminal = app.shutdown().drain().join().expect("shutdown app");
    assert_eq!(terminal.state(), LocalAppState::Closed);
    assert!(
        terminal
            .trace()
            .iter()
            .any(|event| { matches!(event.kind(), RuntimeEventKind::JournalAppended { .. }) })
    );

    let fresh = LocalApp::single_shard(PersistShard, TestMailboxFactory)
        .ingress_capacity(16)
        .trace_retention(TraceRetention::Bounded(512))
        .build();
    let fresh_address = fresh
        .register_root::<PersistService, Infallible>(
            PersistService::new(snapshot, journal, Arc::clone(&observed)),
            16,
        )
        .expect("register fresh persistent root");
    fresh
        .try_send(fresh_address, PersistMsg::Recover)
        .expect("recover");
    wait_until(|| {
        observed
            .lock()
            .expect("observed mutex")
            .iter()
            .filter(|entry| entry.as_str() == "hay,oats")
            .count()
            >= 2
    });
    let fresh_terminal = fresh.shutdown().drain().join().expect("shutdown fresh app");
    assert_eq!(fresh_terminal.state(), LocalAppState::Closed);
    assert!(
        fresh_terminal
            .trace()
            .iter()
            .any(|event| { matches!(event.kind(), RuntimeEventKind::RecoveryFinished) })
    );
}
