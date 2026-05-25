//! Storage call lane.
//!
//! Two shapes:
//! - [`StorageLane::Inline`]: synchronous `std::fs` for the explicit-step
//!   oracle. Unchanged.
//! - [`StorageLane::Reactor`]: the live runtime path. Durability reads/writes/
//!   fsync/size ride the per-shard Betelgeuse file rail (no storage worker
//!   thread); the few ops Betelgeuse lacks (rename/remove/readdir/metadata,
//!   plus internal create-dir-all/truncate) run on one thin bounded off-shard
//!   fallback worker.
//!
//! The byte-on-disk format and recovery semantics live in
//! [`crate::persistence`]. The reactor path reuses its pure encode/decode/
//! replay/sidecar helpers and only swaps the I/O mechanism, so torn-tail,
//! checksum, duplicate-index, and `CommitUncertain` outcomes match inline.
//!
//! Ordering: a write submits `pwrite`, waits for that completion to be
//! harvested, *then* submits `fsync`. A job's terminal completion is produced
//! only after `fsync` (and any sidecar/parent-dir sync) completes, so the
//! runtime applies state strictly after durability. Because the on-disk
//! format is crash-consistent between every syscall, stopping a job between
//! steps (cancel/shutdown) is indistinguishable from a crash there, which
//! recovery already handles.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

use super::*;

pub(super) enum StorageLane {
    Inline,
    // Boxed: the reactor state is far larger than the empty inline variant,
    // and a lane is held one-per-driver.
    Reactor(Box<ReactorStorage>),
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

impl StorageJob {
    /// The path a write-family job must serialize on, so two writes to the
    /// same file never interleave their size/offset/rename steps. Reads and
    /// metadata return `None` and run concurrently.
    fn write_lock_path(&self) -> Option<PathBuf> {
        match self {
            Self::JournalAppend { path, .. } | Self::SnapshotCommit { path, .. } => {
                Some(path.clone())
            }
            _ => None,
        }
    }
}

impl StorageLane {
    pub(super) fn inline() -> Self {
        Self::Inline
    }

    pub(super) fn reactor(io_loop: IOLoopHandle<Global>, capacity: usize) -> Self {
        Self::Reactor(Box::new(ReactorStorage::new(io_loop, capacity)))
    }

    pub(super) fn submit(&mut self, call_id: CallId, job: StorageJob) -> Option<DriverCompletion> {
        match self {
            Self::Inline => Some(DriverCompletion {
                call_id,
                result: execute_storage_job(job),
            }),
            Self::Reactor(lane) => lane.submit(call_id, job),
        }
    }

    pub(super) fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
        if let Self::Reactor(lane) = self {
            lane.advance(completed);
        }
    }

    pub(super) fn has_pending(&self) -> bool {
        match self {
            Self::Inline => false,
            Self::Reactor(lane) => lane.has_pending(),
        }
    }

    pub(super) fn cancel(&mut self, call_id: CallId) -> bool {
        match self {
            Self::Inline => false,
            Self::Reactor(lane) => lane.cancel(call_id),
        }
    }

    pub(super) fn cancel_pending(&mut self, deadline: Instant) {
        if let Self::Reactor(lane) = self {
            lane.cancel_pending(deadline);
        }
    }

    pub(super) fn physical_pending_count(&self) -> usize {
        match self {
            Self::Inline => 0,
            Self::Reactor(lane) => lane.physical_pending_count(),
        }
    }

    /// Whether the off-shard fallback worker thread has been spawned. The
    /// reactor spawns it lazily on first fallback op, so a pure Betelgeuse
    /// durability op (load/replay/sync-parent) leaves it `false`: the guard
    /// that those ops use no worker thread.
    #[cfg(test)]
    pub(super) fn fallback_worker_spawned(&self) -> bool {
        match self {
            Self::Inline => false,
            Self::Reactor(lane) => lane.fallback.is_spawned(),
        }
    }
}

impl Drop for StorageLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}

// -----------------------------------------------------------------------------
// Reactor storage lane
// -----------------------------------------------------------------------------

pub(super) struct ReactorStorage {
    io_loop: IOLoopHandle<Global>,
    capacity: usize,
    jobs: Vec<ReactorJob>,
    fallback: FallbackWorker,
    replies: HashMap<u64, FallbackReply>,
    next_ticket: u64,
    /// Paths with an in-flight write-family job (journal append / snapshot
    /// commit). A second write to the same path waits until the first
    /// releases, so a journal's size→pwrite-at-end is never computed from a
    /// racing peer's stale length. The old single-thread worker serialized all
    /// storage; this serializes per path and lets distinct paths overlap.
    active_write_paths: HashSet<PathBuf>,
}

struct ReactorJob {
    call_id: CallId,
    cancelled: Arc<AtomicBool>,
    user_cancelled: bool,
    shutdown_marked: bool,
    /// `Some` for write-family jobs that must hold their target path's write
    /// lock before issuing any op; `None` for reads and metadata.
    lock_path: Option<PathBuf>,
    holds_lock: bool,
    machine: JobMachine,
}

impl ReactorStorage {
    fn new(io_loop: IOLoopHandle<Global>, capacity: usize) -> Self {
        assert!(capacity > 0, "storage lane capacity must be > 0");
        Self {
            io_loop,
            capacity,
            jobs: Vec::with_capacity(capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
            fallback: FallbackWorker::new(capacity),
            replies: HashMap::new(),
            next_ticket: 1,
            active_write_paths: HashSet::new(),
        }
    }

    fn active_count(&self) -> usize {
        self.jobs.iter().filter(|job| !job.user_cancelled).count()
    }

    fn submit(&mut self, call_id: CallId, job: StorageJob) -> Option<DriverCompletion> {
        if self.fallback.is_closed() {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::StorageClosed),
            });
        }
        if self.active_count() >= self.capacity {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::StorageFull),
            });
        }
        let lock_path = job.write_lock_path();
        self.jobs.push(ReactorJob {
            call_id,
            cancelled: Arc::new(AtomicBool::new(false)),
            user_cancelled: false,
            shutdown_marked: false,
            lock_path,
            holds_lock: false,
            machine: JobMachine::new(job),
        });
        None
    }

    fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
        // One substrate tick, drain fallback replies, then poll each job.
        let _ = self.io_loop.step();
        self.drain_fallback();

        let mut index = 0;
        while index < self.jobs.len() {
            let mut job = self.jobs.remove(index);
            if job.user_cancelled || job.shutdown_marked {
                // Do not advance cancelled work. Keep it (and its write lock)
                // only while the backend still owns an in-flight completion
                // slot — e.g. an in-flight pwrite, which a same-path peer must
                // not race. Once the slot has a result (or none is armed) the
                // job is dropped, its lock freed, and no terminal completion is
                // ever delivered.
                if job.machine.has_outstanding_completion() {
                    self.jobs.insert(index, job);
                    index += 1;
                } else {
                    self.release_write_lock(&mut job);
                }
                continue;
            }

            // Per-path write serialization: a write-family job issues no op
            // until it holds its target path's lock. A same-path peer waits.
            if let Some(path) = &job.lock_path
                && !job.holds_lock
            {
                if self.active_write_paths.contains(path) {
                    self.jobs.insert(index, job);
                    index += 1;
                    continue;
                }
                self.active_write_paths.insert(path.clone());
                job.holds_lock = true;
            }

            let mut bridge = FallbackBridge {
                worker: &mut self.fallback,
                replies: &mut self.replies,
                next_ticket: &mut self.next_ticket,
                cancelled: &job.cancelled,
            };
            match job.machine.poll(&self.io_loop, &mut bridge) {
                Some(result) => {
                    self.release_write_lock(&mut job);
                    completed.push(DriverCompletion {
                        call_id: job.call_id,
                        result,
                    });
                }
                None => {
                    self.jobs.insert(index, job);
                    index += 1;
                }
            }
        }

        self.prune_orphan_replies();
    }

    fn release_write_lock(&mut self, job: &mut ReactorJob) {
        if job.holds_lock {
            if let Some(path) = &job.lock_path {
                self.active_write_paths.remove(path);
            }
            job.holds_lock = false;
        }
    }

    fn drain_fallback(&mut self) {
        while let Some(completion) = self.fallback.try_recv() {
            // Ticket 0 is reserved for detached best-effort work whose reply
            // nobody awaits.
            if completion.ticket != 0 {
                self.replies.insert(completion.ticket, completion.reply);
            }
        }
    }

    /// Drop replies no remaining job awaits (e.g. a cancelled job whose
    /// fallback op had already started). Bounds `replies` memory.
    fn prune_orphan_replies(&mut self) {
        if self.replies.is_empty() {
            return;
        }
        let awaited: Vec<u64> = self
            .jobs
            .iter()
            .filter_map(|job| job.machine.awaited_ticket())
            .collect();
        self.replies.retain(|ticket, _| awaited.contains(ticket));
    }

    fn has_pending(&self) -> bool {
        self.jobs
            .iter()
            .any(|job| !job.user_cancelled && !job.shutdown_marked)
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let Some(job) = self
            .jobs
            .iter_mut()
            .find(|job| job.call_id == call_id && !job.user_cancelled)
        else {
            return false;
        };
        job.user_cancelled = true;
        // Stop any queued fallback sub-step that has not yet started.
        job.cancelled.store(true, Ordering::Release);
        true
    }

    fn cancel_pending(&mut self, deadline: Instant) {
        for job in &mut self.jobs {
            job.shutdown_marked = true;
            job.cancelled.store(true, Ordering::Release);
        }
        // Release backend-owned completion slots, shut the fallback worker,
        // then drain until no job owns an outstanding Betelgeuse completion or
        // the budget elapses. Whatever remains is stuck work that stays
        // visible in `physical_pending_count`.
        let _ = self.io_loop.cancel_pending_completions();
        self.fallback.shutdown();
        loop {
            let _ = self.io_loop.step();
            self.drain_fallback();
            self.jobs
                .retain(|job| job.machine.has_outstanding_completion());
            if self.jobs.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.replies.clear();
        self.active_write_paths.clear();
        self.fallback.join_if_finished();
    }

    fn physical_pending_count(&self) -> usize {
        self.jobs.len()
    }
}

// -----------------------------------------------------------------------------
// Fallback worker: one thin bounded off-shard thread for ops Betelgeuse lacks.
// -----------------------------------------------------------------------------

