use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tina::prelude::*;
use tina_runtime::{
    CallError, DefaultThreadedMailboxFactory, JournalReplay, SnapshotImage, ThreadedRuntime,
    ThreadedRuntimeConfig, journal_append, journal_replay, snapshot_commit, snapshot_load,
};

use super::{PHASE_A_INCREMENTS, PHASE_B_INCREMENTS, SideReport};

// Shared observation slot — the driver thread reads each completed op id
// here so it can tell when an Increment / Snapshot / Recover has actually
// finished on the counter thread. Same shape as `BoundAddr` from earlier
// comparisons.
#[derive(Default)]
struct Observation {
    last_completed_op: AtomicU64,
    last_value: AtomicU64,
    last_journal_index: AtomicU64,
    journal_records_observed: AtomicU64,
}

#[derive(Debug, Clone)]
enum CounterMsg {
    Recover {
        op: u64,
    },
    SnapshotLoaded {
        op: u64,
        result: Result<Option<SnapshotImage>, CallError>,
    },
    JournalLoaded {
        op: u64,
        result: Result<JournalReplay, CallError>,
    },
    Increment {
        op: u64,
    },
    AppendDurable {
        op: u64,
        index: u64,
        value: u64,
        result: Result<(), CallError>,
    },
    CommitSnapshot {
        op: u64,
    },
    SnapshotCommitted {
        op: u64,
        result: Result<(), CallError>,
    },
}

struct Counter {
    snapshot_path: PathBuf,
    journal_path: PathBuf,
    observation: Arc<Observation>,
    value: u64,
    last_journal_index: u64,
}

#[tina_runtime::isolate(message = CounterMsg)]
impl Counter {
    fn handle(&mut self, msg: CounterMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            CounterMsg::Recover { op } => snapshot_load(self.snapshot_path.clone())
                .reply(move |result| CounterMsg::SnapshotLoaded { op, result }),
            CounterMsg::SnapshotLoaded {
                op,
                result: Ok(Some(snapshot)),
            } => {
                self.value = decode_u64(&snapshot.bytes);
                self.last_journal_index = snapshot.last_journal_index;
                journal_replay(self.journal_path.clone())
                    .reply(move |result| CounterMsg::JournalLoaded { op, result })
            }
            CounterMsg::SnapshotLoaded {
                op,
                result: Ok(None),
            } => journal_replay(self.journal_path.clone())
                .reply(move |result| CounterMsg::JournalLoaded { op, result }),
            CounterMsg::SnapshotLoaded { op, result: Err(_) } => {
                self.publish(op);
                noop()
            }
            CounterMsg::JournalLoaded {
                op,
                result: Ok(replay),
            } => {
                for record in replay.records {
                    if record.index > self.last_journal_index {
                        self.value = decode_u64(&record.bytes);
                        self.last_journal_index = record.index;
                    }
                }
                self.publish(op);
                noop()
            }
            CounterMsg::JournalLoaded { op, result: Err(_) } => {
                self.publish(op);
                noop()
            }
            CounterMsg::Increment { op } => {
                let next_index = self.last_journal_index + 1;
                let next_value = self.value + 1;
                journal_append(
                    self.journal_path.clone(),
                    next_index,
                    encode_u64(next_value),
                )
                .reply(move |result| CounterMsg::AppendDurable {
                    op,
                    index: next_index,
                    value: next_value,
                    result,
                })
            }
            CounterMsg::AppendDurable {
                op,
                index,
                value,
                result: Ok(()),
            } => {
                self.value = value;
                self.last_journal_index = index;
                self.observation
                    .journal_records_observed
                    .fetch_add(1, Ordering::Relaxed);
                self.publish(op);
                noop()
            }
            CounterMsg::AppendDurable {
                op, result: Err(_), ..
            } => {
                self.publish(op);
                noop()
            }
            CounterMsg::CommitSnapshot { op } => {
                let last_index = self.last_journal_index;
                let value = self.value;
                snapshot_commit(self.snapshot_path.clone(), encode_u64(value), last_index)
                    .reply(move |result| CounterMsg::SnapshotCommitted { op, result })
            }
            CounterMsg::SnapshotCommitted { op, result: Ok(()) } => {
                self.publish(op);
                noop()
            }
            CounterMsg::SnapshotCommitted { op, result: Err(_) } => {
                self.publish(op);
                noop()
            }
        }
    }
}

