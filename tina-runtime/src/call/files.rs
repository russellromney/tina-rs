//! File and filesystem-path rail constructors.

use std::path::PathBuf;

use super::{
    CallInput, CallOutput, FileId, FileOpenOptions, PathMetadata, TypedCall, WriteOwnedError,
    WriteOwnedReply,
};

/// Returns a typed file-open helper.
pub fn file_open(path: impl Into<PathBuf>, options: FileOpenOptions) -> TypedCall<FileId> {
    TypedCall::new(
        CallInput::FileOpen {
            path: path.into(),
            options,
        },
        CallOutput::into_file_opened,
    )
}

/// Opens a file for common snapshot-style use: read/write, create if missing,
/// and truncate existing contents.
pub fn file_create(path: impl Into<PathBuf>) -> TypedCall<FileId> {
    file_open(path, FileOpenOptions::read_write_create_truncate())
}

/// Returns a typed positional file-read helper.
pub fn file_read_at(file: FileId, len: usize, offset: u64) -> TypedCall<Vec<u8>> {
    TypedCall::new(
        CallInput::FileReadAt { file, len, offset },
        CallOutput::into_file_read,
    )
}

/// Returns a typed file-read helper at offset 0.
pub fn file_read(file: FileId, len: usize) -> TypedCall<Vec<u8>> {
    file_read_at(file, len, 0)
}

/// Returns a typed positional file-write helper.
pub fn file_write_at(file: FileId, bytes: Vec<u8>, offset: u64) -> TypedCall<usize> {
    TypedCall::new(
        CallInput::FileWriteAt {
            file,
            bytes,
            offset,
        },
        CallOutput::into_file_wrote,
    )
}

/// Returns a positional file-write helper that gives the bytes back.
pub fn file_write_at_owned(
    file: FileId,
    bytes: Vec<u8>,
    offset: u64,
) -> TypedCall<WriteOwnedReply, WriteOwnedError> {
    TypedCall::new(
        CallInput::FileWriteAtOwned {
            file,
            bytes,
            offset,
            start: 0,
        },
        CallOutput::into_file_wrote_owned,
    )
}

pub(crate) fn file_write_at_owned_from(
    file: FileId,
    bytes: Vec<u8>,
    offset: u64,
    start: usize,
) -> TypedCall<WriteOwnedReply, WriteOwnedError> {
    TypedCall::new(
        CallInput::FileWriteAtOwned {
            file,
            bytes,
            offset,
            start,
        },
        CallOutput::into_file_wrote_owned,
    )
}

/// Returns a typed file-write helper at offset 0.
///
/// The completion still reports the number of bytes written; callers that need
/// full-write semantics should branch on that count and issue another
/// runtime-owned write if needed.
pub fn file_write(file: FileId, bytes: Vec<u8>) -> TypedCall<usize> {
    file_write_at(file, bytes, 0)
}

/// Returns a typed file fsync helper.
pub fn file_fsync(file: FileId) -> TypedCall<()> {
    TypedCall::new(CallInput::FileFsync { file }, CallOutput::into_file_synced)
}

/// Returns a typed file-size helper.
pub fn file_size(file: FileId) -> TypedCall<u64> {
    TypedCall::new(CallInput::FileSize { file }, CallOutput::into_file_size)
}

/// Returns a typed file-close helper.
pub fn file_close(file: FileId) -> TypedCall<()> {
    TypedCall::new(CallInput::FileClose { file }, CallOutput::into_file_closed)
}

/// Returns a typed directory-create helper.
pub fn mkdir(path: impl Into<PathBuf>, mode: u32) -> TypedCall<()> {
    TypedCall::new(
        CallInput::Mkdir {
            path: path.into(),
            mode,
        },
        CallOutput::into_directory_created,
    )
}

/// Returns a typed path-metadata helper.
pub fn path_metadata(path: impl Into<PathBuf>) -> TypedCall<PathMetadata> {
    TypedCall::new(
        CallInput::PathMetadata { path: path.into() },
        CallOutput::into_path_metadata,
    )
}

/// Returns a typed rename-replace helper.
pub fn rename_replace(from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> TypedCall<()> {
    TypedCall::new(
        CallInput::RenameReplace {
            from: from.into(),
            to: to.into(),
        },
        CallOutput::into_path_renamed,
    )
}

/// Returns a typed remove-file helper.
pub fn remove_file(path: impl Into<PathBuf>) -> TypedCall<()> {
    TypedCall::new(
        CallInput::RemoveFile { path: path.into() },
        CallOutput::into_file_removed,
    )
}

/// Returns a typed read-directory helper.
pub fn read_dir(path: impl Into<PathBuf>) -> TypedCall<Vec<PathBuf>> {
    TypedCall::new(
        CallInput::ReadDir { path: path.into() },
        CallOutput::into_directory_read,
    )
}

/// Returns a typed parent-directory sync helper.
pub fn sync_parent(path: impl Into<PathBuf>) -> TypedCall<()> {
    TypedCall::new(
        CallInput::SyncParent { path: path.into() },
        CallOutput::into_parent_synced,
    )
}