/// Ops Betelgeuse has no opcode for. Run on the fallback worker thread.
#[derive(Clone)]
enum FallbackOp {
    /// Top-level `PathMetadata`.
    Metadata(PathBuf),
    /// Rename; `internal` distinguishes commit/sidecar sub-steps from the
    /// user-facing `RenameReplace` call.
    Rename {
        from: PathBuf,
        to: PathBuf,
        internal: bool,
    },
    /// Remove; `best_effort` cleanup ignores the result.
    Remove { path: PathBuf, best_effort: bool },
    /// Top-level `ReadDir`.
    ReadDir(PathBuf),
    /// Internal recursive parent-directory creation.
    CreateDirAll(PathBuf),
    /// Internal torn-tail repair (`ftruncate`).
    Truncate { path: PathBuf, len: u64 },
}

/// Result of one fallback op. `Output` is a ready-to-deliver completion for a
/// top-level metadata call; `Unit` is the result of an internal sub-step.
enum FallbackReply {
    Output(CallOutput),
    Unit(Result<(), CallError>),
}

struct FallbackCommand {
    ticket: u64,
    op: FallbackOp,
    cancelled: Arc<AtomicBool>,
}

struct FallbackCompletion {
    ticket: u64,
    reply: FallbackReply,
}

struct FallbackWorker {
    capacity: usize,
    sender: Option<SyncSender<FallbackCommand>>,
    completions: Option<Receiver<FallbackCompletion>>,
    handle: Option<JoinHandle<()>>,
    closed: bool,
}

impl FallbackWorker {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            sender: None,
            completions: None,
            handle: None,
            closed: false,
        }
    }

    #[cfg(test)]
    fn is_spawned(&self) -> bool {
        self.handle.is_some()
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    fn ensure_spawned(&mut self) {
        if self.handle.is_some() || self.closed {
            return;
        }
        // Channel sized to the storage capacity: at most `capacity` admitted
        // jobs, each with at most one fallback op in flight at a time.
        let (sender, receiver) = sync_channel(self.capacity.max(1));
        let (completion_sender, completions) = sync_channel(self.capacity.saturating_add(1));
        let handle = thread::Builder::new()
            .name("tina-storage-fallback".to_string())
            .spawn(move || fallback_worker_loop(receiver, completion_sender))
            .expect("spawn storage fallback worker");
        self.sender = Some(sender);
        self.completions = Some(completions);
        self.handle = Some(handle);
    }

    /// Enqueues one fallback op. Returns `false` when the worker is closed or
    /// the queue is momentarily full (the caller retries next advance).
    fn submit(&mut self, command: FallbackCommand) -> bool {
        self.ensure_spawned();
        let Some(sender) = &self.sender else {
            return false;
        };
        match sender.try_send(command) {
            Ok(()) => true,
            Err(MpscTrySendError::Full(_)) => false,
            Err(MpscTrySendError::Disconnected(_)) => {
                self.closed = true;
                false
            }
        }
    }

    fn try_recv(&mut self) -> Option<FallbackCompletion> {
        let receiver = self.completions.as_ref()?;
        match receiver.try_recv() {
            Ok(completion) => Some(completion),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.closed = true;
                None
            }
        }
    }

    fn shutdown(&mut self) {
        self.sender = None;
        self.closed = true;
    }

    fn join_if_finished(&mut self) {
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.handle.take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for FallbackWorker {
    fn drop(&mut self) {
        self.sender = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn fallback_worker_loop(
    receiver: Receiver<FallbackCommand>,
    completions: SyncSender<FallbackCompletion>,
) {
    while let Ok(command) = receiver.recv() {
        // Cancelled-before-start work does not run.
        if command.cancelled.load(Ordering::Acquire) {
            continue;
        }
        let reply = run_fallback_op(command.op);
        if completions
            .send(FallbackCompletion {
                ticket: command.ticket,
                reply,
            })
            .is_err()
        {
            break;
        }
    }
}

fn run_fallback_op(op: FallbackOp) -> FallbackReply {
    match op {
        FallbackOp::Metadata(path) => FallbackReply::Output(path_metadata_output(&path)),
        FallbackOp::Rename { from, to, internal } => {
            let output = rename_replace_output(&from, &to);
            if internal {
                FallbackReply::Unit(call_output_to_unit(output))
            } else {
                FallbackReply::Output(output)
            }
        }
        FallbackOp::Remove { path, best_effort } => {
            if best_effort {
                let _ = std::fs::remove_file(&path);
                FallbackReply::Unit(Ok(()))
            } else {
                FallbackReply::Output(remove_file_output(&path))
            }
        }
        FallbackOp::ReadDir(path) => FallbackReply::Output(read_dir_output(&path)),
        FallbackOp::CreateDirAll(path) => {
            FallbackReply::Unit(std::fs::create_dir_all(&path).map_err(|_| CallError::Io))
        }
        FallbackOp::Truncate { path, len } => FallbackReply::Unit(truncate_file(&path, len)),
    }
}

fn truncate_file(path: &Path, len: u64) -> Result<(), CallError> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|_| CallError::Io)?;
    file.set_len(len).map_err(|_| CallError::Io)
}

fn call_output_to_unit(output: CallOutput) -> Result<(), CallError> {
    match output {
        CallOutput::PathRenamed | CallOutput::FileRemoved => Ok(()),
        CallOutput::Failed(reason) => Err(reason),
        _ => Err(CallError::Io),
    }
}

// -----------------------------------------------------------------------------
// Fallback bridge handed to a polling job.
// -----------------------------------------------------------------------------

struct FallbackBridge<'a> {
    worker: &'a mut FallbackWorker,
    replies: &'a mut HashMap<u64, FallbackReply>,
    next_ticket: &'a mut u64,
    cancelled: &'a Arc<AtomicBool>,
}

impl FallbackBridge<'_> {
    /// Submits a tracked fallback op, returning its ticket. `None` when the
    /// worker could not accept it now (retry next advance).
    fn submit(&mut self, op: FallbackOp) -> Option<u64> {
        let ticket = *self.next_ticket;
        let command = FallbackCommand {
            ticket,
            op,
            cancelled: Arc::clone(self.cancelled),
        };
        if self.worker.submit(command) {
            *self.next_ticket += 1;
            Some(ticket)
        } else {
            None
        }
    }

    /// Submits a fire-and-forget op whose reply nobody awaits (ticket 0).
    fn submit_detached(&mut self, op: FallbackOp) {
        let _ = self.worker.submit(FallbackCommand {
            ticket: 0,
            op,
            cancelled: Arc::new(AtomicBool::new(false)),
        });
    }

    fn take(&mut self, ticket: u64) -> Option<FallbackReply> {
        self.replies.remove(&ticket)
    }
}

/// Awaiting one off-shard fallback op by ticket.
enum FallbackSlot {
    Idle,
    Awaiting(u64),
}

impl FallbackSlot {
    fn ticket(&self) -> Option<u64> {
        match self {
            Self::Idle => None,
            Self::Awaiting(ticket) => Some(*ticket),
        }
    }
}

/// Drives one fallback sub-step expected to yield a unit result. Submits on
/// first call, harvests on a later one.
fn poll_fallback_unit(
    fb: &mut FallbackBridge<'_>,
    slot: &mut FallbackSlot,
    make_op: impl FnOnce() -> FallbackOp,
) -> Option<Result<(), CallError>> {
    match slot {
        FallbackSlot::Idle => {
            if let Some(ticket) = fb.submit(make_op()) {
                *slot = FallbackSlot::Awaiting(ticket);
            }
            None
        }
        FallbackSlot::Awaiting(ticket) => {
            let reply = fb.take(*ticket)?;
            *slot = FallbackSlot::Idle;
            Some(match reply {
                FallbackReply::Unit(result) => result,
                FallbackReply::Output(_) => Err(CallError::Io),
            })
        }
    }
}

// -----------------------------------------------------------------------------
// Job state machines
// -----------------------------------------------------------------------------

/// One in-flight Betelgeuse file op. Heap-boxed so the backend's stored
/// pointer to `CompletionInner` stays valid while the owning job moves
/// through the `jobs` vector.
enum InFlight {
    None,
    Size(Box<SizeCompletion>),
    Read(Box<PReadCompletion>),
    Write(Box<PWriteCompletion>),
    Fsync(Box<FsyncCompletion>),
}

impl InFlight {
    /// Whether the backend still owns this completion slot by pointer: a slot
    /// is armed and the backend has not yet written its result. Once a result
    /// lands, the backend will not touch the slot again, so a cancelled job
    /// holding it can be dropped. (Matches the TCP lane's `has_result`-drop.)
    fn backend_owns(&self) -> bool {
        match self {
            Self::None => false,
            Self::Size(c) => !c.has_result(),
            Self::Read(c) => !c.has_result(),
            Self::Write(c) => !c.has_result(),
            Self::Fsync(c) => !c.has_result(),
        }
    }
}

enum JobMachine {
    // The two multi-leg durability jobs are boxed: they are much larger than
    // the read/metadata jobs, and one `JobMachine` lives per in-flight storage
    // call.
    SnapshotCommit(Box<SnapshotCommitJob>),
    Read(ReadFileJob),
    JournalAppend(Box<JournalAppendJob>),
    SyncParent(SyncParentJob),
    Metadata(MetadataJob),
    #[cfg(test)]
    Park(ParkJob),
}

impl JobMachine {
    fn new(job: StorageJob) -> Self {
        match job {
            StorageJob::SnapshotCommit {
                path,
                bytes,
                last_journal_index,
            } => Self::SnapshotCommit(Box::new(SnapshotCommitJob::new(
                path,
                bytes,
                last_journal_index,
            ))),
            StorageJob::SnapshotLoad { path } => {
                Self::Read(ReadFileJob::new(path, ReadDecode::Snapshot))
            }
            StorageJob::JournalReplay { path } => {
                Self::Read(ReadFileJob::new(path, ReadDecode::Journal))
            }
            StorageJob::JournalAppend {
                path,
                record_index,
                bytes,
            } => Self::JournalAppend(Box::new(JournalAppendJob::new(path, record_index, bytes))),
            StorageJob::SyncParent { path } => Self::SyncParent(SyncParentJob::new(&path)),
            StorageJob::PathMetadata { path } => {
                Self::Metadata(MetadataJob::new(FallbackOp::Metadata(path)))
            }
            StorageJob::RenameReplace { from, to } => {
                Self::Metadata(MetadataJob::new(FallbackOp::Rename {
                    from,
                    to,
                    internal: false,
                }))
            }
            StorageJob::RemoveFile { path } => {
                Self::Metadata(MetadataJob::new(FallbackOp::Remove {
                    path,
                    best_effort: false,
                }))
            }
            StorageJob::ReadDir { path } => {
                Self::Metadata(MetadataJob::new(FallbackOp::ReadDir(path)))
            }
            #[cfg(test)]
            StorageJob::Park { started, release } => Self::Park(ParkJob::new(started, release)),
        }
    }

