//! Storage call lane: snapshot/journal/filesystem work that runs
//! either inline or on a bounded worker thread.

use super::*;

pub(super) enum StorageLane {
    Inline,
    Worker(StorageWorkerLane),
}

pub(super) struct StorageWorkerLane {
    capacity: usize,
    sender: Option<SyncSender<StorageCommand>>,
    completions: Receiver<StorageCompletion>,
    handle: Option<JoinHandle<()>>,
    pending: Vec<StoragePending>,
}

struct StoragePending {
    call_id: CallId,
    cancelled: Arc<AtomicBool>,
}

struct StorageCommand {
    call_id: CallId,
    job: StorageJob,
    cancelled: Arc<AtomicBool>,
}

pub(super) enum StorageJob {
    SnapshotCommit {
        path: PathBuf,
        bytes: Vec<u8>,
        last_journal_index: u64,
    },
    SnapshotLoad {
        path: PathBuf,
    },
    JournalAppend {
        path: PathBuf,
        record_index: u64,
        bytes: Vec<u8>,
    },
    JournalReplay {
        path: PathBuf,
    },
    PathMetadata {
        path: PathBuf,
    },
    RenameReplace {
        from: PathBuf,
        to: PathBuf,
    },
    RemoveFile {
        path: PathBuf,
    },
    ReadDir {
        path: PathBuf,
    },
    SyncParent {
        path: PathBuf,
    },
    #[cfg(test)]
    Park {
        started: SyncSender<()>,
        release: Receiver<()>,
    },
}

struct StorageCompletion {
    call_id: CallId,
    result: CallOutput,
}

impl StorageLane {
    pub(super) fn inline() -> Self {
        Self::Inline
    }

    pub(super) fn new(capacity: usize) -> Self {
        Self::Worker(StorageWorkerLane::new(capacity))
    }

    pub(super) fn submit(&mut self, call_id: CallId, job: StorageJob) -> Option<DriverCompletion> {
        match self {
            Self::Inline => Some(DriverCompletion {
                call_id,
                result: execute_storage_job(job),
            }),
            Self::Worker(lane) => lane.submit(call_id, job),
        }
    }

    pub(super) fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
        if let Self::Worker(lane) = self {
            lane.advance(completed);
        }
    }

    pub(super) fn has_pending(&self) -> bool {
        match self {
            Self::Inline => false,
            Self::Worker(lane) => lane.has_pending(),
        }
    }

    pub(super) fn cancel(&mut self, call_id: CallId) -> bool {
        match self {
            Self::Inline => false,
            Self::Worker(lane) => lane.cancel(call_id),
        }
    }

    pub(super) fn cancel_pending(&mut self, deadline: Instant) {
        if let Self::Worker(lane) = self {
            lane.cancel_pending(deadline);
        }
    }

    pub(super) fn physical_pending_count(&self) -> usize {
        match self {
            Self::Inline => 0,
            Self::Worker(lane) => lane.physical_pending_count(),
        }
    }
}

