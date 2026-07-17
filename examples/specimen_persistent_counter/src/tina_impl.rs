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
use std::time::Duration;

use tempfile::TempDir;
use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, DefaultThreadedMailboxFactory, JournalAppendReply, JournalReplay,
    JournalReplayReply, JournalReplayWarning, LocalPermitGate, LocalPermitName, LocalSystem,
    Permit, SnapshotCommitReply, SnapshotLoadReply, journal_append, journal_replay,
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
    Failed(CounterFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CounterFailure {
    Busy,
    SnapshotLoad(CallError),
    SnapshotDecode { actual_bytes: usize },
    JournalReplay(CallError),
    JournalTruncated { valid_prefix_len: u64 },
    JournalDecode { index: u64, actual_bytes: usize },
    JournalAppend(CallError),
    SnapshotCommit(CallError),
    JournalIndexOverflow,
    ValueOverflow,
}

#[derive(Debug)]
enum CounterEvent {
    SnapshotLoaded {
        req: RequestContext<CounterReply>,
        permit: Permit,
        result: SnapshotLoadReply,
    },
    JournalLoaded {
        req: RequestContext<CounterReply>,
        permit: Permit,
        recovered_value: u64,
        snapshot_index: u64,
        result: JournalReplayReply,
    },
    AppendDurable {
        req: RequestContext<CounterReply>,
        permit: Permit,
        index: u64,
        value: u64,
        result: JournalAppendReply,
    },
    SnapshotCommitted {
        req: RequestContext<CounterReply>,
        permit: Permit,
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
    operations: LocalPermitGate,
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
                permit,
                result: Ok(Some(snapshot)),
            } => {
                let Ok(recovered_value) = decode_u64(&snapshot.bytes) else {
                    self.finish_operation(permit);
                    return reply_to(
                        req,
                        CounterReply::Failed(CounterFailure::SnapshotDecode {
                            actual_bytes: snapshot.bytes.len(),
                        }),
                    );
                };
                let snapshot_index = snapshot.last_journal_index;
                journal_replay(self.journal_path.clone()).then_service_event(move |result| {
                    CounterEvent::JournalLoaded {
                        req,
                        permit,
                        recovered_value,
                        snapshot_index,
                        result,
                    }
                })
            }
            CounterEvent::SnapshotLoaded {
                req,
                permit,
                result: Ok(None),
            } => journal_replay(self.journal_path.clone()).then_service_event(move |result| {
                CounterEvent::JournalLoaded {
                    req,
                    permit,
                    recovered_value: 0,
                    snapshot_index: 0,
                    result,
                }
            }),
            CounterEvent::JournalLoaded {
                req,
                permit,
                recovered_value,
                snapshot_index,
                result: Ok(replay),
            } => {
                let recovered = recover_state(recovered_value, snapshot_index, &replay);
                self.finish_operation(permit);
                let (value, last_journal_index) = match recovered {
                    Ok(state) => state,
                    Err(error) => return reply_to(req, CounterReply::Failed(error)),
                };
                self.state.value = value;
                self.state.last_journal_index = last_journal_index;
                reply_to(req, self.ok_reply())
            }
            CounterEvent::AppendDurable {
                req,
                permit,
                index,
                value,
                result: Ok(()),
            } => {
                self.finish_operation(permit);
                self.state.value = value;
                self.state.last_journal_index = index;
                self.state.journal_records = self.state.journal_records.saturating_add(1);
                reply_to(req, self.ok_reply())
            }
            CounterEvent::SnapshotCommitted {
                req,
                permit,
                result: Ok(()),
            } => {
                self.finish_operation(permit);
                reply_to(req, self.ok_reply())
            }
            CounterEvent::SnapshotLoaded {
                req,
                permit,
                result: Err(error),
            } => {
                self.finish_operation(permit);
                reply_to(
                    req,
                    CounterReply::Failed(CounterFailure::SnapshotLoad(error)),
                )
            }
            CounterEvent::JournalLoaded {
                req,
                permit,
                result: Err(error),
                ..
            } => {
                self.finish_operation(permit);
                reply_to(
                    req,
                    CounterReply::Failed(CounterFailure::JournalReplay(error)),
                )
            }
            CounterEvent::AppendDurable {
                req,
                permit,
                result: Err(error),
                ..
            } => {
                self.finish_operation(permit);
                reply_to(
                    req,
                    CounterReply::Failed(CounterFailure::JournalAppend(error)),
                )
            }
            CounterEvent::SnapshotCommitted {
                req,
                permit,
                result: Err(error),
            } => {
                self.finish_operation(permit);
                reply_to(
                    req,
                    CounterReply::Failed(CounterFailure::SnapshotCommit(error)),
                )
            }
        }
    }

    fn handle_request(
        &mut self,
        request: CounterRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        let permit = match admit_operation(&mut self.operations) {
            Ok(permit) => permit,
            Err(error) => return call.reply(CounterReply::Failed(error)),
        };
        match request {
            CounterRequest::Recover => call.capture(|req| {
                snapshot_load(self.snapshot_path.clone()).then_service_event(move |result| {
                    CounterEvent::SnapshotLoaded {
                        req,
                        permit,
                        result,
                    }
                })
            }),
            CounterRequest::Increment => {
                let (next_index, next_value) = match next_increment(&self.state) {
                    Ok(next) => next,
                    Err(error) => {
                        self.finish_operation(permit);
                        return call.reply(CounterReply::Failed(error));
                    }
                };
                let path = self.journal_path.clone();
                call.capture(move |req| {
                    journal_append(path, next_index, encode_u64(next_value)).then_service_event(
                        move |result| CounterEvent::AppendDurable {
                            req,
                            permit,
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
                        move |result| CounterEvent::SnapshotCommitted {
                            req,
                            permit,
                            result,
                        },
                    )
                })
            }
        }
    }
}