    fn has_outstanding_completion(&self) -> bool {
        match self {
            Self::SnapshotCommit(job) => job.has_outstanding_completion(),
            Self::Read(job) => job.in_flight.backend_owns(),
            Self::JournalAppend(job) => job.has_outstanding_completion(),
            Self::SyncParent(job) => job.in_flight.backend_owns(),
            // Metadata/Park own no Betelgeuse completion slot.
            Self::Metadata(_) => false,
            #[cfg(test)]
            Self::Park(_) => false,
        }
    }

    fn awaited_ticket(&self) -> Option<u64> {
        match self {
            Self::SnapshotCommit(job) => job.awaited_ticket(),
            Self::JournalAppend(job) => job.awaited_ticket(),
            Self::Metadata(job) => job.slot.ticket(),
            Self::Read(_) | Self::SyncParent(_) => None,
            #[cfg(test)]
            Self::Park(_) => None,
        }
    }

    fn poll(
        &mut self,
        io: &IOLoopHandle<Global>,
        fb: &mut FallbackBridge<'_>,
    ) -> Option<CallOutput> {
        match self {
            Self::SnapshotCommit(job) => job.poll(io, fb),
            Self::Read(job) => job.poll(io),
            Self::JournalAppend(job) => job.poll(io, fb),
            Self::SyncParent(job) => job.poll(io),
            Self::Metadata(job) => job.poll(fb),
            #[cfg(test)]
            Self::Park(job) => job.poll(),
        }
    }
}

fn open_read(io: &IOLoopHandle<Global>, path: &Path) -> io::Result<Box<dyn IOFile>> {
    io.open(
        path,
        OpenOptions {
            read: true,
            write: false,
            create: false,
            truncate: false,
        },
    )
}

fn open_write_create(
    io: &IOLoopHandle<Global>,
    path: &Path,
    truncate: bool,
) -> io::Result<Box<dyn IOFile>> {
    io.open(
        path,
        OpenOptions {
            read: false,
            write: true,
            create: true,
            truncate,
        },
    )
}

fn parent_directory_owned(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

// --- Read whole file then decode (SnapshotLoad / JournalReplay) -------------

#[derive(Clone, Copy)]
enum ReadDecode {
    Snapshot,
    Journal,
}

enum ReadPhase {
    Start,
    Size,
    Read,
    Done,
}

struct ReadFileJob {
    path: PathBuf,
    decode: ReadDecode,
    phase: ReadPhase,
    file: Option<Box<dyn IOFile>>,
    in_flight: InFlight,
    want: u64,
    acc: Vec<u8>,
}

impl ReadFileJob {
    fn new(path: PathBuf, decode: ReadDecode) -> Self {
        Self {
            path,
            decode,
            phase: ReadPhase::Start,
            file: None,
            in_flight: InFlight::None,
            want: 0,
            acc: Vec::new(),
        }
    }

    fn poll(&mut self, io: &IOLoopHandle<Global>) -> Option<CallOutput> {
        loop {
            match self.phase {
                ReadPhase::Start => match open_read(io, &self.path) {
                    Ok(file) => {
                        let mut completion = Box::new(SizeCompletion::new());
                        if file.size(&mut completion).is_err() {
                            return Some(CallOutput::Failed(CallError::Io));
                        }
                        self.file = Some(file);
                        self.in_flight = InFlight::Size(completion);
                        self.phase = ReadPhase::Size;
                        return None;
                    }
                    // A missing file is empty for both decoders.
                    Err(error) if error.kind() == ErrorKind::NotFound => {
                        return Some(self.decode_empty());
                    }
                    Err(_) => return Some(CallOutput::Failed(CallError::Io)),
                },
                ReadPhase::Size => {
                    let InFlight::Size(completion) = &mut self.in_flight else {
                        unreachable!("size phase holds a size completion")
                    };
                    let result = completion.take_result()?;
                    self.in_flight = InFlight::None;
                    match result {
                        Ok(0) => self.phase = ReadPhase::Done,
                        Ok(size) => {
                            self.want = size;
                            self.phase = ReadPhase::Read;
                        }
                        Err(_) => return Some(CallOutput::Failed(CallError::Io)),
                    }
                }
                ReadPhase::Read => {
                    if let InFlight::Read(completion) = &mut self.in_flight {
                        let result = completion.take_result()?;
                        self.in_flight = InFlight::None;
                        match result {
                            Ok(bytes) if bytes.is_empty() => self.phase = ReadPhase::Done,
                            Ok(bytes) => {
                                self.acc.extend_from_slice(&bytes);
                                if self.acc.len() as u64 >= self.want {
                                    self.phase = ReadPhase::Done;
                                }
                            }
                            Err(_) => return Some(CallOutput::Failed(CallError::Io)),
                        }
                    } else {
                        let remaining = self.want - self.acc.len() as u64;
                        let mut completion = Box::new(PReadCompletion::new());
                        if self
                            .file
                            .as_ref()
                            .expect("file open")
                            .pread(&mut completion, remaining as usize, self.acc.len() as u64)
                            .is_err()
                        {
                            return Some(CallOutput::Failed(CallError::Io));
                        }
                        self.in_flight = InFlight::Read(completion);
                        return None;
                    }
                }
                ReadPhase::Done => {
                    self.file = None;
                    return Some(self.decode_loaded());
                }
            }
        }
    }

    fn decode_empty(&self) -> CallOutput {
        match self.decode {
            // A missing snapshot file is `None`, not an error: matches the
            // inline path's `load_snapshot` NotFound arm.
            ReadDecode::Snapshot => CallOutput::SnapshotLoaded { snapshot: None },
            ReadDecode::Journal => CallOutput::JournalReplayed {
                replay: crate::JournalReplay {
                    records: Vec::new(),
                    warning: None,
                },
            },
        }
    }

    fn decode_loaded(&mut self) -> CallOutput {
        let bytes = std::mem::take(&mut self.acc);
        match self.decode {
            ReadDecode::Snapshot => match crate::persistence::decode_snapshot(&bytes) {
                Ok(snapshot) => CallOutput::SnapshotLoaded {
                    snapshot: Some(snapshot),
                },
                Err(reason) => CallOutput::Failed(reason),
            },
            ReadDecode::Journal => match crate::persistence::replay_journal_bytes(&bytes) {
                Ok(replay) => CallOutput::JournalReplayed { replay },
                Err(reason) => CallOutput::Failed(reason),
            },
        }
    }
}

// --- Sync a directory: open(read) + fsync -----------------------------------

enum SyncPhase {
    Start,
    Fsync,
    Done,
}

struct SyncParentJob {
    dir: PathBuf,
    phase: SyncPhase,
    file: Option<Box<dyn IOFile>>,
    in_flight: InFlight,
}

impl SyncParentJob {
    fn new(path: &Path) -> Self {
        Self {
            dir: parent_directory_owned(path),
            phase: SyncPhase::Start,
            file: None,
            in_flight: InFlight::None,
        }
    }

    fn for_dir(dir: PathBuf) -> Self {
        Self {
            dir,
            phase: SyncPhase::Start,
            file: None,
            in_flight: InFlight::None,
        }
    }

    /// `Some(Ok(()))` synced, `Some(Err(_))` failed, `None` in flight.
    fn poll_unit(&mut self, io: &IOLoopHandle<Global>) -> Option<Result<(), CallError>> {
        loop {
            match self.phase {
                SyncPhase::Start => {
                    let file = match open_read(io, &self.dir) {
                        Ok(file) => file,
                        Err(_) => return Some(Err(CallError::Io)),
                    };
                    let mut completion = Box::new(FsyncCompletion::new());
                    if file.fsync(&mut completion).is_err() {
                        return Some(Err(CallError::Io));
                    }
                    self.file = Some(file);
                    self.in_flight = InFlight::Fsync(completion);
                    self.phase = SyncPhase::Fsync;
                    return None;
                }
                SyncPhase::Fsync => {
                    let InFlight::Fsync(completion) = &mut self.in_flight else {
                        unreachable!("fsync phase holds an fsync completion")
                    };
                    let result = completion.take_result()?;
                    self.in_flight = InFlight::None;
                    self.phase = SyncPhase::Done;
                    if result.is_err() {
                        return Some(Err(CallError::Io));
                    }
                }
                SyncPhase::Done => {
                    self.file = None;
                    return Some(Ok(()));
                }
            }
        }
    }

    fn poll(&mut self, io: &IOLoopHandle<Global>) -> Option<CallOutput> {
        match self.poll_unit(io)? {
            Ok(()) => Some(CallOutput::ParentSynced),
            Err(reason) => Some(CallOutput::Failed(reason)),
        }
    }
}

// --- Write a fresh file fully then fsync (temp snapshot / sidecar temp) ------

enum WritePhase {
    Open,
    Write,
    Fsync,
    Done,
}

struct WriteNewFile {
    path: PathBuf,
    data: Vec<u8>,
    written: usize,
    phase: WritePhase,
    file: Option<Box<dyn IOFile>>,
    in_flight: InFlight,
}

impl WriteNewFile {
    fn new(path: PathBuf, data: Vec<u8>) -> Self {
        Self {
            path,
            data,
            written: 0,
            phase: WritePhase::Open,
            file: None,
            in_flight: InFlight::None,
        }
    }

    fn poll(&mut self, io: &IOLoopHandle<Global>) -> Option<Result<(), CallError>> {
        loop {
            match self.phase {
                WritePhase::Open => match open_write_create(io, &self.path, true) {
                    Ok(file) => {
                        self.file = Some(file);
                        self.phase = WritePhase::Write;
                    }
                    Err(_) => return Some(Err(CallError::Io)),
                },
                WritePhase::Write => {
                    if let InFlight::Write(completion) = &mut self.in_flight {
                        let result = completion.take_result()?;
                        self.in_flight = InFlight::None;
                        match result {
                            Ok(0) if self.written < self.data.len() => {
                                return Some(Err(CallError::Io)); // no progress
                            }
                            Ok(count) => {
                                self.written += count;
                                if self.written >= self.data.len() {
                                    self.phase = WritePhase::Fsync;
                                }
                            }
                            Err(_) => return Some(Err(CallError::Io)),
                        }
                    } else {
                        let chunk = self.data[self.written..].to_vec();
                        let offset = self.written as u64;
                        let mut completion = Box::new(PWriteCompletion::new());
                        if self
                            .file
                            .as_ref()
                            .expect("file open")
                            .pwrite(&mut completion, chunk, offset)
                            .is_err()
                        {
                            return Some(Err(CallError::Io));
                        }
                        self.in_flight = InFlight::Write(completion);
                        return None;
                    }
                }
                WritePhase::Fsync => {
                    if let InFlight::Fsync(completion) = &mut self.in_flight {
                        let result = completion.take_result()?;
                        self.in_flight = InFlight::None;
                        self.phase = WritePhase::Done;
                        if result.is_err() {
                            return Some(Err(CallError::Io));
                        }
                    } else {
                        let mut completion = Box::new(FsyncCompletion::new());
                        if self
                            .file
                            .as_ref()
                            .expect("file open")
                            .fsync(&mut completion)
                            .is_err()
                        {
                            return Some(Err(CallError::Io));
                        }
                        self.in_flight = InFlight::Fsync(completion);
                        return None;
                    }
                }
                WritePhase::Done => {
                    self.file = None;
                    return Some(Ok(()));
                }
            }
        }
    }
}

// --- Append one record to a journal: open(write,create), size, pwrite@end,
//     fsync. Returns the new file length. ---------------------------------

enum AppendPhaseInner {
    Open,
    Size,
    Write,
    Fsync,
    Done,
}

struct AppendData {
    data: Vec<u8>,
    base: u64,
    written: usize,
    phase: AppendPhaseInner,
    file: Option<Box<dyn IOFile>>,
    in_flight: InFlight,
}

impl AppendData {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            base: 0,
            written: 0,
            phase: AppendPhaseInner::Open,
            file: None,
            in_flight: InFlight::None,
        }
    }

    fn poll(&mut self, io: &IOLoopHandle<Global>, path: &Path) -> Option<Result<u64, CallError>> {
        loop {
            match self.phase {
                AppendPhaseInner::Open => match open_write_create(io, path, false) {
                    Ok(file) => {
                        let mut completion = Box::new(SizeCompletion::new());
                        if file.size(&mut completion).is_err() {
                            return Some(Err(CallError::Io));
                        }
                        self.file = Some(file);
                        self.in_flight = InFlight::Size(completion);
                        self.phase = AppendPhaseInner::Size;
                        return None;
                    }
                    Err(_) => return Some(Err(CallError::Io)),
                },
                AppendPhaseInner::Size => {
                    let InFlight::Size(completion) = &mut self.in_flight else {
                        unreachable!("size phase holds a size completion")
                    };
                    let result = completion.take_result()?;
                    self.in_flight = InFlight::None;
                    match result {
                        Ok(size) => {
                            self.base = size;
                            self.phase = AppendPhaseInner::Write;
                        }
                        Err(_) => return Some(Err(CallError::Io)),
                    }
                }
                AppendPhaseInner::Write => {
                    if let InFlight::Write(completion) = &mut self.in_flight {
                        let result = completion.take_result()?;
                        self.in_flight = InFlight::None;
                        match result {
                            Ok(0) if self.written < self.data.len() => {
                                return Some(Err(CallError::Io));
                            }
                            Ok(count) => {
                                self.written += count;
                                if self.written >= self.data.len() {
                                    self.phase = AppendPhaseInner::Fsync;
                                }
                            }
                            Err(_) => return Some(Err(CallError::Io)),
                        }
                    } else {
                        let chunk = self.data[self.written..].to_vec();
                        let offset = self.base + self.written as u64;
                        let mut completion = Box::new(PWriteCompletion::new());
                        if self
                            .file
                            .as_ref()
                            .expect("file open")
                            .pwrite(&mut completion, chunk, offset)
                            .is_err()
                        {
                            return Some(Err(CallError::Io));
                        }
                        self.in_flight = InFlight::Write(completion);
                        return None;
                    }
                }
                AppendPhaseInner::Fsync => {
                    if let InFlight::Fsync(completion) = &mut self.in_flight {
                        let result = completion.take_result()?;
                        self.in_flight = InFlight::None;
                        self.phase = AppendPhaseInner::Done;
                        if result.is_err() {
                            return Some(Err(CallError::Io));
                        }
                    } else {
                        let mut completion = Box::new(FsyncCompletion::new());
                        if self
                            .file
                            .as_ref()
                            .expect("file open")
                            .fsync(&mut completion)
                            .is_err()
                        {
                            return Some(Err(CallError::Io));
                        }
                        self.in_flight = InFlight::Fsync(completion);
                        return None;
                    }
                }
                AppendPhaseInner::Done => {
                    self.file = None;
                    return Some(Ok(self.base + self.data.len() as u64));
                }
            }
        }
    }
}

