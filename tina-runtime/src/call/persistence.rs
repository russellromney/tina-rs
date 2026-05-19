//! Local-persistence rail constructors (snapshot and journal helpers).

use std::path::PathBuf;

use super::{CallInput, CallOutput, JournalReplay, SnapshotImage, TypedCall};

/// Commits one snapshot with Tina's local persistence framing.
pub fn snapshot_commit(
    path: impl Into<PathBuf>,
    bytes: Vec<u8>,
    last_journal_index: u64,
) -> TypedCall<()> {
    TypedCall::new(
        CallInput::SnapshotCommit {
            path: path.into(),
            bytes,
            last_journal_index,
        },
        CallOutput::into_snapshot_committed,
    )
}

/// Loads one snapshot committed by [`snapshot_commit`].
pub fn snapshot_load(path: impl Into<PathBuf>) -> TypedCall<Option<SnapshotImage>> {
    TypedCall::new(
        CallInput::SnapshotLoad { path: path.into() },
        CallOutput::into_snapshot_loaded,
    )
}

/// Appends one domain record to a local journal.
pub fn journal_append(
    path: impl Into<PathBuf>,
    record_index: u64,
    bytes: Vec<u8>,
) -> TypedCall<()> {
    TypedCall::new(
        CallInput::JournalAppend {
            path: path.into(),
            record_index,
            bytes,
        },
        CallOutput::into_journal_appended,
    )
}

/// Replays one local journal committed by [`journal_append`].
pub fn journal_replay(path: impl Into<PathBuf>) -> TypedCall<JournalReplay> {
    TypedCall::new(
        CallInput::JournalReplay { path: path.into() },
        CallOutput::into_journal_replayed,
    )
}
