//! Tina: a `Counter` isolate using runtime-owned snapshot + journal
//! primitives. Recovery, increments, and snapshots are all sequences
//! of `journal_*` / `snapshot_*` runtime calls — no hand-rolled byte
//! framing, no fsync decisions per call site.
//!
//! Each host op is a typed request. The isolate privately sequences the
//! IO continuations and replies with the current value when the op
//! settles. No shared observation slot, no host spin loop.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, JournalAppendReply, JournalReplayReply,
    SnapshotCommitReply, SnapshotLoadReply, ThreadedRuntime, journal_append, journal_replay,
    snapshot_commit, snapshot_load,
};

use crate::{PHASE_A_INCREMENTS, PHASE_B_INCREMENTS, Report};

const OP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CounterRequest {
    Recover,
    Increment,
    CommitSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CounterReply {
    /// Op settled; `value` is the counter after the op, `journal_records`
    /// is how many durable appends this isolate has completed so far.
    Ok {
        value: u64,
        journal_records: u64,
    },
    Failed,
}

#[derive(Debug)]
enum CounterEvent {
    SnapshotLoaded {
        req: RequestContext<CounterReply>,
        result: SnapshotLoadReply,
    },
    JournalLoaded {
        req: RequestContext<CounterReply>,
        result: JournalReplayReply,
    },
    AppendDurable {
        req: RequestContext<CounterReply>,
        index: u64,
        value: u64,
        result: JournalAppendReply,
    },
    SnapshotCommitted {
        req: RequestContext<CounterReply>,
        result: SnapshotCommitReply,
    },
}

/// Persisted state: zero on construction, recovered from disk by
/// the first `Recover` op.
#[derive(Debug, Default)]
struct CounterState {
    value: u64,
    last_journal_index: u64,
    journal_records: u64,
}

struct Counter {
    snapshot_path: PathBuf,
    journal_path: PathBuf,
    state: CounterState,
}

#[tina_runtime::isolate(event = CounterEvent, request = CounterRequest, reply = CounterReply)]
impl Counter {
    fn handle_event(
        &mut self,
        event: CounterEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            CounterEvent::SnapshotLoaded {
                req,
                result: Ok(Some(snapshot)),
            } => {
                self.state.value = decode_u64(&snapshot.bytes);
                self.state.last_journal_index = snapshot.last_journal_index;
                journal_replay(self.journal_path.clone()).then_service_event(move |result| {
                    CounterEvent::JournalLoaded { req, result }
                })
            }
            CounterEvent::SnapshotLoaded {
                req,
                result: Ok(None),
            } => journal_replay(self.journal_path.clone())
                .then_service_event(move |result| CounterEvent::JournalLoaded { req, result }),
            CounterEvent::JournalLoaded {
                req,
                result: Ok(replay),
            } => {
                for record in replay.records {
                    if record.index > self.state.last_journal_index {
                        self.state.value = decode_u64(&record.bytes);
                        self.state.last_journal_index = record.index;
                    }
                }
                reply_to(req, self.ok_reply())
            }
            CounterEvent::AppendDurable {
                req,
                index,
                value,
                result: Ok(()),
            } => {
                self.state.value = value;
                self.state.last_journal_index = index;
                self.state.journal_records += 1;
                reply_to(req, self.ok_reply())
            }
            CounterEvent::SnapshotCommitted {
                req,
                result: Ok(()),
            } => reply_to(req, self.ok_reply()),
            CounterEvent::SnapshotLoaded { req, result: Err(_) }
            | CounterEvent::JournalLoaded { req, result: Err(_) }
            | CounterEvent::AppendDurable {
                req, result: Err(_), ..
            }
            | CounterEvent::SnapshotCommitted { req, result: Err(_) } => {
                reply_to(req, CounterReply::Failed)
            }
        }
    }

    fn handle_request(
        &mut self,
        request: CounterRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            CounterRequest::Recover => call.capture(|req| {
                snapshot_load(self.snapshot_path.clone()).then_service_event(move |result| {
                    CounterEvent::SnapshotLoaded { req, result }
                })
            }),
            CounterRequest::Increment => {
                let next_index = self.state.last_journal_index + 1;
                let next_value = self.state.value + 1;
                let path = self.journal_path.clone();
                call.capture(move |req| {
                    journal_append(path, next_index, encode_u64(next_value)).then_service_event(
                        move |result| CounterEvent::AppendDurable {
                            req,
                            index: next_index,
                            value: next_value,
                            result,
                        },
                    )
                })
            }
            CounterRequest::CommitSnapshot => {
                let last_index = self.state.last_journal_index;
                let value = self.state.value;
                let path = self.snapshot_path.clone();
                call.capture(move |req| {
                    snapshot_commit(path, encode_u64(value), last_index).then_service_event(
                        move |result| CounterEvent::SnapshotCommitted { req, result },
                    )
                })
            }
        }
    }
}

impl Counter {
    fn ok_reply(&self) -> CounterReply {
        CounterReply::Ok {
            value: self.state.value,
            journal_records: self.state.journal_records,
        }
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

type CounterHandle = tina::ServiceRequestAddress<CounterEvent, CounterRequest, CounterReply>;

fn call_counter(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    counter: CounterHandle,
    request: CounterRequest,
) -> anyhow::Result<CounterReply> {
    match runtime.call_blocking_request(counter, request, OP_TIMEOUT)? {
        CallOutcome::Replied(reply) => Ok(reply),
        other => anyhow::bail!("counter call {request:?} failed: {other:?}"),
    }
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
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();

    let counter = runtime
        .register_split_service::<Counter, CounterEvent, CounterRequest, Infallible>(
            Counter {
                snapshot_path,
                journal_path,
                state: CounterState::default(),
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register counter: {e:?}"))?
        .requests;

    let recovered = match call_counter(&runtime, counter, CounterRequest::Recover)? {
        CounterReply::Ok { value, .. } => value,
        CounterReply::Failed => anyhow::bail!("recover failed"),
    };

    let mut final_value = recovered;
    let mut journal_records_written = 0u64;
    for _ in 0..increments {
        match call_counter(&runtime, counter, CounterRequest::Increment)? {
            CounterReply::Ok {
                value,
                journal_records,
            } => {
                final_value = value;
                journal_records_written = journal_records;
            }
            CounterReply::Failed => anyhow::bail!("increment failed"),
        }
    }

    let mut snapshot_committed = false;
    if take_snapshot {
        match call_counter(&runtime, counter, CounterRequest::CommitSnapshot)? {
            CounterReply::Ok { .. } => snapshot_committed = true,
            CounterReply::Failed => anyhow::bail!("commit snapshot failed"),
        }
    }

    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;

    Ok(PhaseReport {
        recovered_value: recovered,
        final_value,
        snapshot_committed,
        journal_records_written,
    })
}
