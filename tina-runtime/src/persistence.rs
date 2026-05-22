//! Local persistence framing for Tina snapshot and journal helpers.
//!
//! This module owns bytes-on-disk shape. It does not serialize isolate state;
//! callers provide opaque payload bytes and Tina wraps only the metadata needed
//! for recovery.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{CallError, JournalRecord, JournalReplay, JournalReplayWarning, SnapshotImage};

const SNAPSHOT_MAGIC: &[u8; 8] = b"TNSNAP01";
const JOURNAL_MAGIC: &[u8; 8] = b"TNJRNL01";
const U64_BYTES: usize = 8;
const SNAPSHOT_HEADER_BYTES: usize = 8 + U64_BYTES + U64_BYTES + U64_BYTES;
const JOURNAL_HEADER_BYTES: usize = 8 + U64_BYTES + U64_BYTES + U64_BYTES;
const JOURNAL_INDEX_MAGIC: &[u8; 8] = b"TNJIDX01";
const JOURNAL_INDEX_BYTES: usize = 8 + U64_BYTES + U64_BYTES;
static SNAPSHOT_TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Support level for one local persistence safety step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceSupportLevel {
    /// Tina performs this step on this platform.
    Supported,
    /// Tina does not claim this step on this platform.
    NotClaimed,
}

/// Platform support table for Tina's local snapshot and journal helpers.
///
/// This names the actual file-system contract. Tina does not silently claim
/// durability properties that the current platform path does not provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalPersistenceSupport {
    /// Snapshot commits write a temporary file before renaming into place.
    pub temp_write_before_rename: PersistenceSupportLevel,
    /// Snapshot commits use an atomic filesystem rename where the platform
    /// provides that guarantee for the target filesystem.
    pub rename_commit: PersistenceSupportLevel,
    /// Snapshot and journal writes sync file contents before success.
    pub file_fsync: PersistenceSupportLevel,
    /// Snapshot commits sync the parent directory after rename.
    pub directory_fsync_after_rename: PersistenceSupportLevel,
    /// Journal replay treats a truncated tail as a visible warning.
    pub truncated_tail_warning: PersistenceSupportLevel,
    /// Snapshot and journal replay reject bad checksums as corrupt records.
    pub checksum_validation: PersistenceSupportLevel,
}

#[cfg(unix)]
const DIRECTORY_FSYNC_SUPPORT: PersistenceSupportLevel = PersistenceSupportLevel::Supported;

#[cfg(not(unix))]
const DIRECTORY_FSYNC_SUPPORT: PersistenceSupportLevel = PersistenceSupportLevel::NotClaimed;

#[cfg(unix)]
const RENAME_COMMIT_SUPPORT: PersistenceSupportLevel = PersistenceSupportLevel::Supported;

#[cfg(not(unix))]
const RENAME_COMMIT_SUPPORT: PersistenceSupportLevel = PersistenceSupportLevel::NotClaimed;

/// Local persistence support provided by this build.
pub const LOCAL_PERSISTENCE_SUPPORT: LocalPersistenceSupport = LocalPersistenceSupport {
    temp_write_before_rename: PersistenceSupportLevel::Supported,
    rename_commit: RENAME_COMMIT_SUPPORT,
    file_fsync: PersistenceSupportLevel::Supported,
    directory_fsync_after_rename: DIRECTORY_FSYNC_SUPPORT,
    truncated_tail_warning: PersistenceSupportLevel::Supported,
    checksum_validation: PersistenceSupportLevel::Supported,
};

/// Commits one snapshot to a local path using temp-write, data fsync, rename,
/// and parent-directory fsync where supported by the platform.
pub fn commit_snapshot(
    path: &Path,
    bytes: Vec<u8>,
    last_journal_index: u64,
) -> Result<(), CallError> {
    commit_snapshot_with_parent_sync(path, bytes, last_journal_index, sync_parent_directory)
}

