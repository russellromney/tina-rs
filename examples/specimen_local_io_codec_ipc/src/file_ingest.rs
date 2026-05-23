//! Bounded file streaming using the simulator's file rails and
//! `tina_runtime::FileReadChunks`.

use std::cell::RefCell;
use std::convert::Infallible;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use tina::{Context, Effect, Isolate, Outbound, Shard, ShardId};
use tina_runtime::{
    CallError, FileId, FileLoopEnd, FileLoopReport, FileLoopStep, FileOpenOptions, FileReadChunks,
    FileReadReply, FileWriteReply, RuntimeCall, file_close, file_open,
};
use tina_sim::{Simulator, SimulatorConfig};

use crate::SpecimenReport;

#[derive(Debug, Default)]
pub struct IngestShard;

impl Shard for IngestShard {
    fn id(&self) -> ShardId {
        ShardId::new(101)
    }
}

#[derive(Debug)]
enum IngestMsg {
    Start,
    Opened(Result<FileId, CallError>),
    Read(FileReadReply),
    Closed,
}

type IngestFinishedSlot = Arc<Mutex<Option<(Vec<u8>, FileLoopReport)>>>;

struct Ingest {
    path: PathBuf,
    chunk: usize,
    cap: u64,
    helper: Option<FileReadChunks>,
    file: Option<FileId>,
    finished: IngestFinishedSlot,
}

impl Isolate for Ingest {
    type Message = IngestMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<IngestMsg>;
    type Fact = Infallible;
    type Shard = IngestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            IngestMsg::Start => {
                file_open(self.path.clone(), FileOpenOptions::read_only()).then(IngestMsg::Opened)
            }
            IngestMsg::Opened(Ok(file)) => {
                self.file = Some(file);
                let helper = FileReadChunks::new(file, 0, self.chunk, self.cap);
                let effect = helper
                    .next_effect(IngestMsg::Read)
                    .unwrap_or_else(tina::noop);
                self.helper = Some(helper);
                effect
            }
            IngestMsg::Opened(Err(_)) => Effect::Stop,
            IngestMsg::Read(reply) => {
                let helper = self.helper.as_mut().expect("helper armed");
                match helper.advance::<Self, _, _>(reply, IngestMsg::Read) {
                    FileLoopStep::Pending(effect) => effect,
                    FileLoopStep::Done(buffer, report) => {
                        *self.finished.lock().unwrap() = Some((buffer, report));
                        self.helper = None;
                        if let Some(file) = self.file.take() {
                            file_close(file).then(|_| IngestMsg::Closed)
                        } else {
                            Effect::Stop
                        }
                    }
                    FileLoopStep::Ended(report) => {
                        *self.finished.lock().unwrap() = Some((Vec::new(), report));
                        Effect::Stop
                    }
                }
            }
            IngestMsg::Closed => Effect::Stop,
        }
    }
}

/// Result of running the file-ingest specimen.
#[derive(Debug, Clone)]
pub struct IngestRunReport {
    /// The bytes the helper assembled (or empty if it ended without
    /// producing a payload).
    pub bytes: Vec<u8>,
    /// The terminal report from the file-loop helper.
    pub helper: FileLoopReport,
}

/// Build a simulator pre-loaded with `payload` at `path` and run an
/// ingest isolate that bounded-streams it back through Tina rails.
pub fn run_ingest(path: PathBuf, payload: Vec<u8>, chunk: usize, cap: u64) -> IngestRunReport {
    let mut config = SimulatorConfig::default();
    config.tcp.pending_completion_capacity = 64;
    let mut sim = Simulator::new(IngestShard, config);
    // Pre-load the file by directly seeding the simulator's path-backed
    // file storage. This sidesteps having a writer isolate for the
    // specimen's setup.
    seed_file(&mut sim, &path, payload);
    let finished = Arc::new(Mutex::new(None));
    let ingest = Ingest {
        path: path.clone(),
        chunk,
        cap,
        helper: None,
        file: None,
        finished: Arc::clone(&finished),
    };
    let addr = sim.register(ingest);
    sim.try_send(addr, IngestMsg::Start).unwrap();
    sim.run_until_quiescent();
    let guard = finished.lock().unwrap();
    let (bytes, report) = guard.clone().expect("ingest finished");
    IngestRunReport {
        bytes,
        helper: report,
    }
}