// --- Snapshot commit --------------------------------------------------------

enum CommitPhase {
    CreateDir,
    WriteTemp,
    Rename,
    SyncParent,
}

struct SnapshotCommitJob {
    path: PathBuf,
    temp_path: PathBuf,
    parent: PathBuf,
    encoded: Vec<u8>,
    phase: CommitPhase,
    fallback: FallbackSlot,
    write: Option<WriteNewFile>,
    sync: SyncParentJob,
}

impl SnapshotCommitJob {
    fn new(path: PathBuf, bytes: Vec<u8>, last_journal_index: u64) -> Self {
        let encoded = crate::persistence::encode_snapshot(&crate::SnapshotImage {
            bytes,
            last_journal_index,
        });
        let temp_path = crate::persistence::temp_snapshot_path(&path);
        let parent = parent_directory_owned(&path);
        Self {
            path,
            temp_path,
            parent: parent.clone(),
            encoded,
            phase: CommitPhase::CreateDir,
            fallback: FallbackSlot::Idle,
            write: None,
            sync: SyncParentJob::for_dir(parent),
        }
    }

    fn has_outstanding_completion(&self) -> bool {
        self.write
            .as_ref()
            .is_some_and(|w| w.in_flight.backend_owns())
            || self.sync.in_flight.backend_owns()
    }

    fn awaited_ticket(&self) -> Option<u64> {
        self.fallback.ticket()
    }

    fn poll(
        &mut self,
        io: &IOLoopHandle<Global>,
        fb: &mut FallbackBridge<'_>,
    ) -> Option<CallOutput> {
        loop {
            match self.phase {
                CommitPhase::CreateDir => {
                    let parent = self.parent.clone();
                    match poll_fallback_unit(fb, &mut self.fallback, || {
                        FallbackOp::CreateDirAll(parent)
                    }) {
                        None => return None,
                        Some(Ok(())) => {
                            self.write = Some(WriteNewFile::new(
                                self.temp_path.clone(),
                                std::mem::take(&mut self.encoded),
                            ));
                            self.phase = CommitPhase::WriteTemp;
                        }
                        Some(Err(reason)) => return Some(CallOutput::Failed(reason)),
                    }
                }
                CommitPhase::WriteTemp => {
                    match self.write.as_mut().expect("write sub-job").poll(io) {
                        None => return None,
                        Some(Ok(())) => {
                            self.write = None;
                            self.phase = CommitPhase::Rename;
                        }
                        Some(Err(reason)) => {
                            self.write = None;
                            return Some(self.cleanup_then_fail(fb, reason));
                        }
                    }
                }
                CommitPhase::Rename => {
                    let from = self.temp_path.clone();
                    let to = self.path.clone();
                    match poll_fallback_unit(fb, &mut self.fallback, || FallbackOp::Rename {
                        from,
                        to,
                        internal: true,
                    }) {
                        None => return None,
                        Some(Ok(())) => self.phase = CommitPhase::SyncParent,
                        // Rename failed: the original is intact, durable state
                        // is known. Remove the temp and report Io.
                        Some(Err(_)) => return Some(self.cleanup_then_fail(fb, CallError::Io)),
                    }
                }
                CommitPhase::SyncParent => match self.sync.poll_unit(io) {
                    None => return None,
                    Some(Ok(())) => return Some(CallOutput::SnapshotCommitted),
                    // Rename landed but parent-dir durability is unproven.
                    Some(Err(_)) => return Some(CallOutput::Failed(CallError::CommitUncertain)),
                },
            }
        }
    }

    fn cleanup_then_fail(&mut self, fb: &mut FallbackBridge<'_>, reason: CallError) -> CallOutput {
        fb.submit_detached(FallbackOp::Remove {
            path: self.temp_path.clone(),
            best_effort: true,
        });
        CallOutput::Failed(reason)
    }
}

// --- Journal append ---------------------------------------------------------

enum AppendPhase {
    CreateDir,
    Validate,
    AppendData,
    StoreIndex,
}

struct JournalAppendJob {
    path: PathBuf,
    parent: PathBuf,
    record_index: u64,
    phase: AppendPhase,
    fallback: FallbackSlot,
    validate: ValidateIndex,
    append: AppendData,
    sidecar: Option<StoreIndexJob>,
}

impl JournalAppendJob {
    fn new(path: PathBuf, record_index: u64, bytes: Vec<u8>) -> Self {
        let encoded = crate::persistence::encode_journal_record(&crate::JournalRecord {
            index: record_index,
            bytes,
        });
        let parent = parent_directory_owned(&path);
        Self {
            parent,
            record_index,
            phase: AppendPhase::CreateDir,
            fallback: FallbackSlot::Idle,
            validate: ValidateIndex::new(path.clone()),
            append: AppendData::new(encoded),
            sidecar: None,
            path,
        }
    }

    fn has_outstanding_completion(&self) -> bool {
        self.validate.has_outstanding_completion()
            || self.append.in_flight.backend_owns()
            || self
                .sidecar
                .as_ref()
                .is_some_and(StoreIndexJob::has_outstanding_completion)
    }

    fn awaited_ticket(&self) -> Option<u64> {
        match self.phase {
            AppendPhase::CreateDir => self.fallback.ticket(),
            AppendPhase::Validate => self.validate.awaited_ticket(),
            AppendPhase::AppendData => None,
            AppendPhase::StoreIndex => self
                .sidecar
                .as_ref()
                .and_then(StoreIndexJob::awaited_ticket),
        }
    }