impl Counter {
    fn finish_operation(&mut self, permit: Permit) {
        self.operations
            .release(permit)
            .expect("operation permit belongs to this counter");
    }

    fn ok_reply(&self) -> CounterReply {
        CounterReply::Ok {
            value: self.state.value,
            journal_records: self.state.journal_records,
        }
    }
}

fn admit_operation(operations: &mut LocalPermitGate) -> Result<Permit, CounterFailure> {
    operations.try_admit().map_err(|_| CounterFailure::Busy)
}

fn next_increment(state: &CounterState) -> Result<(u64, u64), CounterFailure> {
    let next_index = state
        .last_journal_index
        .checked_add(1)
        .ok_or(CounterFailure::JournalIndexOverflow)?;
    let next_value = state
        .value
        .checked_add(1)
        .ok_or(CounterFailure::ValueOverflow)?;
    Ok((next_index, next_value))
}

fn recover_state(
    snapshot_value: u64,
    snapshot_index: u64,
    replay: &JournalReplay,
) -> Result<(u64, u64), CounterFailure> {
    if let Some(JournalReplayWarning::TruncatedTail { valid_prefix_len }) = replay.warning {
        return Err(CounterFailure::JournalTruncated { valid_prefix_len });
    }
    let mut value = snapshot_value;
    let mut last_journal_index = snapshot_index;
    for record in &replay.records {
        if record.index > last_journal_index {
            value = decode_u64(&record.bytes).map_err(|actual_bytes| {
                CounterFailure::JournalDecode {
                    index: record.index,
                    actual_bytes,
                }
            })?;
            last_journal_index = record.index;
        }
    }
    Ok((value, last_journal_index))
}

fn encode_u64(value: u64) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

fn decode_u64(bytes: &[u8]) -> Result<u64, usize> {
    let arr: [u8; 8] = bytes.try_into().map_err(|_| bytes.len())?;
    Ok(u64::from_le_bytes(arr))
}

type CounterHandle = tina::ServiceRequestAddress<CounterEvent, CounterRequest, CounterReply>;