/// Pre-load the simulator's path-backed storage with `bytes` at `path`
/// by running a one-shot writer isolate on the same shard as the
/// reader (`IngestShard`). Production isolates would do this through
/// the normal file rails too — this helper just keeps the specimen
/// self-contained.
fn seed_file(sim: &mut Simulator<IngestShard>, path: &std::path::Path, bytes: Vec<u8>) {
    use tina_runtime::file_write_at;

    #[derive(Debug)]
    enum SeederMsg {
        Start,
        Opened(Result<FileId, CallError>),
        Wrote(Result<usize, CallError>),
        Closed,
    }

    struct Seeder {
        path: PathBuf,
        bytes: Rc<RefCell<Vec<u8>>>,
        file: Option<FileId>,
        offset: u64,
    }

    impl Isolate for Seeder {
        type Message = SeederMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = Infallible;
        type Call = RuntimeCall<SeederMsg>;
        type Fact = Infallible;
        type Shard = IngestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SeederMsg::Start => file_open(
                    self.path.clone(),
                    FileOpenOptions::read_write_create_truncate(),
                )
                .then(SeederMsg::Opened),
                SeederMsg::Opened(Ok(file)) => {
                    self.file = Some(file);
                    let payload = self.bytes.borrow().clone();
                    if payload.is_empty() {
                        file_close(file).then(|_| SeederMsg::Closed)
                    } else {
                        file_write_at(file, payload, self.offset).then(SeederMsg::Wrote)
                    }
                }
                SeederMsg::Opened(Err(_)) => Effect::Stop,
                SeederMsg::Wrote(Ok(count)) => {
                    let mut bytes = self.bytes.borrow_mut();
                    if count >= bytes.len() {
                        bytes.clear();
                    } else {
                        let _ = bytes.drain(..count);
                    }
                    self.offset += count as u64;
                    if bytes.is_empty() {
                        let file = self.file.take().expect("file open");
                        file_close(file).then(|_| SeederMsg::Closed)
                    } else {
                        let payload = bytes.clone();
                        drop(bytes);
                        file_write_at(self.file.expect("file open"), payload, self.offset)
                            .then(SeederMsg::Wrote)
                    }
                }
                SeederMsg::Wrote(Err(_)) => Effect::Stop,
                SeederMsg::Closed => Effect::Stop,
            }
        }
    }

    let seeder = Seeder {
        path: path.to_path_buf(),
        bytes: Rc::new(RefCell::new(bytes)),
        file: None,
        offset: 0,
    };
    let addr = sim.register(seeder);
    sim.try_send(addr, SeederMsg::Start).unwrap();
    sim.run_until_quiescent();
}

/// Convenience wrapper that returns a [`SpecimenReport`] for the smoke
/// command.
pub fn smoke() -> SpecimenReport {
    let payload = b"the quick brown fox jumps over the lazy dog".to_vec();
    let result = run_ingest(
        PathBuf::from("/tmp/specimen_file_ingest.dat"),
        payload,
        8,
        64,
    );
    SpecimenReport {
        name: "file_ingest",
        bytes: result.helper.bytes_transferred,
        frames: result
            .helper
            .high_water_chunk
            .max(1)
            .try_into()
            .unwrap_or(0),
        ok: matches!(result.helper.end, FileLoopEnd::Eof),
        note: format!(
            "end={:?} high_water_chunk={} final_offset={}",
            result.helper.end, result.helper.high_water_chunk, result.helper.final_offset,
        ),
    }
}

/// Bad-input proof: cap that is shorter than the file forces
/// `FileLoopEnd::CapReached`, NOT a fake all-or-nothing success.
pub fn bad_input_cap_reached() -> SpecimenReport {
    let payload = b"AAAAAAAAAAAAAAAAAAAAAAAA".to_vec();
    let result = run_ingest(
        PathBuf::from("/tmp/specimen_file_ingest_cap.dat"),
        payload,
        4,
        8,
    );
    SpecimenReport {
        name: "file_ingest:cap_reached",
        bytes: result.helper.bytes_transferred,
        frames: 1,
        // For the "honest cap" proof, ok means the helper reported
        // CapReached without lying about completion.
        ok: matches!(result.helper.end, FileLoopEnd::CapReached) && result.bytes.len() == 8,
        note: format!("end={:?} bytes={}", result.helper.end, result.bytes.len()),
    }
}

// ---------------------------------------------------------------------------
// FileCopyBounded: pump bytes src -> dst, bounded, through the unified
// advance/next_effect shape.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum CopyMsg {
    Start,
    SrcOpened(Result<FileId, CallError>),
    DstOpened(Result<FileId, CallError>),
    Read(FileReadReply),
    Wrote(FileWriteReply),
    DstReadBack(FileReadReply),
    Closed,
}

type CopyFinishedSlot = Arc<Mutex<Option<FileLoopReport>>>;
type CopyContentsSlot = Arc<Mutex<Vec<u8>>>;

struct CopyPump {
    src_path: PathBuf,
    dst_path: PathBuf,
    chunk: usize,
    cap: u64,
    src: Option<FileId>,
    dst: Option<FileId>,
    pump: Option<tina_runtime::FileCopyBounded>,
    finished: CopyFinishedSlot,
    dst_contents: CopyContentsSlot,
}

impl CopyPump {
    fn drive(&mut self) -> Effect<Self> {
        let next = {
            let pump = self.pump.as_ref().expect("pump armed");
            pump.next_effect(CopyMsg::Read, CopyMsg::Wrote)
                .map(Ok)
                .unwrap_or_else(|| Err(pump.report(FileLoopEnd::CapReached)))
        };
        match next {
            Ok(effect) => effect,
            Err(report) => self.finish(report),
        }
    }