    fn poll(
        &mut self,
        io: &IOLoopHandle<Global>,
        fb: &mut FallbackBridge<'_>,
    ) -> Option<CallOutput> {
        loop {
            match self.phase {
                AppendPhase::CreateDir => {
                    let parent = self.parent.clone();
                    match poll_fallback_unit(fb, &mut self.fallback, || {
                        FallbackOp::CreateDirAll(parent)
                    }) {
                        None => return None,
                        Some(Ok(())) => self.phase = AppendPhase::Validate,
                        Some(Err(reason)) => return Some(CallOutput::Failed(reason)),
                    }
                }
                AppendPhase::Validate => match self.validate.poll(io, fb, self.record_index) {
                    None => return None,
                    Some(Ok(())) => self.phase = AppendPhase::AppendData,
                    Some(Err(reason)) => return Some(CallOutput::Failed(reason)),
                },
                AppendPhase::AppendData => match self.append.poll(io, &self.path) {
                    None => return None,
                    Some(Ok(file_len)) => {
                        self.sidecar = Some(StoreIndexJob::new(
                            self.path.clone(),
                            self.record_index,
                            file_len,
                        ));
                        self.phase = AppendPhase::StoreIndex;
                    }
                    Some(Err(reason)) => return Some(CallOutput::Failed(reason)),
                },
                AppendPhase::StoreIndex => {
                    match self.sidecar.as_mut().expect("sidecar job").poll(io, fb) {
                        None => return None,
                        Some(Ok(())) => {
                            return Some(CallOutput::JournalAppended {
                                record_index: self.record_index,
                            });
                        }
                        Some(Err(reason)) => return Some(CallOutput::Failed(reason)),
                    }
                }
            }
        }
    }
}

/// Reproduces `persistence::validate_next_journal_index`: trust a consistent
/// index sidecar, else replay the journal (repairing a torn tail) to find the
/// last committed index. `record_index <= last` is a duplicate/out-of-order
/// append.
enum ValidatePhase {
    LoadSidecar,
    ReplayJournal,
    RepairTail,
    Done,
}

struct ValidateIndex {
    journal_path: PathBuf,
    phase: ValidatePhase,
    sidecar: SidecarLoad,
    replay: ReadJournalRecords,
    repair: FallbackSlot,
    repair_len: u64,
    replay_last_index: Option<u64>,
}

impl ValidateIndex {
    fn new(journal_path: PathBuf) -> Self {
        Self {
            sidecar: SidecarLoad::new(journal_path.clone()),
            replay: ReadJournalRecords::new(journal_path.clone()),
            journal_path,
            phase: ValidatePhase::LoadSidecar,
            repair: FallbackSlot::Idle,
            repair_len: 0,
            replay_last_index: None,
        }
    }

    fn has_outstanding_completion(&self) -> bool {
        self.sidecar.has_outstanding_completion() || self.replay.backend_owns()
    }

    fn awaited_ticket(&self) -> Option<u64> {
        match self.phase {
            ValidatePhase::RepairTail => self.repair.ticket(),
            _ => None,
        }
    }

    fn poll(
        &mut self,
        io: &IOLoopHandle<Global>,
        fb: &mut FallbackBridge<'_>,
        record_index: u64,
    ) -> Option<Result<(), CallError>> {
        loop {
            match self.phase {
                ValidatePhase::LoadSidecar => match self.sidecar.poll(io) {
                    None => return None,
                    Some(Err(reason)) => return Some(Err(reason)),
                    Some(Ok(Some(last))) => {
                        return if record_index <= last {
                            Some(Err(CallError::CorruptRecord))
                        } else {
                            Some(Ok(()))
                        };
                    }
                    Some(Ok(None)) => self.phase = ValidatePhase::ReplayJournal,
                },
                ValidatePhase::ReplayJournal => match self.replay.poll(io) {
                    None => return None,
                    Some(Err(reason)) => return Some(Err(reason)),
                    Some(Ok(replay)) => {
                        self.replay_last_index = replay.records.last().map(|record| record.index);
                        if let Some(crate::JournalReplayWarning::TruncatedTail {
                            valid_prefix_len,
                        }) = replay.warning
                        {
                            self.repair_len = valid_prefix_len;
                            self.phase = ValidatePhase::RepairTail;
                        } else if let Some(last) = self.replay_last_index
                            && record_index <= last
                        {
                            return Some(Err(CallError::CorruptRecord));
                        } else {
                            self.phase = ValidatePhase::Done;
                        }
                    }
                },
                ValidatePhase::RepairTail => {
                    let path = self.journal_path.clone();
                    let len = self.repair_len;
                    match poll_fallback_unit(fb, &mut self.repair, || FallbackOp::Truncate {
                        path,
                        len,
                    }) {
                        None => return None,
                        Some(Ok(())) => {
                            if let Some(last) = self.replay_last_index
                                && record_index <= last
                            {
                                return Some(Err(CallError::CorruptRecord));
                            }
                            self.phase = ValidatePhase::Done;
                        }
                        Some(Err(reason)) => return Some(Err(reason)),
                    }
                }
                ValidatePhase::Done => return Some(Ok(())),
            }
        }
    }
}

/// Loads the journal index sidecar and checks it against the journal's actual
/// length. Reproduces `persistence::load_journal_last_index`.
enum SidecarPhase {
    ReadSidecar,
    JournalSize,
    Done,
}

struct SidecarLoad {
    phase: SidecarPhase,
    reader: ReadFileBytes,
    journal_size: SizeOfFile,
    last_index: u64,
    expected_len: u64,
}

impl SidecarLoad {
    fn new(journal_path: PathBuf) -> Self {
        let index_path = crate::persistence::journal_index_path(&journal_path);
        Self {
            reader: ReadFileBytes::new(index_path),
            journal_size: SizeOfFile::new(journal_path),
            phase: SidecarPhase::ReadSidecar,
            last_index: 0,
            expected_len: 0,
        }
    }

    fn has_outstanding_completion(&self) -> bool {
        self.reader.in_flight.backend_owns() || self.journal_size.in_flight.backend_owns()
    }

    /// `Some(Ok(Some(last)))` trusted last index, `Some(Ok(None))` no usable
    /// sidecar, `Some(Err(_))` on I/O error.
    fn poll(&mut self, io: &IOLoopHandle<Global>) -> Option<Result<Option<u64>, CallError>> {
        loop {
            match self.phase {
                SidecarPhase::ReadSidecar => {
                    match self.reader.poll(io) {
                        None => return None,
                        Some(Err(reason)) => return Some(Err(reason)),
                        // Missing sidecar.
                        Some(Ok(None)) => return Some(Ok(None)),
                        Some(Ok(Some(bytes))) => {
                            match crate::persistence::parse_journal_index(&bytes) {
                                Some((last_index, expected_len)) => {
                                    self.last_index = last_index;
                                    self.expected_len = expected_len;
                                    self.phase = SidecarPhase::JournalSize;
                                }
                                None => return Some(Ok(None)),
                            }
                        }
                    }
                }
                SidecarPhase::JournalSize => match self.journal_size.poll(io) {
                    None => return None,
                    Some(Err(reason)) => return Some(Err(reason)),
                    // Journal missing => sidecar cannot be trusted.
                    Some(Ok(None)) => return Some(Ok(None)),
                    Some(Ok(Some(actual_len))) => {
                        self.phase = SidecarPhase::Done;
                        if actual_len != self.expected_len {
                            return Some(Ok(None));
                        }
                        return Some(Ok(Some(self.last_index)));
                    }
                },
                SidecarPhase::Done => unreachable!("sidecar load resolved before Done"),
            }
        }
    }
}

/// Reads a whole file's bytes; `None` result means the file does not exist.
struct ReadFileBytes {
    path: PathBuf,
    phase: ReadPhase,
    file: Option<Box<dyn IOFile>>,
    in_flight: InFlight,
    want: u64,
    acc: Vec<u8>,
}

impl ReadFileBytes {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            phase: ReadPhase::Start,
            file: None,
            in_flight: InFlight::None,
            want: 0,
            acc: Vec::new(),
        }
    }

    fn poll(&mut self, io: &IOLoopHandle<Global>) -> Option<Result<Option<Vec<u8>>, CallError>> {
        loop {
            match self.phase {
                ReadPhase::Start => match open_read(io, &self.path) {
                    Ok(file) => {
                        let mut completion = Box::new(SizeCompletion::new());
                        if file.size(&mut completion).is_err() {
                            return Some(Err(CallError::Io));
                        }
                        self.file = Some(file);
                        self.in_flight = InFlight::Size(completion);
                        self.phase = ReadPhase::Size;
                        return None;
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => return Some(Ok(None)),
                    Err(_) => return Some(Err(CallError::Io)),
                },
                ReadPhase::Size => {
                    let InFlight::Size(completion) = &mut self.in_flight else {
                        unreachable!("size phase holds a size completion")
                    };
                    let result = completion.take_result()?;
                    self.in_flight = InFlight::None;
                    match result {
                        Ok(0) => self.phase = ReadPhase::Done,
                        Ok(size) => {
                            self.want = size;
                            self.phase = ReadPhase::Read;
                        }
                        Err(_) => return Some(Err(CallError::Io)),
                    }
                }
                ReadPhase::Read => {
                    if let InFlight::Read(completion) = &mut self.in_flight {
                        let result = completion.take_result()?;
                        self.in_flight = InFlight::None;
                        match result {
                            Ok(bytes) if bytes.is_empty() => self.phase = ReadPhase::Done,
                            Ok(bytes) => {
                                self.acc.extend_from_slice(&bytes);
                                if self.acc.len() as u64 >= self.want {
                                    self.phase = ReadPhase::Done;
                                }
                            }
                            Err(_) => return Some(Err(CallError::Io)),
                        }
                    } else {
                        let remaining = self.want - self.acc.len() as u64;
                        let mut completion = Box::new(PReadCompletion::new());
                        if self
                            .file
                            .as_ref()
                            .expect("file open")
                            .pread(&mut completion, remaining as usize, self.acc.len() as u64)
                            .is_err()
                        {
                            return Some(Err(CallError::Io));
                        }
                        self.in_flight = InFlight::Read(completion);
                        return None;
                    }
                }
                ReadPhase::Done => {
                    self.file = None;
                    return Some(Ok(Some(std::mem::take(&mut self.acc))));
                }
            }
        }
    }
}

/// Replays a journal's records (read whole file + `replay_journal_bytes`),
/// treating a missing file as empty.
struct ReadJournalRecords {
    reader: ReadFileBytes,
}

impl ReadJournalRecords {
    fn new(path: PathBuf) -> Self {
        Self {
            reader: ReadFileBytes::new(path),
        }
    }

    fn backend_owns(&self) -> bool {
        self.reader.in_flight.backend_owns()
    }

    fn poll(
        &mut self,
        io: &IOLoopHandle<Global>,
    ) -> Option<Result<crate::JournalReplay, CallError>> {
        match self.reader.poll(io)? {
            Err(reason) => Some(Err(reason)),
            Ok(None) => Some(Ok(crate::JournalReplay {
                records: Vec::new(),
                warning: None,
            })),
            Ok(Some(bytes)) => Some(crate::persistence::replay_journal_bytes(&bytes)),
        }
    }
}

/// Reads a file's size; `None` result means the file does not exist.
struct SizeOfFile {
    path: PathBuf,
    started: bool,
    file: Option<Box<dyn IOFile>>,
    in_flight: InFlight,
}