fn commit_snapshot_with_parent_sync(
    path: &Path,
    bytes: Vec<u8>,
    last_journal_index: u64,
    sync_parent: impl FnOnce(&Path) -> Result<(), CallError>,
) -> Result<(), CallError> {
    let encoded = encode_snapshot(&SnapshotImage {
        bytes,
        last_journal_index,
    });
    let parent = parent_directory(path);
    fs::create_dir_all(parent).map_err(|_| CallError::Io)?;
    let temp_path = temp_snapshot_path(path);
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)
            .map_err(|_| CallError::Io)?;
        file.write_all(&encoded).map_err(|_| CallError::Io)?;
        file.sync_all().map_err(|_| CallError::Io)?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if fs::rename(&temp_path, path).is_err() {
        let _ = fs::remove_file(&temp_path);
        return Err(CallError::Io);
    }
    if sync_parent(parent).is_err() {
        return Err(CallError::CommitUncertain);
    }
    Ok(())
}

/// Atomically replaces a file with `bytes`: writes a temp file, fsyncs it,
/// renames into place, then fsyncs the parent directory where supported.
///
/// Returns [`CallError::CommitUncertain`] when the rename landed but the final
/// parent-directory fsync could not be confirmed — the durable state is then
/// unknown and the caller should recover from disk. Used to swap a compacted
/// outbox journal in one durable step.
pub fn commit_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), CallError> {
    commit_file_atomic_with_parent_sync(path, bytes, sync_parent_directory)
}

fn commit_file_atomic_with_parent_sync(
    path: &Path,
    bytes: &[u8],
    sync_parent: impl FnOnce(&Path) -> Result<(), CallError>,
) -> Result<(), CallError> {
    let parent = parent_directory(path);
    fs::create_dir_all(parent).map_err(|_| CallError::Io)?;
    let temp_path = temp_file_path(path);
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)
            .map_err(|_| CallError::Io)?;
        file.write_all(bytes).map_err(|_| CallError::Io)?;
        file.sync_all().map_err(|_| CallError::Io)?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if fs::rename(&temp_path, path).is_err() {
        let _ = fs::remove_file(&temp_path);
        return Err(CallError::Io);
    }
    if sync_parent(parent).is_err() {
        return Err(CallError::CommitUncertain);
    }
    Ok(())
}

/// Raises a durable commit fence before a commit whose final durability step
/// (the parent-directory fsync) could be interrupted by a crash.
///
/// The marker file is fsynced, and so is its parent directory, so the fence
/// itself survives a crash. Pair with [`clear_commit_fence`] after the commit's
/// final step confirms, and [`commit_fence_present`] on recovery: a leftover
/// fence means the last commit could not confirm, which maps to
/// [`crate::CommitConfidence::Uncertain`].
pub fn raise_commit_fence(path: &Path) -> Result<(), CallError> {
    let parent = parent_directory(path);
    fs::create_dir_all(parent).map_err(|_| CallError::Io)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|_| CallError::Io)?;
    file.sync_all().map_err(|_| CallError::Io)?;
    sync_parent_directory(parent)
}

/// Clears the commit fence after a commit's final durability step confirmed.
/// A missing fence is already cleared, so this is idempotent.
pub fn clear_commit_fence(path: &Path) -> Result<(), CallError> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(CallError::Io),
    }
    sync_parent_directory(parent_directory(path))
}

/// Whether a commit fence is present. `true` means a prior commit raised the
/// fence but never cleared it, so its durability is unconfirmed.
pub fn commit_fence_present(path: &Path) -> Result<bool, CallError> {
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(_) => Err(CallError::Io),
    }
}

/// Loads one snapshot, returning `Ok(None)` when no committed snapshot exists.
pub fn load_snapshot(path: &Path) -> Result<Option<SnapshotImage>, CallError> {
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(CallError::Io),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|_| CallError::Io)?;
    decode_snapshot(&bytes).map(Some)
}

/// Appends one framed journal record and fsyncs the journal file.
pub fn append_journal_record(
    path: &Path,
    record_index: u64,
    bytes: Vec<u8>,
) -> Result<(), CallError> {
    let parent = parent_directory(path);
    fs::create_dir_all(parent).map_err(|_| CallError::Io)?;
    validate_next_journal_index(path, record_index)?;
    let encoded = encode_journal_record(&JournalRecord {
        index: record_index,
        bytes,
    });
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|_| CallError::Io)?;
    file.write_all(&encoded).map_err(|_| CallError::Io)?;
    file.sync_all().map_err(|_| CallError::Io)?;
    let file_len = file.metadata().map_err(|_| CallError::Io)?.len();
    store_journal_last_index(path, record_index, file_len)?;
    Ok(())
}