    fn finish(&mut self, report: FileLoopReport) -> Effect<Self> {
        *self.finished.lock().unwrap() = Some(report);
        self.pump = None;
        // Read the destination back to prove the bytes actually landed.
        let dst = self.dst.expect("dst open");
        tina_runtime::file_read_at(dst, 4096, 0).then(CopyMsg::DstReadBack)
    }
}

impl Isolate for CopyPump {
    type Message = CopyMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<CopyMsg>;
    type Fact = Infallible;
    type Shard = IngestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CopyMsg::Start => file_open(self.src_path.clone(), FileOpenOptions::read_only())
                .then(CopyMsg::SrcOpened),
            CopyMsg::SrcOpened(Ok(src)) => {
                self.src = Some(src);
                file_open(
                    self.dst_path.clone(),
                    FileOpenOptions::read_write_create_truncate(),
                )
                .then(CopyMsg::DstOpened)
            }
            CopyMsg::SrcOpened(Err(_)) => Effect::Stop,
            CopyMsg::DstOpened(Ok(dst)) => {
                self.dst = Some(dst);
                self.pump = Some(tina_runtime::FileCopyBounded::new(
                    self.src.expect("src"),
                    dst,
                    0,
                    0,
                    self.chunk,
                    self.cap,
                ));
                self.drive()
            }
            CopyMsg::DstOpened(Err(_)) => Effect::Stop,
            CopyMsg::Read(reply) => {
                let pump = self.pump.as_mut().expect("pump armed");
                match pump.advance(
                    tina_runtime::FileCopyProgress::Read(reply),
                    CopyMsg::Read,
                    CopyMsg::Wrote,
                ) {
                    tina_runtime::FileCopyStep::Pending(effect) => effect,
                    tina_runtime::FileCopyStep::Done(report) => self.finish(report),
                }
            }
            CopyMsg::Wrote(reply) => {
                let pump = self.pump.as_mut().expect("pump armed");
                match pump.advance(
                    tina_runtime::FileCopyProgress::Write(reply),
                    CopyMsg::Read,
                    CopyMsg::Wrote,
                ) {
                    tina_runtime::FileCopyStep::Pending(effect) => effect,
                    tina_runtime::FileCopyStep::Done(report) => self.finish(report),
                }
            }
            CopyMsg::DstReadBack(Ok(bytes)) => {
                *self.dst_contents.lock().unwrap() = bytes;
                let mut effects = Vec::new();
                if let Some(src) = self.src.take() {
                    effects.push(file_close(src).then(|_| CopyMsg::Closed));
                }
                if let Some(dst) = self.dst.take() {
                    effects.push(file_close(dst).then(|_| CopyMsg::Closed));
                }
                match effects.len() {
                    0 => Effect::Stop,
                    _ => tina::batch(effects),
                }
            }
            CopyMsg::DstReadBack(Err(_)) => Effect::Stop,
            CopyMsg::Closed => Effect::Stop,
        }
    }
}

/// Result of a bounded copy run.
#[derive(Debug, Clone)]
pub struct CopyRun {
    /// The destination file contents after the copy.
    pub dst_contents: Vec<u8>,
    /// The terminal copy report.
    pub report: FileLoopReport,
}

/// Seed `payload` at a source path, then bounded-copy it to a
/// destination path via [`tina_runtime::FileCopyBounded`].
pub fn run_copy(payload: Vec<u8>, chunk: usize, cap: u64) -> CopyRun {
    let src_path = PathBuf::from("/tmp/specimen_file_copy_src.dat");
    let dst_path = PathBuf::from("/tmp/specimen_file_copy_dst.dat");
    let mut config = SimulatorConfig::default();
    config.tcp.pending_completion_capacity = 64;
    let mut sim = Simulator::new(IngestShard, config);
    seed_file(&mut sim, &src_path, payload);

    let finished: CopyFinishedSlot = Arc::new(Mutex::new(None));
    let dst_contents: CopyContentsSlot = Arc::new(Mutex::new(Vec::new()));
    let pump = CopyPump {
        src_path,
        dst_path,
        chunk,
        cap,
        src: None,
        dst: None,
        pump: None,
        finished: Arc::clone(&finished),
        dst_contents: Arc::clone(&dst_contents),
    };
    let addr = sim.register(pump);
    sim.try_send(addr, CopyMsg::Start).unwrap();
    sim.run_until_quiescent();

    CopyRun {
        dst_contents: dst_contents.lock().unwrap().clone(),
        report: finished.lock().unwrap().clone().expect("copy finished"),
    }
}

/// Smoke for the bounded copy pump: copy a payload smaller than the cap
/// end to end and confirm the destination matches.
pub fn copy_smoke() -> SpecimenReport {
    let payload = b"copy me through a bounded two-FD pump".to_vec();
    let result = run_copy(payload.clone(), 8, 1024);
    SpecimenReport {
        name: "file_copy",
        bytes: result.report.bytes_transferred,
        frames: 1,
        ok: matches!(result.report.end, FileLoopEnd::Eof) && result.dst_contents == payload,
        note: format!(
            "end={:?} transferred={} dst_len={}",
            result.report.end,
            result.report.bytes_transferred,
            result.dst_contents.len(),
        ),
    }
}