impl Counter {
    fn publish(&self, op: u64) {
        self.observation
            .last_value
            .store(self.value, Ordering::Relaxed);
        self.observation
            .last_journal_index
            .store(self.last_journal_index, Ordering::Relaxed);
        self.observation
            .last_completed_op
            .store(op, Ordering::Release);
    }
}

fn encode_u64(value: u64) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

fn decode_u64(bytes: &[u8]) -> u64 {
    let arr: [u8; 8] = bytes
        .try_into()
        .expect("counter snapshot/journal payload is 8 bytes");
    u64::from_le_bytes(arr)
}

pub(crate) fn run() -> SideReport {
    let dir = TempDir::new().expect("tempdir");
    let snapshot_path = dir.path().join("counter.snap");
    let journal_path = dir.path().join("counter.journal");

    // Phase A
    let phase_a = run_phase(
        snapshot_path.clone(),
        journal_path.clone(),
        PHASE_A_INCREMENTS,
        true,
    );

    // Simulated process restart: tear down the whole runtime, then build a
    // fresh one against the same data files. This is the apples-to-apples
    // shape against the Tokio side dropping its in-memory counter and
    // re-reading from disk.
    let phase_b = run_phase(
        snapshot_path.clone(),
        journal_path.clone(),
        PHASE_B_INCREMENTS,
        false,
    );

    SideReport {
        phase_a_final: phase_a.final_value,
        phase_b_recovered: phase_b.recovered_value,
        phase_b_final: phase_b.final_value,
        snapshot_committed: phase_a.snapshot_committed,
        journal_records_phase_b: phase_b.journal_records_written,
        exit_clean: true,
    }
}

struct PhaseReport {
    recovered_value: u64,
    final_value: u64,
    snapshot_committed: bool,
    journal_records_written: u64,
}

fn run_phase(
    snapshot_path: PathBuf,
    journal_path: PathBuf,
    increments: u64,
    take_snapshot: bool,
) -> PhaseReport {
    let runtime = ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let observation = Arc::new(Observation::default());
    let counter = runtime
        .register_with_capacity::<Counter, _>(
            Counter {
                snapshot_path,
                journal_path,
                observation: Arc::clone(&observation),
                value: 0,
                last_journal_index: 0,
            },
            16,
        )
        .expect("register counter");

    let mut next_op = 1u64;
    let wait_op = |runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
                   msg: CounterMsg,
                   op: u64| {
        runtime.try_send(counter, msg).expect("send op");
        let deadline = Instant::now() + Duration::from_secs(5);
        while observation.last_completed_op.load(Ordering::Acquire) < op {
            if Instant::now() > deadline {
                panic!("timed out waiting for op {op}");
            }
            thread::yield_now();
        }
    };

    // Recover.
    let op = next_op;
    next_op += 1;
    wait_op(&runtime, CounterMsg::Recover { op }, op);
    let recovered_value = observation.last_value.load(Ordering::Relaxed);

    // Apply increments.
    for _ in 0..increments {
        let op = next_op;
        next_op += 1;
        wait_op(&runtime, CounterMsg::Increment { op }, op);
    }
    let final_value = observation.last_value.load(Ordering::Relaxed);

    // Optional snapshot.
    let mut snapshot_committed = false;
    if take_snapshot {
        let op = next_op;
        wait_op(&runtime, CounterMsg::CommitSnapshot { op }, op);
        snapshot_committed = true;
    }

    let journal_records_written = observation.journal_records_observed.load(Ordering::Relaxed);

    let _ = runtime.shutdown().expect("runtime shutdown");

    PhaseReport {
        recovered_value,
        final_value,
        snapshot_committed,
        journal_records_written,
    }
}