impl SizeOfFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            started: false,
            file: None,
            in_flight: InFlight::None,
        }
    }

    fn poll(&mut self, io: &IOLoopHandle<Global>) -> Option<Result<Option<u64>, CallError>> {
        if !self.started {
            self.started = true;
            match open_read(io, &self.path) {
                Ok(file) => {
                    let mut completion = Box::new(SizeCompletion::new());
                    if file.size(&mut completion).is_err() {
                        return Some(Err(CallError::Io));
                    }
                    self.file = Some(file);
                    self.in_flight = InFlight::Size(completion);
                    return None;
                }
                Err(error) if error.kind() == ErrorKind::NotFound => return Some(Ok(None)),
                Err(_) => return Some(Err(CallError::Io)),
            }
        }
        let InFlight::Size(completion) = &mut self.in_flight else {
            unreachable!("size query holds a size completion once started")
        };
        let result = completion.take_result()?;
        self.in_flight = InFlight::None;
        self.file = None;
        Some(match result {
            Ok(size) => Ok(Some(size)),
            Err(_) => Err(CallError::Io),
        })
    }
}

/// Stores the journal index sidecar via temp-write + rename + parent fsync.
/// Reproduces `persistence::store_journal_last_index`.
enum StoreIndexPhase {
    WriteTemp,
    Rename,
    SyncParent,
}

struct StoreIndexJob {
    index_path: PathBuf,
    temp_path: PathBuf,
    phase: StoreIndexPhase,
    write: WriteNewFile,
    fallback: FallbackSlot,
    sync: SyncParentJob,
}

impl StoreIndexJob {
    fn new(journal_path: PathBuf, last_index: u64, file_len: u64) -> Self {
        let index_path = crate::persistence::journal_index_path(&journal_path);
        let temp_path = crate::persistence::temp_journal_index_path(&journal_path);
        let encoded = crate::persistence::encode_journal_index(last_index, file_len);
        let parent = parent_directory_owned(&index_path);
        Self {
            write: WriteNewFile::new(temp_path.clone(), encoded),
            temp_path,
            sync: SyncParentJob::for_dir(parent),
            index_path,
            phase: StoreIndexPhase::WriteTemp,
            fallback: FallbackSlot::Idle,
        }
    }

    fn has_outstanding_completion(&self) -> bool {
        self.write.in_flight.backend_owns() || self.sync.in_flight.backend_owns()
    }

    fn awaited_ticket(&self) -> Option<u64> {
        self.fallback.ticket()
    }

    fn poll(
        &mut self,
        io: &IOLoopHandle<Global>,
        fb: &mut FallbackBridge<'_>,
    ) -> Option<Result<(), CallError>> {
        loop {
            match self.phase {
                StoreIndexPhase::WriteTemp => match self.write.poll(io) {
                    None => return None,
                    Some(Ok(())) => self.phase = StoreIndexPhase::Rename,
                    Some(Err(reason)) => return Some(Err(self.cleanup_then(fb, reason))),
                },
                StoreIndexPhase::Rename => {
                    let from = self.temp_path.clone();
                    let to = self.index_path.clone();
                    match poll_fallback_unit(fb, &mut self.fallback, || FallbackOp::Rename {
                        from,
                        to,
                        internal: true,
                    }) {
                        None => return None,
                        Some(Ok(())) => self.phase = StoreIndexPhase::SyncParent,
                        Some(Err(_)) => return Some(Err(self.cleanup_then(fb, CallError::Io))),
                    }
                }
                StoreIndexPhase::SyncParent => match self.sync.poll_unit(io) {
                    None => return None,
                    Some(Ok(())) => return Some(Ok(())),
                    Some(Err(_)) => return Some(Err(CallError::CommitUncertain)),
                },
            }
        }
    }

    fn cleanup_then(&mut self, fb: &mut FallbackBridge<'_>, reason: CallError) -> CallError {
        fb.submit_detached(FallbackOp::Remove {
            path: self.temp_path.clone(),
            best_effort: true,
        });
        reason
    }
}

// --- Pure fallback metadata call (PathMetadata/Rename/Remove/ReadDir) --------

struct MetadataJob {
    op: FallbackOp,
    slot: FallbackSlot,
}

impl MetadataJob {
    fn new(op: FallbackOp) -> Self {
        Self {
            op,
            slot: FallbackSlot::Idle,
        }
    }

    fn poll(&mut self, fb: &mut FallbackBridge<'_>) -> Option<CallOutput> {
        match &self.slot {
            FallbackSlot::Idle => {
                // Submit a clone so a momentarily-full queue just retries next
                // advance; the queue is sized to capacity so admitted work is
                // accepted promptly.
                if let Some(ticket) = fb.submit(self.op.clone()) {
                    self.slot = FallbackSlot::Awaiting(ticket);
                }
                None
            }
            FallbackSlot::Awaiting(ticket) => {
                let reply = fb.take(*ticket)?;
                Some(match reply {
                    FallbackReply::Output(output) => output,
                    FallbackReply::Unit(_) => CallOutput::Failed(CallError::Io),
                })
            }
        }
    }
}

// --- Test-only park job to fill capacity deterministically ------------------

#[cfg(test)]
struct ParkJob {
    started: Option<SyncSender<()>>,
    release: Receiver<()>,
}

#[cfg(test)]
impl ParkJob {
    fn new(started: SyncSender<()>, release: Receiver<()>) -> Self {
        Self {
            started: Some(started),
            release,
        }
    }

    fn poll(&mut self) -> Option<CallOutput> {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
        }
        match self.release.try_recv() {
            Ok(()) => Some(CallOutput::DirectoryCreated),
            Err(_) => None,
        }
    }
}

// -----------------------------------------------------------------------------
// Inline path (explicit-step oracle): synchronous std::fs. Unchanged.
// -----------------------------------------------------------------------------

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
        StorageJob::RemoveFile { path } => remove_file_output(&path),
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

