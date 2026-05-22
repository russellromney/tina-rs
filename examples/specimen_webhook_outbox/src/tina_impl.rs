//! Tina: the webhook outbox built on [`tina_runtime::DurableOutbox`].
//!
//! Record-before-send is a type rule — `apply` only accepts a `RecordedWork`,
//! which only a successful durable append produces. Recovery is typed
//! (`TailStatus`), compaction is one call (`recover_compacted`), the durable
//! swap is one call (`commit_file_atomic`) guarded by a commit fence, and the
//! resume loop is one call (`ResumeQueue::next_apply`). No hand-rolled byte
//! framing, checksums, or fsync decisions.

use std::path::Path;

use tempfile::TempDir;
use tina_runtime::{
    ApplyStatus, CommitConfidence, CompletionStart, DurableOutbox, RecordError, TailStatus, WorkId,
    persistence,
};

use crate::{Report, WEBHOOKS};

/// Map a runtime `CallError` (not a `std::error::Error`) into `anyhow`.
fn io<T>(result: Result<T, tina_runtime::CallError>) -> anyhow::Result<T> {
    result.map_err(|error| anyhow::anyhow!("persistence error: {error:?}"))
}

/// A fake webhook endpoint: records the payloads it received.
struct Sink {
    delivered: Vec<String>,
}

impl Sink {
    fn deliver(&mut self, payload: &[u8]) {
        self.delivered
            .push(String::from_utf8_lossy(payload).into_owned());
    }
}

pub fn run() -> anyhow::Result<Report> {
    let dir = TempDir::new()?;
    let journal = dir.path().join("webhooks.journal");
    let fence = dir.path().join("commit.fence");
    let mut sink = Sink {
        delivered: Vec::new(),
    };
    let mut report = Report::default();

    // --- Phase A: enqueue three, send + mark two, crash before marking the third.
    let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(64);
    for (position, hook) in WEBHOOKS.iter().enumerate() {
        // Stage and durably record before any send.
        let staged = outbox
            .enqueue(hook.as_bytes().to_vec())
            .map_err(|_| anyhow::anyhow!("outbox full"))?;
        io(persistence::append_journal_record(
            &journal,
            staged.journal_index(),
            staged.journal_bytes().to_vec(),
        ))?;
        let recorded = match outbox.record(staged, Ok(())) {
            Ok(recorded) => recorded,
            Err(RecordError::Append(failed)) => anyhow::bail!("append failed: {:?}", failed.error),
            Err(RecordError::Stale(_)) => anyhow::bail!("stale token"),
        };

        // Only a RecordedWork authorizes the send.
        let id = recorded.work_id();
        let payload = match outbox.apply(recorded) {
            ApplyStatus::Apply(payload) => payload,
            ApplyStatus::DuplicateWork(_) => continue,
        };
        sink.deliver(&payload);
        report.phase_a_sent += 1;

        // Mark sent durably — except the last, which "crashes" first.
        if position != WEBHOOKS.len() - 1 {
            mark_sent(&journal, &mut outbox, id)?;
            report.phase_a_marked += 1;
        }
    }
    report.journal_records_before_compaction =
        io(persistence::replay_journal(&journal))?.records.len() as u64;

    // The process is gone.
    drop(outbox);

    // --- Phase B: fresh process. Recover, compact, resume.
    let confidence =
        CommitConfidence::from_fence_present(io(persistence::commit_fence_present(&fence))?);
    let (mut outbox, recovery, compacted) = DurableOutbox::<Vec<u8>>::recover_compacted(
        64,
        persistence::replay_journal(&journal),
        confidence,
    )
    .map_err(|error| anyhow::anyhow!("recovery rejected: {error:?}"))?;
    report.exit_clean = matches!(recovery.tail_status, TailStatus::Clean);
    report.recovered_pending = recovery.pending.len() as u64;

    // Install the compacted journal in one durable step, fenced so an
    // interrupted swap would recover as uncertain instead of silently clean.
    io(persistence::raise_commit_fence(&fence))?;
    io(persistence::commit_file_atomic(&journal, &compacted))?;
    io(persistence::clear_commit_fence(&fence))?;
    report.journal_records_after_compaction =
        io(persistence::replay_journal(&journal))?.records.len() as u64;

    // Resume the unsent webhooks, oldest first. The third webhook is delivered
    // again — at-least-once, the honest outcome of a crash after send.
    let mut queue = recovery.into_resume();
    while let Some((id, payload)) = queue.next_apply(&mut outbox) {
        sink.deliver(&payload);
        report.phase_b_resent += 1;
        mark_sent(&journal, &mut outbox, id)?;
    }

    report.final_marked = report.phase_a_marked + report.phase_b_resent;
    Ok(report)
}

/// Durably mark one webhook sent: stage the completion, append it, confirm.
fn mark_sent(journal: &Path, outbox: &mut DurableOutbox<Vec<u8>>, id: WorkId) -> anyhow::Result<()> {
    match outbox.begin_complete(id) {
        CompletionStart::Record(completion) => {
            io(persistence::append_journal_record(
                journal,
                completion.journal_index(),
                completion.journal_bytes().to_vec(),
            ))?;
            outbox
                .finish_complete(completion, Ok(()))
                .map_err(|failed| anyhow::anyhow!("complete failed: {:?}", failed.error))?;
            Ok(())
        }
        CompletionStart::AlreadyCompleted(_) => Ok(()),
        CompletionStart::NotPending(id) => anyhow::bail!("not pending: {id:?}"),
    }
}