/// Replays one journal from disk, treating a missing journal as empty.
pub fn replay_journal(path: &Path) -> Result<JournalReplay, CallError> {
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JournalReplay {
                records: Vec::new(),
                warning: None,
            });
        }
        Err(_) => return Err(CallError::Io),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|_| CallError::Io)?;
    replay_journal_bytes(&bytes)
}

fn validate_next_journal_index(path: &Path, record_index: u64) -> Result<(), CallError> {
    if let Some(last) = load_journal_last_index(path)? {
        if record_index <= last {
            return Err(CallError::CorruptRecord);
        }
        return Ok(());
    }
    let replay = replay_journal(path)?;
    if let Some(JournalReplayWarning::TruncatedTail { valid_prefix_len }) = replay.warning {
        repair_journal_tail(path, valid_prefix_len)?;
    }
    if let Some(last) = replay.records.last()
        && record_index <= last.index
    {
        return Err(CallError::CorruptRecord);
    }
    Ok(())
}

fn journal_index_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "{}idx",
        path.extension()
            .and_then(std::ffi::OsStr::to_str)
            .map(|ext| format!("{ext}."))
            .unwrap_or_default()
    ))
}

fn temp_journal_index_path(path: &Path) -> PathBuf {
    let n = SNAPSHOT_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!(
        "{}idx.tmp.{n}",
        path.extension()
            .and_then(std::ffi::OsStr::to_str)
            .map(|ext| format!("{ext}."))
            .unwrap_or_default()
    ))
}

fn load_journal_last_index(path: &Path) -> Result<Option<u64>, CallError> {
    let index_path = journal_index_path(path);
    let mut file = match OpenOptions::new().read(true).open(index_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(CallError::Io),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|_| CallError::Io)?;
    if bytes.len() != JOURNAL_INDEX_BYTES {
        return Ok(None);
    }
    if &bytes[..8] != JOURNAL_INDEX_MAGIC {
        return Ok(None);
    }
    let last_index = match read_u64(&bytes, 8) {
        Some(value) => value,
        None => return Ok(None),
    };
    let expected_len = match read_u64(&bytes, 16) {
        Some(value) => value,
        None => return Ok(None),
    };
    let actual_len = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(CallError::Io),
    };
    if actual_len != expected_len {
        return Ok(None);
    }
    Ok(Some(last_index))
}

fn store_journal_last_index(path: &Path, last_index: u64, file_len: u64) -> Result<(), CallError> {
    let index_path = journal_index_path(path);
    let temp_path = temp_journal_index_path(path);
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)
            .map_err(|_| CallError::Io)?;
        file.write_all(JOURNAL_INDEX_MAGIC)
            .map_err(|_| CallError::Io)?;
        file.write_all(&last_index.to_le_bytes())
            .map_err(|_| CallError::Io)?;
        file.write_all(&file_len.to_le_bytes())
            .map_err(|_| CallError::Io)?;
        file.sync_all().map_err(|_| CallError::Io)
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if fs::rename(&temp_path, &index_path).is_err() {
        let _ = fs::remove_file(&temp_path);
        return Err(CallError::Io);
    }
    if sync_parent_directory(parent_directory(&index_path)).is_err() {
        return Err(CallError::CommitUncertain);
    }
    Ok(())
}

fn repair_journal_tail(path: &Path, valid_prefix_len: u64) -> Result<(), CallError> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|_| CallError::Io)?;
    file.set_len(valid_prefix_len).map_err(|_| CallError::Io)
}