impl Drop for StorageLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}
impl StorageWorkerLane {
    pub(super) fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "storage lane capacity must be > 0");
        let (sender, receiver) = sync_channel(capacity);
        let (completion_sender, completions) = sync_channel(capacity.saturating_add(1));
        let handle = thread::spawn(move || storage_worker_loop(receiver, completion_sender));
        Self {
            capacity,
            sender: Some(sender),
            completions,
            handle: Some(handle),
            pending: Vec::with_capacity(capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
        }
    }

    pub(super) fn submit(&mut self, call_id: CallId, job: StorageJob) -> Option<DriverCompletion> {
        let Some(sender) = &self.sender else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::StorageClosed),
            });
        };
        if self.active_pending_count() >= self.capacity {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::StorageFull),
            });
        }
        let cancelled = Arc::new(AtomicBool::new(false));

        match sender.try_send(StorageCommand {
            call_id,
            job,
            cancelled: Arc::clone(&cancelled),
        }) {
            Ok(()) => {
                self.pending.push(StoragePending { call_id, cancelled });
                None
            }
            Err(MpscTrySendError::Full(command)) => Some(DriverCompletion {
                call_id: command.call_id,
                result: CallOutput::Failed(CallError::StorageFull),
            }),
            Err(MpscTrySendError::Disconnected(command)) => Some(DriverCompletion {
                call_id: command.call_id,
                result: CallOutput::Failed(CallError::StorageClosed),
            }),
        }
    }

    pub(super) fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
        loop {
            match self.completions.try_recv() {
                Ok(completion) => self.finish_completion(completion, completed),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.sender = None;
                    break;
                }
            }
        }
    }

    fn finish_completion(
        &mut self,
        completion: StorageCompletion,
        completed: &mut Vec<DriverCompletion>,
    ) {
        let Some(index) = self
            .pending
            .iter()
            .position(|entry| entry.call_id == completion.call_id)
        else {
            return;
        };
        let pending = self.pending.remove(index);
        if pending.cancelled.load(Ordering::Acquire) {
            return;
        }
        completed.push(DriverCompletion {
            call_id: completion.call_id,
            result: completion.result,
        });
    }

    pub(super) fn has_pending(&self) -> bool {
        self.active_pending_count() > 0
    }

    pub(super) fn cancel(&mut self, call_id: CallId) -> bool {
        let Some(pending) = self
            .pending
            .iter_mut()
            .find(|entry| entry.call_id == call_id && !entry.cancelled.load(Ordering::Acquire))
        else {
            return false;
        };
        pending.cancelled.store(true, Ordering::Release);
        true
    }

    pub(super) fn cancel_pending(&mut self, deadline: Instant) {
        // Drop the command sender so the worker thread will exit on its
        // next recv. Then drain completion results until the lane has no
        // pending work or the budget elapses; whatever remains in
        // `self.pending` is stuck work that the worker has not finished
        // and will surface in `physical_pending_count`.
        self.sender = None;
        let mut sink = Vec::new();
        loop {
            self.drain_into_sink(&mut sink);
            if self.pending.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        sink.clear();
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn drain_into_sink(&mut self, sink: &mut Vec<DriverCompletion>) {
        while let Ok(completion) = self.completions.try_recv() {
            self.finish_completion(completion, sink);
        }
    }

    fn active_pending_count(&self) -> usize {
        self.pending
            .iter()
            .filter(|entry| !entry.cancelled.load(Ordering::Acquire))
            .count()
    }

    pub(super) fn physical_pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Drop for StorageWorkerLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}
fn storage_worker_loop(
    receiver: Receiver<StorageCommand>,
    completions: SyncSender<StorageCompletion>,
) {
    while let Ok(command) = receiver.recv() {
        if command.cancelled.load(Ordering::Acquire) {
            continue;
        }
        let completion = StorageCompletion {
            call_id: command.call_id,
            result: execute_storage_job(command.job),
        };
        if completions.send(completion).is_err() {
            break;
        }
    }
}

fn execute_storage_job(job: StorageJob) -> CallOutput {
    match job {
        StorageJob::SnapshotCommit {
            path,
            bytes,
            last_journal_index,
        } => match crate::persistence::commit_snapshot(&path, bytes, last_journal_index) {
            Ok(()) => CallOutput::SnapshotCommitted,
            Err(reason) => CallOutput::Failed(reason),
        },
        StorageJob::SnapshotLoad { path } => match crate::persistence::load_snapshot(&path) {
            Ok(snapshot) => CallOutput::SnapshotLoaded { snapshot },
            Err(reason) => CallOutput::Failed(reason),
        },
        StorageJob::JournalAppend {
            path,
            record_index,
            bytes,
        } => match crate::persistence::append_journal_record(&path, record_index, bytes) {
            Ok(()) => CallOutput::JournalAppended { record_index },
            Err(reason) => CallOutput::Failed(reason),
        },
        StorageJob::JournalReplay { path } => match crate::persistence::replay_journal(&path) {
            Ok(replay) => CallOutput::JournalReplayed { replay },
            Err(reason) => CallOutput::Failed(reason),
        },
        StorageJob::PathMetadata { path } => path_metadata_output(&path),
        StorageJob::RenameReplace { from, to } => rename_replace_output(&from, &to),
        StorageJob::RemoveFile { path } => match std::fs::remove_file(&path) {
            Ok(()) => CallOutput::FileRemoved,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                CallOutput::Failed(CallError::NotFound)
            }
            Err(_) => CallOutput::Failed(CallError::Io),
        },
        StorageJob::ReadDir { path } => read_dir_output(&path),
        StorageJob::SyncParent { path } => {
            let parent = path_parent_or_current(&path);
            match crate::persistence::sync_parent_directory(parent) {
                Ok(()) => CallOutput::ParentSynced,
                Err(error) => CallOutput::Failed(error),
            }
        }
        #[cfg(test)]
        StorageJob::Park { started, release } => {
            let _ = started.send(());
            let _ = release.recv();
            CallOutput::DirectoryCreated
        }
    }
}

fn path_metadata_output(path: &std::path::Path) -> CallOutput {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let kind = if metadata.is_file() {
                PathKind::File
            } else if metadata.is_dir() {
                PathKind::Directory
            } else {
                PathKind::Other
            };
            let len = matches!(kind, PathKind::File).then_some(metadata.len());
            CallOutput::PathMetadata {
                metadata: PathMetadata { kind, len },
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => CallOutput::PathMetadata {
            metadata: PathMetadata::missing(),
        },
        Err(_) => CallOutput::Failed(CallError::Io),
    }
}

fn rename_replace_output(from: &std::path::Path, to: &std::path::Path) -> CallOutput {
    #[cfg(not(unix))]
    if to.exists() {
        return CallOutput::Failed(CallError::Unsupported);
    }

    match std::fs::rename(from, to) {
        Ok(()) => CallOutput::PathRenamed,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            CallOutput::Failed(CallError::NotFound)
        }
        Err(_) => CallOutput::Failed(CallError::Io),
    }
}

fn read_dir_output(path: &std::path::Path) -> CallOutput {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return CallOutput::Failed(CallError::NotFound);
        }
        Err(_) => return CallOutput::Failed(CallError::Io),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return CallOutput::Failed(CallError::Io);
        };
        paths.push(entry.path());
    }
    paths.sort();
    CallOutput::DirectoryRead { entries: paths }
}

fn path_parent_or_current(path: &std::path::Path) -> &std::path::Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
}
