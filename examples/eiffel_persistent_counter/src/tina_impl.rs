//! Tina: a `Counter` isolate using runtime-owned snapshot + journal
//! primitives. Recovery, increments, and snapshots are all sequences
//! of `journal_*` / `snapshot_*` runtime calls — no hand-rolled byte
//! framing, no fsync decisions per call site.
//!
//! The host correlates "did my op finish?" via a `u64` op id
//! threaded through every continuation message and read back from a
//! shared `Observation` slot. App-specific data the runtime can't
//! know about; `FINDINGS.md` tracks this as typed isolate result
//! waiter work.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, JournalAppendReply, JournalReplayReply, SnapshotCommitReply,
    SnapshotLoadReply, ThreadedRuntime, journal_append, journal_replay, snapshot_commit,
    snapshot_load,
};

use crate::{PHASE_A_INCREMENTS, PHASE_B_INCREMENTS, Report};

/// Side channel between the Counter isolate and the host. Each
/// completed op writes its id, value, and journal index here. Host
/// loops on `last_completed_op` to know an op finished.
#[derive(Debug, Default)]
struct Observation {
    last_completed_op: AtomicU64,
    last_value: AtomicU64,
    journal_records_observed: AtomicU64,
}

#[derive(Debug, Clone)]
enum CounterMsg {
    Recover {
        op: u64,
    },
    SnapshotLoaded {
        op: u64,
        result: SnapshotLoadReply,
    },
    JournalLoaded {
        op: u64,
        result: JournalReplayReply,
    },
    Increment {
        op: u64,
    },
    AppendDurable {
        op: u64,
        index: u64,
        value: u64,
        result: JournalAppendReply,
    },
    CommitSnapshot {
        op: u64,
    },
    SnapshotCommitted {
        op: u64,
        result: SnapshotCommitReply,
    },
}

/// Persisted state: zero on construction, recovered from disk by
/// the first `Recover` op. Lives in its own `Default`-derived
/// sub-struct so the spawn site doesn't manually zero each field.
#[derive(Debug, Default)]
struct CounterState {
    value: u64,
    last_journal_index: u64,
}

struct Counter {
    snapshot_path: PathBuf,
    journal_path: PathBuf,
    observation: Arc<Observation>,
    state: CounterState,
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
                self.state.value = decode_u64(&snapshot.bytes);
                self.state.last_journal_index = snapshot.last_journal_index;
                journal_replay(self.journal_path.clone())
                    .reply(move |result| CounterMsg::JournalLoaded { op, result })
            }
            CounterMsg::SnapshotLoaded {
                op,
                result: Ok(None),
            } => journal_replay(self.journal_path.clone())
                .reply(move |result| CounterMsg::JournalLoaded { op, result }),
            CounterMsg::JournalLoaded {
                op,
                result: Ok(replay),
            } => {
                for record in replay.records {
                    if record.index > self.state.last_journal_index {
                        self.state.value = decode_u64(&record.bytes);
                        self.state.last_journal_index = record.index;
                    }
                }
                self.publish(op);
                noop()
            }
            CounterMsg::Increment { op } => {
                let next_index = self.state.last_journal_index + 1;
                let next_value = self.state.value + 1;
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
                self.state.value = value;
                self.state.last_journal_index = index;
                self.observation
                    .journal_records_observed
                    .fetch_add(1, Ordering::Relaxed);
                self.publish(op);
                noop()
            }
            CounterMsg::CommitSnapshot { op } => {
                let last_index = self.state.last_journal_index;
                let value = self.state.value;
                snapshot_commit(self.snapshot_path.clone(), encode_u64(value), last_index)
                    .reply(move |result| CounterMsg::SnapshotCommitted { op, result })
            }
            CounterMsg::SnapshotCommitted { op, result: Ok(()) } => {
                self.publish(op);
                noop()
            }
            // All Err arms (and the snapshot-load Err arm) just publish so
            // the host's wait_op completes; the failure is observable via
            // the unchanged `last_value` if the caller cares.
            CounterMsg::SnapshotLoaded { op, result: Err(_) }
            | CounterMsg::JournalLoaded { op, result: Err(_) }
            | CounterMsg::AppendDurable {
                op, result: Err(_), ..
            }
            | CounterMsg::SnapshotCommitted { op, result: Err(_) } => {
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
            .store(self.state.value, Ordering::Relaxed);
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

pub fn run() -> anyhow::Result<Report> {
    let dir = TempDir::new()?;
    let snapshot_path = dir.path().join("counter.snap");
    let journal_path = dir.path().join("counter.journal");

    // Phase A: fresh dir, increments, snapshot.
    let phase_a = run_phase(
        snapshot_path.clone(),
        journal_path.clone(),
        PHASE_A_INCREMENTS,
        true,
    )?;

    // Simulated process restart: tear down the runtime and rebuild
    // against the same data files.
    let phase_b = run_phase(snapshot_path, journal_path, PHASE_B_INCREMENTS, false)?;

    Ok(Report {
        phase_a_final: phase_a.final_value,
        phase_b_recovered: phase_b.recovered_value,
        phase_b_final: phase_b.final_value,
        snapshot_committed: phase_a.snapshot_committed,
        journal_records_phase_b: phase_b.journal_records_written,
        exit_clean: true,
    })
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
) -> anyhow::Result<PhaseReport> {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let observation = Arc::new(Observation::default());
    let counter = runtime
        .register_with_capacity::<_, Infallible>(
            Counter {
                snapshot_path,
                journal_path,
                observation: Arc::clone(&observation),
                state: CounterState::default(),
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register counter: {e:?}"))?;

    let mut next_op = 1u64;
    let wait_op = |runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
                   msg: CounterMsg,
                   op: u64|
     -> anyhow::Result<()> {
        runtime
            .try_send(counter, msg)
            .map_err(|e| anyhow::anyhow!("send op: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while observation.last_completed_op.load(Ordering::Acquire) < op {
            if Instant::now() > deadline {
                anyhow::bail!("timed out waiting for op {op}");
            }
            thread::yield_now();
        }
        Ok(())
    };

    let op = next_op;
    next_op += 1;
    wait_op(&runtime, CounterMsg::Recover { op }, op)?;
    let recovered_value = observation.last_value.load(Ordering::Relaxed);

    for _ in 0..increments {
        let op = next_op;
        next_op += 1;
        wait_op(&runtime, CounterMsg::Increment { op }, op)?;
    }
    let final_value = observation.last_value.load(Ordering::Relaxed);

    let mut snapshot_committed = false;
    if take_snapshot {
        let op = next_op;
        wait_op(&runtime, CounterMsg::CommitSnapshot { op }, op)?;
        snapshot_committed = true;
    }
    let journal_records_written = observation.journal_records_observed.load(Ordering::Relaxed);

    let _ = runtime.shutdown();

    Ok(PhaseReport {
        recovered_value,
        final_value,
        snapshot_committed,
        journal_records_written,
    })
}