/// Encodes one snapshot image.
pub fn encode_snapshot(snapshot: &SnapshotImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(SNAPSHOT_HEADER_BYTES + snapshot.bytes.len());
    out.extend_from_slice(SNAPSHOT_MAGIC);
    out.extend_from_slice(&snapshot.last_journal_index.to_le_bytes());
    out.extend_from_slice(&(snapshot.bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(&checksum(&snapshot.bytes).to_le_bytes());
    out.extend_from_slice(&snapshot.bytes);
    out
}

/// Decodes one snapshot image.
pub fn decode_snapshot(bytes: &[u8]) -> Result<SnapshotImage, CallError> {
    if bytes.len() < SNAPSHOT_HEADER_BYTES || &bytes[..8] != SNAPSHOT_MAGIC {
        return Err(CallError::CorruptRecord);
    }
    let last_journal_index = read_u64(bytes, 8).ok_or(CallError::CorruptRecord)?;
    let payload_len = read_u64(bytes, 16).ok_or(CallError::CorruptRecord)? as usize;
    let expected_checksum = read_u64(bytes, 24).ok_or(CallError::CorruptRecord)?;
    let end = SNAPSHOT_HEADER_BYTES
        .checked_add(payload_len)
        .ok_or(CallError::CorruptRecord)?;
    if bytes.len() != end {
        return Err(CallError::CorruptRecord);
    }
    let payload = bytes[SNAPSHOT_HEADER_BYTES..end].to_vec();
    if checksum(&payload) != expected_checksum {
        return Err(CallError::CorruptRecord);
    }
    Ok(SnapshotImage {
        bytes: payload,
        last_journal_index,
    })
}

/// Encodes one journal record.
pub fn encode_journal_record(record: &JournalRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(JOURNAL_HEADER_BYTES + record.bytes.len());
    out.extend_from_slice(JOURNAL_MAGIC);
    out.extend_from_slice(&record.index.to_le_bytes());
    out.extend_from_slice(&(record.bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(&checksum(&record.bytes).to_le_bytes());
    out.extend_from_slice(&record.bytes);
    out
}

/// Replays journal bytes.
pub fn replay_journal_bytes(bytes: &[u8]) -> Result<JournalReplay, CallError> {
    let mut cursor = 0;
    let mut records = Vec::new();
    let mut warning = None;
    let mut last_index = None;

    while cursor < bytes.len() {
        if bytes.len() - cursor < JOURNAL_HEADER_BYTES {
            warning = Some(JournalReplayWarning::TruncatedTail {
                valid_prefix_len: cursor as u64,
            });
            break;
        }
        let header = &bytes[cursor..cursor + JOURNAL_HEADER_BYTES];
        if &header[..8] != JOURNAL_MAGIC {
            return Err(CallError::CorruptRecord);
        }
        let index = read_u64(header, 8).ok_or(CallError::CorruptRecord)?;
        let payload_len = read_u64(header, 16).ok_or(CallError::CorruptRecord)? as usize;
        let expected_checksum = read_u64(header, 24).ok_or(CallError::CorruptRecord)?;
        let payload_start = cursor + JOURNAL_HEADER_BYTES;
        let Some(payload_end) = payload_start.checked_add(payload_len) else {
            return Err(CallError::CorruptRecord);
        };
        if payload_end > bytes.len() {
            warning = Some(JournalReplayWarning::TruncatedTail {
                valid_prefix_len: cursor as u64,
            });
            break;
        }
        let payload = bytes[payload_start..payload_end].to_vec();
        if checksum(&payload) != expected_checksum {
            return Err(CallError::CorruptRecord);
        }
        if let Some(last) = last_index
            && index <= last
        {
            return Err(CallError::CorruptRecord);
        }
        records.push(JournalRecord {
            index,
            bytes: payload,
        });
        last_index = Some(index);
        cursor = payload_end;
    }

    Ok(JournalReplay { records, warning })
}

fn temp_snapshot_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("snapshot");
    let unique = SNAPSHOT_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        unique
    ))
}

fn temp_file_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("file");
    let unique = SNAPSHOT_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        unique
    ))
}

fn parent_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let chunk = bytes.get(offset..offset + U64_BYTES)?;
    let mut raw = [0; U64_BYTES];
    raw.copy_from_slice(chunk);
    Some(u64::from_le_bytes(raw))
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(unix)]
pub(crate) fn sync_parent_directory(path: &Path) -> Result<(), CallError> {
    let dir = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| CallError::Io)?;
    dir.sync_all().map_err(|_| CallError::Io)
}