fn call_counter(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    counter: CounterHandle,
    request: CounterRequest,
) -> anyhow::Result<CounterReply> {
    match app.call_blocking_request(counter, request, OP_TIMEOUT)? {
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
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    app.run_to_shutdown_reported(Duration::from_secs(5), |app| {
        let counter = app
            .register_split_service::<Counter, CounterEvent, CounterRequest, Infallible>(
                Counter {
                    snapshot_path,
                    journal_path,
                    state: CounterState::default(),
                    operations: LocalPermitGate::with_capacity(1)
                        .named(LocalPermitName("counter.operation")),
                },
                16,
            )
            .map_err(|e| anyhow::anyhow!("register counter: {e:?}"))?
            .requests;

        let recovered = match call_counter(app, counter, CounterRequest::Recover)? {
            CounterReply::Ok { value, .. } => value,
            CounterReply::Failed(error) => anyhow::bail!("recover failed: {error:?}"),
        };

        let mut final_value = recovered;
        let mut journal_records_written = 0u64;
        for _ in 0..increments {
            match call_counter(app, counter, CounterRequest::Increment)? {
                CounterReply::Ok {
                    value,
                    journal_records,
                } => {
                    final_value = value;
                    journal_records_written = journal_records;
                }
                CounterReply::Failed(error) => anyhow::bail!("increment failed: {error:?}"),
            }
        }

        let mut snapshot_committed = false;
        if take_snapshot {
            match call_counter(app, counter, CounterRequest::CommitSnapshot)? {
                CounterReply::Ok { .. } => snapshot_committed = true,
                CounterReply::Failed(error) => {
                    anyhow::bail!("commit snapshot failed: {error:?}")
                }
            }
        }

        Ok(PhaseReport {
            recovered_value: recovered,
            final_value,
            snapshot_committed,
            journal_records_written,
        })
    })
    .map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn request_with_state(
        snapshot_path: PathBuf,
        journal_path: PathBuf,
        state: CounterState,
        request: CounterRequest,
    ) -> CounterReply {
        let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
            .try_build()
            .expect("system");
        let counter = app
            .register_split_service::<Counter, CounterEvent, CounterRequest, Infallible>(
                Counter {
                    snapshot_path,
                    journal_path,
                    state,
                    operations: LocalPermitGate::with_capacity(1)
                        .named(LocalPermitName("test.counter.operation")),
                },
                8,
            )
            .expect("register")
            .requests;
        let reply = match app
            .call_blocking_request(counter, request, Duration::from_secs(2))
            .expect("host call")
        {
            CallOutcome::Replied(reply) => reply,
            other => panic!("expected typed counter reply, got {other:?}"),
        };
        let terminal = app.shutdown().join_report();
        terminal.ensure_clean().expect("clean shutdown");
        reply
    }

    #[test]
    fn persisted_values_require_exactly_eight_bytes() {
        assert_eq!(decode_u64(&7_u64.to_le_bytes()), Ok(7));
        assert_eq!(decode_u64(&[]), Err(0));
        assert_eq!(decode_u64(&[0; 7]), Err(7));
        assert_eq!(decode_u64(&[0; 9]), Err(9));
    }

    #[test]
    fn recovery_is_transactional_and_reports_exact_corruption() {
        let corrupt = JournalReplay {
            records: vec![tina_runtime::JournalRecord {
                index: 3,
                bytes: vec![0; 7],
            }],
            warning: None,
        };
        assert_eq!(
            recover_state(41, 2, &corrupt),
            Err(CounterFailure::JournalDecode {
                index: 3,
                actual_bytes: 7,
            })
        );

        let truncated = JournalReplay {
            records: vec![tina_runtime::JournalRecord {
                index: 3,
                bytes: encode_u64(42),
            }],
            warning: Some(JournalReplayWarning::TruncatedTail {
                valid_prefix_len: 48,
            }),
        };
        assert_eq!(
            recover_state(41, 2, &truncated),
            Err(CounterFailure::JournalTruncated {
                valid_prefix_len: 48,
            })
        );
    }

    #[test]
    fn increment_overflow_is_exact_and_does_not_wrap() {
        assert_eq!(
            next_increment(&CounterState {
                value: 1,
                last_journal_index: u64::MAX,
                journal_records: 0,
            }),
            Err(CounterFailure::JournalIndexOverflow)
        );
        assert_eq!(
            next_increment(&CounterState {
                value: u64::MAX,
                last_journal_index: 1,
                journal_records: 0,
            }),
            Err(CounterFailure::ValueOverflow)
        );
    }

    #[test]
    fn overlapping_operation_is_rejected_without_losing_gate_authority() {
        let mut operations = LocalPermitGate::with_capacity(1)
            .named(LocalPermitName("test.counter.operation"));
        let in_flight = admit_operation(&mut operations).expect("first operation admitted");
        assert!(matches!(
            admit_operation(&mut operations),
            Err(CounterFailure::Busy)
        ));
        operations
            .release(in_flight)
            .expect("original operation releases exactly once");
        let next = admit_operation(&mut operations).expect("capacity restored after settlement");
        operations.release(next).expect("next operation settles");
    }

    #[test]
    fn live_recovery_surfaces_framing_payload_and_truncation_distinctly() {
        let dir = TempDir::new().expect("tempdir");
        let snapshot = dir.path().join("counter.snap");
        let journal = dir.path().join("counter.journal");

        std::fs::write(&snapshot, b"not-a-snapshot").expect("write corrupt snapshot");
        assert_eq!(
            request_with_state(
                snapshot.clone(),
                journal.clone(),
                CounterState::default(),
                CounterRequest::Recover,
            ),
            CounterReply::Failed(CounterFailure::SnapshotLoad(CallError::CorruptRecord))
        );

        tina_runtime::persistence::commit_snapshot(&snapshot, vec![0; 7], 0)
            .expect("commit domain-corrupt snapshot");
        assert_eq!(
            request_with_state(
                snapshot.clone(),
                journal.clone(),
                CounterState::default(),
                CounterRequest::Recover,
            ),
            CounterReply::Failed(CounterFailure::SnapshotDecode { actual_bytes: 7 })
        );

        tina_runtime::persistence::commit_snapshot(&snapshot, encode_u64(0), 0)
            .expect("commit snapshot");
        std::fs::write(&journal, [0; 32]).expect("write corrupt journal");
        assert_eq!(
            request_with_state(
                snapshot.clone(),
                journal.clone(),
                CounterState::default(),
                CounterRequest::Recover,
            ),
            CounterReply::Failed(CounterFailure::JournalReplay(CallError::CorruptRecord))
        );

        std::fs::remove_file(&journal).expect("remove corrupt journal");
        tina_runtime::persistence::append_journal_record(&journal, 1, vec![0; 7])
            .expect("append domain-corrupt record");
        assert_eq!(
            request_with_state(
                snapshot.clone(),
                journal.clone(),
                CounterState::default(),
                CounterRequest::Recover,
            ),
            CounterReply::Failed(CounterFailure::JournalDecode {
                index: 1,
                actual_bytes: 7,
            })
        );

        std::fs::remove_file(&journal).expect("remove domain-corrupt journal");
        tina_runtime::persistence::append_journal_record(&journal, 1, encode_u64(1))
            .expect("append valid record");
        let valid_prefix_len = std::fs::metadata(&journal).expect("metadata").len();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .expect("open journal")
            .write_all(b"partial")
            .expect("append partial tail");
        assert_eq!(
            request_with_state(
                snapshot,
                journal,
                CounterState::default(),
                CounterRequest::Recover,
            ),
            CounterReply::Failed(CounterFailure::JournalTruncated { valid_prefix_len })
        );
    }

    #[test]
    fn live_write_failures_and_overflow_keep_exact_stage_identity() {
        let dir = TempDir::new().expect("tempdir");
        let snapshot = dir.path().join("counter.snap");
        let journal = dir.path().join("counter.journal");
        std::fs::create_dir(&snapshot).expect("snapshot directory");
        std::fs::create_dir(&journal).expect("journal directory");

        assert_eq!(
            request_with_state(
                snapshot.clone(),
                journal.clone(),
                CounterState::default(),
                CounterRequest::Increment,
            ),
            CounterReply::Failed(CounterFailure::JournalAppend(CallError::Io))
        );
        assert_eq!(
            request_with_state(
                snapshot.clone(),
                journal.clone(),
                CounterState::default(),
                CounterRequest::CommitSnapshot,
            ),
            CounterReply::Failed(CounterFailure::SnapshotCommit(CallError::Io))
        );
        assert_eq!(
            request_with_state(
                snapshot,
                journal,
                CounterState {
                    value: u64::MAX,
                    last_journal_index: 1,
                    journal_records: 0,
                },
                CounterRequest::Increment,
            ),
            CounterReply::Failed(CounterFailure::ValueOverflow)
        );
    }
}