fn path_metadata_output(path: &Path) -> CallOutput {
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

fn rename_replace_output(from: &Path, to: &Path) -> CallOutput {
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

fn remove_file_output(path: &Path) -> CallOutput {
    match std::fs::remove_file(path) {
        Ok(()) => CallOutput::FileRemoved,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            CallOutput::Failed(CallError::NotFound)
        }
        Err(_) => CallOutput::Failed(CallError::Io),
    }
}

fn read_dir_output(path: &Path) -> CallOutput {
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

fn path_parent_or_current(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

// -----------------------------------------------------------------------------
// Reactor proofs.
//
// A small fault-injecting Betelgeuse-trait backend (real file I/O, but
// completion-based delivery with deterministic delay, fsync-failure injection,
// and a per-fd ordering log) lets these tests prove the ordering and
// CommitUncertain claims on the rail without touching vendor-betelgeuse.
// -----------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod reactor_proofs {
    // The completion model fills caller-owned slots via stored pointers, the
    // same mechanism the vendored Betelgeuse backends use. This test-only
    // backend mirrors that, so it needs the local unsafe the crate otherwise
    // denies.
    #![allow(unsafe_code)]

    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::os::unix::fs::FileExt;
    use std::rc::Rc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use betelgeuse::{FsyncOp, Operation, PReadOp, PWriteOp, SizeOp};

    fn unique_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tina-reactor-storage-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn drive(lane: &mut StorageLane, call_id: CallId) -> CallOutput {
        let mut completed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            lane.advance(&mut completed);
            if let Some(index) = completed.iter().position(|c| c.call_id == call_id) {
                return completed.remove(index).result;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("storage completion {call_id:?} did not arrive");
    }

    // ---- real-rail round trips ------------------------------------------

    #[test]
    fn journal_append_then_replay_round_trip_on_real_rail() {
        let dir = unique_dir("round-trip");
        let journal = dir.join("state.journal");
        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 4);

        let appended = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::JournalAppend {
                path: journal.clone(),
                record_index: 1,
                bytes: b"first".to_vec(),
            },
        );
        assert!(matches!(
            appended,
            CallOutput::JournalAppended { record_index: 1 }
        ));

        let replayed = drive_submit(
            &mut lane,
            CallId::new(2),
            StorageJob::JournalReplay {
                path: journal.clone(),
            },
        );
        let CallOutput::JournalReplayed { replay } = replayed else {
            panic!("expected replay, got {replayed:?}");
        };
        assert_eq!(replay.warning, None);
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.records[0].index, 1);
        assert_eq!(replay.records[0].bytes, b"first");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pure_betelgeuse_ops_use_no_fallback_worker() {
        let dir = unique_dir("no-worker");
        let journal = dir.join("state.journal");
        // Seed a journal on disk with the inline path (sidecar + record).
        crate::persistence::append_journal_record(&journal, 1, b"only".to_vec())
            .expect("seed journal");

        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 4);

        // Replay is open+size+pread: pure Betelgeuse, no fallback op.
        let replayed = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::JournalReplay {
                path: journal.clone(),
            },
        );
        assert!(matches!(replayed, CallOutput::JournalReplayed { .. }));
        assert!(
            !lane.fallback_worker_spawned(),
            "a pure Betelgeuse durability op must not spawn the fallback worker thread"
        );

        // Sync-parent is open(dir)+fsync: also pure Betelgeuse.
        let synced = drive_submit(
            &mut lane,
            CallId::new(2),
            StorageJob::SyncParent {
                path: journal.clone(),
            },
        );
        assert!(matches!(synced, CallOutput::ParentSynced));
        assert!(
            !lane.fallback_worker_spawned(),
            "sync-parent rides the reactor fsync, no fallback worker"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reactor_replay_torn_tail_and_corrupt_match_inline() {
        let dir = unique_dir("torn");
        let journal = dir.join("state.journal");
        let good = crate::persistence::encode_journal_record(&crate::JournalRecord {
            index: 1,
            bytes: b"good".to_vec(),
        });
        // Torn tail: a valid record plus trailing partial bytes.
        let mut torn = good.clone();
        torn.extend_from_slice(&[1, 2, 3]);
        std::fs::write(&journal, &torn).expect("write torn journal");

        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 4);
        let replayed = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::JournalReplay {
                path: journal.clone(),
            },
        );
        let CallOutput::JournalReplayed { replay } = replayed else {
            panic!("expected replay");
        };
        assert_eq!(replay.records.len(), 1);
        assert_eq!(
            replay.warning,
            Some(crate::JournalReplayWarning::TruncatedTail {
                valid_prefix_len: good.len() as u64,
            })
        );

        // Corrupt checksum: flip the last payload byte of a lone record.
        let mut corrupt = good;
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xff;
        std::fs::write(&journal, &corrupt).expect("write corrupt journal");
        let replayed = drive_submit(
            &mut lane,
            CallId::new(2),
            StorageJob::JournalReplay {
                path: journal.clone(),
            },
        );
        assert!(matches!(
            replayed,
            CallOutput::Failed(CallError::CorruptRecord)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reactor_append_duplicate_index_is_corrupt_record() {
        let dir = unique_dir("dup-index");
        let journal = dir.join("state.journal");
        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 4);

        let first = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::JournalAppend {
                path: journal.clone(),
                record_index: 2,
                bytes: b"second".to_vec(),
            },
        );
        assert!(matches!(
            first,
            CallOutput::JournalAppended { record_index: 2 }
        ));

        // A non-monotonic index is a duplicate/out-of-order append.
        let dup = drive_submit(
            &mut lane,
            CallId::new(2),
            StorageJob::JournalAppend {
                path: journal.clone(),
                record_index: 1,
                bytes: b"stale".to_vec(),
            },
        );
        assert!(matches!(dup, CallOutput::Failed(CallError::CorruptRecord)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reactor_rename_failure_is_io_not_uncertain() {
        // A snapshot commit whose destination is an existing directory cannot
        // rename into place. Durable state is known (old data intact), so the
        // outcome is Io — exactly the inline path, not CommitUncertain.
        let dir = unique_dir("rename-fail");
        let target = dir.join("state.snapshot");
        std::fs::create_dir(&target).expect("directory blocks the rename");

        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 4);
        let result = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::SnapshotCommit {
                path: target,
                bytes: b"payload".to_vec(),
                last_journal_index: 7,
            },
        );
        assert!(matches!(result, CallOutput::Failed(CallError::Io)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn storage_closed_after_shutdown() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 1);
        lane.cancel_pending(Instant::now());
        let closed = lane
            .submit(
                CallId::new(1),
                StorageJob::JournalReplay {
                    path: PathBuf::from("closed.journal"),
                },
            )
            .expect("submit after shutdown is rejected inline");
        assert!(matches!(
            closed.result,
            CallOutput::Failed(CallError::StorageClosed)
        ));
    }

    fn drive_submit(lane: &mut StorageLane, call_id: CallId, job: StorageJob) -> CallOutput {
        if let Some(completion) = lane.submit(call_id, job) {
            return completion.result;
        }
        drive(lane, call_id)
    }

    // ---- fault backend proofs -------------------------------------------

    #[test]
    fn pwrite_completion_harvested_before_fsync_submitted() {
        let dir = unique_dir("ordering");
        let journal = dir.join("ordering.journal");
        // Delay every completion so a journal append genuinely spans many
        // advances; a buggy lane that submitted fsync before harvesting the
        // pwrite would trip the per-fd ordering detector.
        let (mut lane, state) = fault_lane(
            FaultConfig {
                completion_delay: 2,
                ..FaultConfig::default()
            },
            4,
        );

        let result = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::JournalAppend {
                path: journal,
                record_index: 1,
                bytes: b"durable".to_vec(),
            },
        );
        assert!(matches!(
            result,
            CallOutput::JournalAppended { record_index: 1 }
        ));

        let st = state.borrow();
        assert!(
            !st.ordering_violation,
            "fsync was submitted while a pwrite on the same file was still in flight"
        );
        assert!(
            st.saw_pwrite_complete && st.saw_fsync_complete,
            "the append must have ridden real pwrite and fsync completions"
        );
        drop(st);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parent_fsync_failure_is_commit_uncertain() {
        let dir = unique_dir("uncertain");
        let snapshot = dir.join("state.snapshot");
        // Fail only the parent-directory fsync; the temp-file fsync still
        // succeeds, so the commit reaches the final durability step and finds
        // it unprovable.
        let (mut lane, _state) = fault_lane(
            FaultConfig {
                fail_fsync_paths: vec![dir.clone()],
                ..FaultConfig::default()
            },
            4,
        );

        let result = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::SnapshotCommit {
                path: snapshot,
                bytes: b"installed".to_vec(),
                last_journal_index: 9,
            },
        );
        assert!(matches!(
            result,
            CallOutput::Failed(CallError::CommitUncertain)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shutdown_returns_within_budget_with_stuck_completion() {
        let dir = unique_dir("stuck");
        let journal = dir.join("stuck.journal");
        crate::persistence::append_journal_record(&journal, 1, b"only".to_vec())
            .expect("seed journal");
        // A backend that never delivers a completion and never releases on
        // cancel: a replay stalls on its first size completion.
        let (mut lane, _state) = fault_lane(
            FaultConfig {
                never_complete: true,
                ..FaultConfig::default()
            },
            4,
        );
        assert!(
            lane.submit(CallId::new(1), StorageJob::JournalReplay { path: journal },)
                .is_none()
        );
        let mut completed = Vec::new();
        lane.advance(&mut completed); // opens + arms size; size never completes
        assert!(completed.is_empty());

        let budget = Duration::from_millis(50);
        let start = Instant::now();
        lane.cancel_pending(Instant::now() + budget);
        assert!(
            start.elapsed() < budget * 20,
            "bounded shutdown must return near the budget even when stuck"
        );
        assert!(
            lane.physical_pending_count() >= 1,
            "a job stuck on an unreleased Betelgeuse completion stays visible"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancelled_job_drops_once_its_in_flight_completion_lands() {
        // A job cancelled mid-op must not leak: once the backend produces the
        // result for its armed slot, the job drops and stops counting. (The
        // slot is never harvested, so a naive "slot present" retention check
        // would keep it forever.)
        let dir = unique_dir("cancel-drop");
        let journal = dir.join("c.journal");
        crate::persistence::append_journal_record(&journal, 1, b"x".to_vec()).expect("seed");
        let (mut lane, _state) = fault_lane(
            FaultConfig {
                completion_delay: 3,
                ..FaultConfig::default()
            },
            4,
        );
        assert!(
            lane.submit(
                CallId::new(1),
                StorageJob::JournalReplay {
                    path: journal.clone(),
                },
            )
            .is_none()
        );
        let mut completed = Vec::new();
        lane.advance(&mut completed); // open + arm size (delayed 3 steps)
        assert!(completed.is_empty());
        assert_eq!(lane.physical_pending_count(), 1);
        assert!(lane.cancel(CallId::new(1)));

        // Drive past the delay so the backend completes the armed slot.
        for _ in 0..50 {
            lane.advance(&mut completed);
            if lane.physical_pending_count() == 0 {
                break;
            }
        }
        assert_eq!(
            lane.physical_pending_count(),
            0,
            "cancelled job must drop once its in-flight completion lands"
        );
        assert!(
            completed.is_empty(),
            "cancelled work delivers no terminal completion"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_same_path_appends_serialize_and_do_not_corrupt() {
        // Two appends to the SAME journal admitted before either runs. Without
        // per-path serialization they would both size→pwrite-at-end from a
        // stale length and clobber each other. The reactor serializes them and
        // the journal replays cleanly to [1, 2].
        let dir = unique_dir("same-path-serialize");
        let journal = dir.join("state.journal");
        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 4);

        assert!(
            lane.submit(
                CallId::new(1),
                StorageJob::JournalAppend {
                    path: journal.clone(),
                    record_index: 1,
                    bytes: b"first".to_vec(),
                },
            )
            .is_none()
        );
        assert!(
            lane.submit(
                CallId::new(2),
                StorageJob::JournalAppend {
                    path: journal.clone(),
                    record_index: 2,
                    bytes: b"second".to_vec(),
                },
            )
            .is_none()
        );

        let mut completed = Vec::new();
        let mut seen = HashMap::new();
        for _ in 0..4000 {
            lane.advance(&mut completed);
            for done in completed.drain(..) {
                seen.insert(done.call_id, done.result);
            }
            if seen.len() == 2 {
                break;
            }
        }
        assert!(matches!(
            seen.get(&CallId::new(1)),
            Some(CallOutput::JournalAppended { record_index: 1 })
        ));
        assert!(matches!(
            seen.get(&CallId::new(2)),
            Some(CallOutput::JournalAppended { record_index: 2 })
        ));

        let replay = crate::persistence::replay_journal(&journal).expect("replay");
        assert_eq!(replay.warning, None);
        assert_eq!(
            replay
                .records
                .iter()
                .map(|record| (record.index, record.bytes.clone()))
                .collect::<Vec<_>>(),
            vec![(1, b"first".to_vec()), (2, b"second".to_vec())]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn partial_pwrite_and_pread_transfer_fully() {
        // One byte per completion forces every partial-transfer loop; the
        // record must still land whole and replay byte-for-byte.
        let dir = unique_dir("partial-io");
        let journal = dir.join("state.journal");
        let payload = b"a-multi-byte-journal-record".to_vec();
        let (mut lane, _state) = fault_lane(
            FaultConfig {
                partial_io: true,
                ..FaultConfig::default()
            },
            4,
        );
        let appended = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::JournalAppend {
                path: journal.clone(),
                record_index: 1,
                bytes: payload.clone(),
            },
        );
        assert!(matches!(
            appended,
            CallOutput::JournalAppended { record_index: 1 }
        ));
        let replayed = drive_submit(
            &mut lane,
            CallId::new(2),
            StorageJob::JournalReplay {
                path: journal.clone(),
            },
        );
        let CallOutput::JournalReplayed { replay } = replayed else {
            panic!("expected replay, got {replayed:?}");
        };
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.records[0].bytes, payload);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reactor_snapshot_load_corrupt_is_corrupt_record() {
        // A present-but-corrupt snapshot file reads as CorruptRecord on the
        // reactor, matching the inline `load_snapshot`.
        let dir = unique_dir("snapshot-corrupt");
        let snapshot = dir.join("state.snapshot");
        let mut bytes = crate::persistence::encode_snapshot(&crate::SnapshotImage {
            bytes: b"state".to_vec(),
            last_journal_index: 3,
        });
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff; // flip a payload byte: checksum mismatch
        std::fs::write(&snapshot, &bytes).expect("write corrupt snapshot");

        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 4);
        let result = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::SnapshotLoad {
                path: snapshot.clone(),
            },
        );
        assert!(matches!(
            result,
            CallOutput::Failed(CallError::CorruptRecord)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reactor_snapshot_commit_then_load_round_trips() {
        // The full successful snapshot path on the rail: encode → temp write +
        // fsync → rename → parent fsync, then open → size → pread → decode.
        let dir = unique_dir("snapshot-round-trip");
        let snapshot = dir.join("state.snapshot");
        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 4);

        let committed = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::SnapshotCommit {
                path: snapshot.clone(),
                bytes: b"durable-state".to_vec(),
                last_journal_index: 12,
            },
        );
        assert!(matches!(committed, CallOutput::SnapshotCommitted));

        let loaded = drive_submit(
            &mut lane,
            CallId::new(2),
            StorageJob::SnapshotLoad {
                path: snapshot.clone(),
            },
        );
        let CallOutput::SnapshotLoaded {
            snapshot: Some(image),
        } = loaded
        else {
            panic!("expected a loaded snapshot, got {loaded:?}");
        };
        assert_eq!(image.bytes, b"durable-state");
        assert_eq!(image.last_journal_index, 12);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reactor_append_to_non_directory_parent_is_io() {
        // A journal whose parent is a regular file cannot have its directory
        // created; the create-dir fallback leg fails and the append surfaces a
        // typed Io error (not a panic or hang), matching the inline path.
        let dir = unique_dir("bad-parent");
        let blocking = dir.join("not-a-dir");
        std::fs::write(&blocking, b"stone").expect("write blocking file");
        let journal = blocking.join("state.journal");
        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 4);

        let result = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::JournalAppend {
                path: journal,
                record_index: 1,
                bytes: b"first".to_vec(),
            },
        );
        assert!(matches!(result, CallOutput::Failed(CallError::Io)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reactor_write_failure_is_io_and_installs_nothing() {
        // A failing pwrite (e.g. full disk) on the snapshot temp surfaces Io,
        // and the rename never runs, so no snapshot is installed.
        let dir = unique_dir("write-fail");
        let snapshot = dir.join("state.snapshot");
        let (mut lane, _state) = fault_lane(
            FaultConfig {
                fail_pwrite: true,
                ..FaultConfig::default()
            },
            4,
        );
        let result = drive_submit(
            &mut lane,
            CallId::new(1),
            StorageJob::SnapshotCommit {
                path: snapshot.clone(),
                bytes: b"never-installed".to_vec(),
                last_journal_index: 1,
            },
        );
        assert!(matches!(result, CallOutput::Failed(CallError::Io)));
        assert!(
            !snapshot.exists(),
            "a failed temp write must not install a snapshot"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---- fault backend --------------------------------------------------

    #[derive(Default, Clone)]
    struct FaultConfig {
        completion_delay: u64,
        fail_fsync_paths: Vec<PathBuf>,
        never_complete: bool,
        /// Complete each pread/pwrite one byte at a time, forcing the lane's
        /// partial-transfer loops to re-issue until the whole buffer moves.
        partial_io: bool,
        /// Fail every pwrite (e.g. a full disk), so a write-family job reports
        /// a typed Io error instead of installing partial data.
        fail_pwrite: bool,
    }

    fn fault_lane(config: FaultConfig, capacity: usize) -> (StorageLane, Rc<RefCell<FaultState>>) {
        let state = Rc::new(RefCell::new(FaultState::new(config)));
        let io: Rc<dyn IOLoop> = Rc::new(FaultIo {
            state: Rc::clone(&state),
        });
        let handle = IOLoopHandle::new(io, Global);
        (StorageLane::reactor(handle, capacity), state)
    }

    enum FileOpKind {
        PRead { len: usize, offset: u64 },
        PWrite { buf: Vec<u8>, offset: u64 },
        Fsync,
        Size,
    }

    struct PendingFileOp {
        fd: i32,
        kind: FileOpKind,
        completion: usize,
        delay: u64,
    }

    struct FaultState {
        config: FaultConfig,
        next_fd: i32,
        files: HashMap<i32, std::fs::File>,
        paths: HashMap<i32, PathBuf>,
        pending: VecDeque<PendingFileOp>,
        outstanding_pwrite: HashSet<i32>,
        ordering_violation: bool,
        saw_pwrite_complete: bool,
        saw_fsync_complete: bool,
    }

    impl FaultState {
        fn new(config: FaultConfig) -> Self {
            Self {
                config,
                next_fd: 1,
                files: HashMap::new(),
                paths: HashMap::new(),
                pending: VecDeque::new(),
                outstanding_pwrite: HashSet::new(),
                ordering_violation: false,
                saw_pwrite_complete: false,
                saw_fsync_complete: false,
            }
        }
    }

    struct FaultIo {
        state: Rc<RefCell<FaultState>>,
    }

    struct FaultFile {
        state: Rc<RefCell<FaultState>>,
        fd: i32,
    }

    impl IOFile for FaultFile {
        fn pread(&self, c: &mut PReadCompletion, len: usize, offset: u64) -> io::Result<()> {
            c.inner_mut().prepare(Operation::PRead(PReadOp {
                fd: self.fd,
                buf: Vec::new(),
                offset,
            }));
            let mut st = self.state.borrow_mut();
            let delay = st.config.completion_delay;
            st.pending.push_back(PendingFileOp {
                fd: self.fd,
                kind: FileOpKind::PRead { len, offset },
                completion: c as *mut PReadCompletion as usize,
                delay,
            });
            Ok(())
        }

        fn pwrite(&self, c: &mut PWriteCompletion, buf: Vec<u8>, offset: u64) -> io::Result<()> {
            c.inner_mut().prepare(Operation::PWrite(PWriteOp {
                fd: self.fd,
                buf: Vec::new(),
                offset,
            }));
            let mut st = self.state.borrow_mut();
            let delay = st.config.completion_delay;
            st.outstanding_pwrite.insert(self.fd);
            st.pending.push_back(PendingFileOp {
                fd: self.fd,
                kind: FileOpKind::PWrite { buf, offset },
                completion: c as *mut PWriteCompletion as usize,
                delay,
            });
            Ok(())
        }

        fn fsync(&self, c: &mut FsyncCompletion) -> io::Result<()> {
            c.inner_mut()
                .prepare(Operation::Fsync(FsyncOp { fd: self.fd }));
            let mut st = self.state.borrow_mut();
            // Ordering rule: a pwrite on this file must be harvested (and so
            // completed by the backend) before fsync is submitted.
            if st.outstanding_pwrite.contains(&self.fd) {
                st.ordering_violation = true;
            }
            let delay = st.config.completion_delay;
            st.pending.push_back(PendingFileOp {
                fd: self.fd,
                kind: FileOpKind::Fsync,
                completion: c as *mut FsyncCompletion as usize,
                delay,
            });
            Ok(())
        }

        fn size(&self, c: &mut SizeCompletion) -> io::Result<()> {
            c.inner_mut()
                .prepare(Operation::Size(SizeOp { fd: self.fd }));
            let mut st = self.state.borrow_mut();
            let delay = st.config.completion_delay;
            st.pending.push_back(PendingFileOp {
                fd: self.fd,
                kind: FileOpKind::Size,
                completion: c as *mut SizeCompletion as usize,
                delay,
            });
            Ok(())
        }
    }

    impl IO for FaultIo {
        fn open(&self, path: &Path, options: OpenOptions) -> io::Result<Box<dyn IOFile>> {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(options.read)
                .write(options.write)
                .create(options.create)
                .truncate(options.truncate);
            let file = opts.open(path)?;
            let mut st = self.state.borrow_mut();
            let fd = st.next_fd;
            st.next_fd += 1;
            st.files.insert(fd, file);
            st.paths.insert(fd, path.to_path_buf());
            Ok(Box::new(FaultFile {
                state: Rc::clone(&self.state),
                fd,
            }))
        }

        fn socket(&self) -> io::Result<Box<dyn IOSocket>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "fault backend has no sockets",
            ))
        }

        fn mkdir(&self, _c: &mut MkdirCompletion, path: &Path, _mode: u32) -> io::Result<()> {
            // Storage never issues a Betelgeuse mkdir, but keep the trait honest.
            std::fs::create_dir(path).map_err(|_| io::Error::other("fault backend mkdir failed"))
        }

        fn backend_name(&self) -> &'static str {
            "fault"
        }
    }

    impl IOLoop for FaultIo {
        fn step(&self) -> io::Result<bool> {
            let mut st = self.state.borrow_mut();
            if st.config.never_complete {
                return Ok(false);
            }
            let drained: Vec<PendingFileOp> = st.pending.drain(..).collect();
            let mut kept = VecDeque::new();
            let mut progressed = false;
            for mut op in drained {
                if op.delay > 0 {
                    op.delay -= 1;
                    kept.push_back(op);
                    progressed = true;
                    continue;
                }
                execute_op(&mut st, &op);
                progressed = true;
            }
            st.pending = kept;
            Ok(progressed)
        }
    }

    fn execute_op(st: &mut FaultState, op: &PendingFileOp) {
        match &op.kind {
            FileOpKind::PRead { len, offset } => {
                // Partial mode yields one byte per completion, exercising the
                // lane's short-read accumulation loop.
                let cap = if st.config.partial_io {
                    (*len).min(1)
                } else {
                    *len
                };
                let result = {
                    let file = st.files.get(&op.fd).expect("fd open");
                    let mut buf = vec![0_u8; cap];
                    match file.read_at(&mut buf, *offset) {
                        Ok(n) => {
                            buf.truncate(n);
                            Ok(buf)
                        }
                        Err(error) => Err(error),
                    }
                };
                unsafe { &mut *(op.completion as *mut PReadCompletion) }.complete(result);
            }
            FileOpKind::PWrite { buf, offset } => {
                // Partial mode writes one byte per completion, exercising the
                // lane's partial-write re-issue loop.
                let chunk = if st.config.partial_io {
                    &buf[..buf.len().min(1)]
                } else {
                    &buf[..]
                };
                let result = if st.config.fail_pwrite {
                    Err(io::Error::other("injected pwrite failure"))
                } else {
                    let file = st.files.get(&op.fd).expect("fd open");
                    file.write_at(chunk, *offset)
                };
                st.outstanding_pwrite.remove(&op.fd);
                st.saw_pwrite_complete = true;
                unsafe { &mut *(op.completion as *mut PWriteCompletion) }.complete(result);
            }
            FileOpKind::Fsync => {
                let path = st.paths.get(&op.fd).cloned().unwrap_or_default();
                let result = if st.config.fail_fsync_paths.iter().any(|p| p == &path) {
                    Err(io::Error::other("injected fsync failure"))
                } else {
                    let file = st.files.get(&op.fd).expect("fd open");
                    file.sync_all()
                };
                st.saw_fsync_complete = true;
                unsafe { &mut *(op.completion as *mut FsyncCompletion) }.complete(result);
            }
            FileOpKind::Size => {
                let result = {
                    let file = st.files.get(&op.fd).expect("fd open");
                    file.metadata().map(|m| m.len())
                };
                unsafe { &mut *(op.completion as *mut SizeCompletion) }.complete(result);
            }
        }
    }
}