#[cfg(not(unix))]
pub(crate) fn sync_parent_directory(_path: &Path) -> Result<(), CallError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static CURRENT_DIR_LOCK: Mutex<()> = Mutex::new(());

    struct CurrentDirGuard {
        old_dir: PathBuf,
    }

    impl CurrentDirGuard {
        fn enter(path: &Path) -> Self {
            let old_dir = std::env::current_dir().expect("current dir");
            std::env::set_current_dir(path).expect("enter temp dir");
            Self { old_dir }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.old_dir).expect("restore current dir");
        }
    }

    fn unique_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tina-persistence-{name}-{nanos}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn current_directory_paths_use_dot_parent() {
        let _guard = CURRENT_DIR_LOCK.lock().expect("cwd lock");
        let dir = unique_dir("current-dir");
        let _cwd = CurrentDirGuard::enter(&dir);

        commit_snapshot(Path::new("state.snapshot"), b"snapshot".to_vec(), 3).unwrap();
        append_journal_record(Path::new("state.journal"), 4, b"journal".to_vec()).unwrap();
        let snapshot = load_snapshot(Path::new("state.snapshot"))
            .unwrap()
            .expect("snapshot");
        let journal = replay_journal(Path::new("state.journal")).unwrap();

        assert_eq!(snapshot.bytes, b"snapshot");
        assert_eq!(snapshot.last_journal_index, 3);
        assert_eq!(journal.records.len(), 1);
        assert_eq!(journal.records[0].index, 4);
    }

    #[test]
    fn parent_sync_failure_after_rename_is_commit_uncertain() {
        let dir = unique_dir("commit-uncertain");
        let path = dir.join("state.snapshot");
        let result = commit_snapshot_with_parent_sync(&path, b"installed".to_vec(), 9, |_| {
            Err(CallError::Io)
        });

        assert_eq!(result, Err(CallError::CommitUncertain));
        let installed = load_snapshot(&path).unwrap().expect("installed snapshot");
        assert_eq!(installed.bytes, b"installed");
        assert_eq!(installed.last_journal_index, 9);
    }

    #[test]
    fn snapshot_rename_failure_removes_temp_file() {
        let dir = unique_dir("rename-cleanup");
        let path = dir.join("state.snapshot");
        fs::create_dir(&path).expect("directory blocks file rename");
        let result = commit_snapshot(&path, b"not-installed".to_vec(), 1);

        assert_eq!(result, Err(CallError::Io));
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files should be cleaned after rename failure: {leftovers:?}"
        );
    }

    #[test]
    fn commit_file_atomic_replaces_bytes_and_round_trips() {
        let dir = unique_dir("atomic-file");
        let path = dir.join("compacted.journal");
        commit_file_atomic(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");
        // a second commit fully replaces the contents
        commit_file_atomic(&path, b"second-longer").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second-longer");
    }

    #[test]
    fn commit_file_atomic_parent_sync_failure_is_uncertain_but_installed() {
        let dir = unique_dir("atomic-uncertain");
        let path = dir.join("compacted.journal");
        let result =
            commit_file_atomic_with_parent_sync(&path, b"installed", |_| Err(CallError::Io));
        assert_eq!(result, Err(CallError::CommitUncertain));
        // the rename landed; only the final durability step is unconfirmed.
        assert_eq!(fs::read(&path).unwrap(), b"installed");
    }

    #[test]
    fn commit_fence_lifecycle_is_present_then_cleared() {
        let dir = unique_dir("fence");
        let fence = dir.join("commit.fence");
        assert!(!commit_fence_present(&fence).unwrap());
        raise_commit_fence(&fence).unwrap();
        assert!(commit_fence_present(&fence).unwrap());
        clear_commit_fence(&fence).unwrap();
        assert!(!commit_fence_present(&fence).unwrap());
        // clearing an absent fence is idempotent.
        clear_commit_fence(&fence).unwrap();
    }

    #[test]
    fn leftover_commit_fence_reads_as_present_for_recovery() {
        let dir = unique_dir("fence-leftover");
        let fence = dir.join("commit.fence");
        raise_commit_fence(&fence).unwrap();
        // simulate a crash before the fence was cleared: a fresh read sees it.
        assert!(commit_fence_present(&fence).unwrap());
    }
}
