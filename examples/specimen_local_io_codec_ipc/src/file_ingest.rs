//! Bounded file streaming and copying over simulator file rails.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tina::{Effect, Shard, ShardId, stop_with};
use tina_runtime::{
    CallError, FileCopyBounded, FileCopyProgress, FileCopyStep, FileId, FileLoopEnd,
    FileLoopReport, FileLoopStep, FileOpenOptions, FileReadChunks, FileReadReply, FileWriteAll,
    FileWriteOwnedReply, file_close, file_open,
};
use tina_sim::{Simulator, SimulatorConfig};

use crate::{RunError, SpecimenReport, map_start, wait_actor};

#[derive(Debug, Default)]
pub struct IngestShard;

impl Shard for IngestShard {
    fn id(&self) -> ShardId {
        ShardId::new(101)
    }
}

/// Exact file operation represented by a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStage {
    SeedOpen,
    SeedWrite,
    SeedClose,
    IngestOpen,
    IngestRead,
    IngestClose,
    SourceOpen,
    DestinationOpen,
    Copy,
    DestinationReadBack,
    SourceClose,
    DestinationClose,
}

/// Typed detail for one file failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileIssueKind {
    Call(CallError),
    Loop {
        end: FileLoopEnd,
        error: Option<CallError>,
    },
}

/// One exact primary or cleanup failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIssue {
    pub stage: FileStage,
    pub error: FileIssueKind,
}

/// All failures produced before an actor released its file resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFailure {
    pub issues: Vec<FileIssue>,
}

impl std::fmt::Display for FileFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}", self.issues)
    }
}

impl std::error::Error for FileFailure {}

/// Host-side failure from a file specimen.
pub type FileRunError = RunError<FileFailure>;

#[derive(Debug)]
enum SeederMsg {
    Start,
    Opened(Result<FileId, CallError>),
    Wrote(FileWriteOwnedReply),
    Closed(Result<(), CallError>),
}

struct Seeder {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    file: Option<FileId>,
    helper: Option<FileWriteAll>,
    issues: Vec<FileIssue>,
}

impl Seeder {
    fn issue(&mut self, stage: FileStage, error: FileIssueKind) {
        self.issues.push(FileIssue { stage, error });
    }

    fn close(&mut self) -> Effect<Self> {
        self.helper = None;
        if let Some(file) = self.file.take() {
            file_close(file).then(SeederMsg::Closed)
        } else {
            self.publish()
        }
    }

    fn publish(&mut self) -> Effect<Self> {
        if self.issues.is_empty() {
            stop_with(Ok::<(), FileFailure>(()))
        } else {
            stop_with(Err::<(), _>(FileFailure {
                issues: std::mem::take(&mut self.issues),
            }))
        }
    }
}

#[tina_runtime::isolate(message = SeederMsg, shard = IngestShard)]
impl Seeder {
    fn handle(
        &mut self,
        msg: SeederMsg,
        _ctx: &mut Context<'_, IngestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SeederMsg::Start => file_open(
                self.path.clone(),
                FileOpenOptions::read_write_create_truncate(),
            )
            .then(SeederMsg::Opened),
            SeederMsg::Opened(Ok(file)) => {
                self.file = Some(file);
                let payload = self.bytes.take().expect("seed payload available");
                if payload.is_empty() {
                    self.close()
                } else {
                    let mut helper = FileWriteAll::new(file, 0, payload);
                    let effect = helper
                        .next_effect(SeederMsg::Wrote)
                        .expect("non-empty seed payload has a write effect");
                    self.helper = Some(helper);
                    effect
                }
            }
            SeederMsg::Opened(Err(error)) => {
                self.issue(FileStage::SeedOpen, FileIssueKind::Call(error));
                self.publish()
            }
            SeederMsg::Wrote(reply) => {
                let helper = self.helper.as_mut().expect("seed writer armed");
                match helper.advance::<Self, _, _>(reply, SeederMsg::Wrote) {
                    FileLoopStep::Pending(effect) => effect,
                    FileLoopStep::Ended(report) => {
                        if report.end != FileLoopEnd::Done {
                            self.issue(
                                FileStage::SeedWrite,
                                FileIssueKind::Loop {
                                    end: report.end,
                                    error: report.error,
                                },
                            );
                        }
                        self.close()
                    }
                    FileLoopStep::Done((), _) => {
                        unreachable!("write-all has no payload result")
                    }
                }
            }
            SeederMsg::Closed(result) => {
                if let Err(error) = result {
                    self.issue(FileStage::SeedClose, FileIssueKind::Call(error));
                }
                self.publish()
            }
        }
    }
}

fn seed_file(
    sim: &mut Simulator<IngestShard>,
    path: &Path,
    bytes: Vec<u8>,
) -> Result<(), FileRunError> {
    let seeder = sim.register(Seeder {
        path: path.to_path_buf(),
        bytes: Some(bytes),
        file: None,
        helper: None,
        issues: Vec::new(),
    });
    let waiter = sim
        .observe_result::<Result<(), FileFailure>, _, _>(seeder)
        .map_err(|error| RunError::Observe {
            actor: "file seeder",
            error,
        })?;
    map_start::<_, FileFailure>("file seeder", sim.try_send(seeder, SeederMsg::Start))?;
    sim.run_until_quiescent();
    if sim.has_in_flight_calls() {
        return Err(RunError::InFlightCalls);
    }
    wait_actor("file seeder", waiter, Duration::ZERO)
}

#[derive(Debug)]
enum IngestMsg {
    Start,
    Opened(Result<FileId, CallError>),
    Read(FileReadReply),
    Closed(Result<(), CallError>),
}

struct Ingest {
    path: PathBuf,
    chunk: usize,
    cap: u64,
    helper: Option<FileReadChunks>,
    file: Option<FileId>,
    result: Option<IngestRunReport>,
    chunks: u64,
    issues: Vec<FileIssue>,
}

impl Ingest {
    fn issue(&mut self, stage: FileStage, error: FileIssueKind) {
        self.issues.push(FileIssue { stage, error });
    }

    fn finish(&mut self, bytes: Vec<u8>, report: FileLoopReport) -> Effect<Self> {
        if matches!(report.end, FileLoopEnd::Error | FileLoopEnd::StuckWrite) {
            self.issue(
                FileStage::IngestRead,
                FileIssueKind::Loop {
                    end: report.end,
                    error: report.error,
                },
            );
        }
        self.result = Some(IngestRunReport {
            bytes,
            helper: report,
            chunks: self.chunks,
        });
        self.helper = None;
        if let Some(file) = self.file.take() {
            file_close(file).then(IngestMsg::Closed)
        } else {
            self.publish()
        }
    }

    fn publish(&mut self) -> Effect<Self> {
        if self.issues.is_empty() {
            stop_with(Ok::<_, FileFailure>(
                self.result.take().expect("successful ingest has a report"),
            ))
        } else {
            stop_with(Err::<IngestRunReport, _>(FileFailure {
                issues: std::mem::take(&mut self.issues),
            }))
        }
    }
}

#[tina_runtime::isolate(message = IngestMsg, shard = IngestShard)]
impl Ingest {
    fn handle(
        &mut self,
        msg: IngestMsg,
        _ctx: &mut Context<'_, IngestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            IngestMsg::Start => {
                file_open(self.path.clone(), FileOpenOptions::read_only()).then(IngestMsg::Opened)
            }
            IngestMsg::Opened(Ok(file)) => {
                self.file = Some(file);
                let helper = FileReadChunks::new(file, 0, self.chunk, self.cap);
                if let Some(effect) = helper.next_effect(IngestMsg::Read) {
                    self.helper = Some(helper);
                    effect
                } else {
                    self.helper = Some(helper);
                    let report = self
                        .helper
                        .as_ref()
                        .expect("helper stored")
                        .report(FileLoopEnd::CapReached);
                    self.finish(Vec::new(), report)
                }
            }
            IngestMsg::Opened(Err(error)) => {
                self.issue(FileStage::IngestOpen, FileIssueKind::Call(error));
                self.publish()
            }
            IngestMsg::Read(reply) => {
                if matches!(&reply, Ok(bytes) if !bytes.is_empty()) {
                    self.chunks += 1;
                }
                let helper = self.helper.as_mut().expect("ingest helper armed");
                match helper.advance::<Self, _, _>(reply, IngestMsg::Read) {
                    FileLoopStep::Pending(effect) => effect,
                    FileLoopStep::Done(bytes, report) => self.finish(bytes, report),
                    FileLoopStep::Ended(report) => self.finish(Vec::new(), report),
                }
            }
            IngestMsg::Closed(result) => {
                if let Err(error) = result {
                    self.issue(FileStage::IngestClose, FileIssueKind::Call(error));
                }
                self.publish()
            }
        }
    }
}

/// Result of running the file-ingest specimen.
#[derive(Debug, Clone)]
pub struct IngestRunReport {
    /// Bytes assembled by the bounded helper.
    pub bytes: Vec<u8>,
    /// Terminal report from the file-loop helper.
    pub helper: FileLoopReport,
    /// Non-empty read completions processed.
    pub chunks: u64,
}

/// Seed and bounded-stream one simulated file.
pub fn run_ingest(
    path: PathBuf,
    payload: Vec<u8>,
    chunk: usize,
    cap: u64,
) -> Result<IngestRunReport, FileRunError> {
    if chunk == 0 {
        return Err(RunError::InvalidConfig("chunk must be greater than zero"));
    }
    let mut config = SimulatorConfig::default();
    config.storage.file_write_cap = Some(1);
    let mut sim = Simulator::new(IngestShard, config);
    seed_file(&mut sim, &path, payload)?;

    let ingest = sim.register(Ingest {
        path,
        chunk,
        cap,
        helper: None,
        file: None,
        result: None,
        chunks: 0,
        issues: Vec::new(),
    });
    let waiter = sim
        .observe_result::<Result<IngestRunReport, FileFailure>, _, _>(ingest)
        .map_err(|error| RunError::Observe {
            actor: "file ingest",
            error,
        })?;
    map_start::<_, FileFailure>("file ingest", sim.try_send(ingest, IngestMsg::Start))?;
    sim.run_until_quiescent();
    if sim.has_in_flight_calls() {
        return Err(RunError::InFlightCalls);
    }
    wait_actor("file ingest", waiter, Duration::ZERO)
}

#[derive(Debug)]
enum CopyMsg {
    Start,
    SrcOpened(Result<FileId, CallError>),
    DstOpened(Result<FileId, CallError>),
    Read(FileReadReply),
    Wrote(FileWriteOwnedReply),
    DstReadBack(FileReadReply),
    SrcClosed(Result<(), CallError>),
    DstClosed(Result<(), CallError>),
}

struct CopyPump {
    src_path: PathBuf,
    dst_path: PathBuf,
    chunk: usize,
    cap: u64,
    src: Option<FileId>,
    dst: Option<FileId>,
    pump: Option<FileCopyBounded>,
    readback: Option<FileReadChunks>,
    report: Option<FileLoopReport>,
    dst_contents: Vec<u8>,
    close_completions: usize,
    issues: Vec<FileIssue>,
}

impl CopyPump {
    fn issue(&mut self, stage: FileStage, error: FileIssueKind) {
        self.issues.push(FileIssue { stage, error });
    }

    fn drive(&mut self) -> Effect<Self> {
        let next = {
            let pump = self.pump.as_mut().expect("copy pump armed");
            pump.next_effect(CopyMsg::Read, CopyMsg::Wrote)
                .map(Ok)
                .unwrap_or_else(|| Err(pump.report(FileLoopEnd::CapReached)))
        };
        match next {
            Ok(effect) => effect,
            Err(report) => self.finish_copy(report),
        }
    }

    fn finish_copy(&mut self, report: FileLoopReport) -> Effect<Self> {
        if matches!(report.end, FileLoopEnd::Error | FileLoopEnd::StuckWrite) {
            self.issue(
                FileStage::Copy,
                FileIssueKind::Loop {
                    end: report.end,
                    error: report.error,
                },
            );
        }
        self.report = Some(report);
        self.pump = None;
        let max_total = self
            .report
            .as_ref()
            .expect("copy report stored")
            .bytes_transferred;
        if max_total == 0 {
            return self.cleanup();
        }
        let helper = FileReadChunks::new(
            self.dst.expect("destination open"),
            0,
            self.chunk,
            max_total,
        );
        let effect = helper
            .next_effect(CopyMsg::DstReadBack)
            .expect("non-zero readback cap has a read effect");
        self.readback = Some(helper);
        effect
    }

    fn cleanup(&mut self) -> Effect<Self> {
        self.pump = None;
        self.readback = None;
        if let Some(src) = self.src.take() {
            return file_close(src).then(CopyMsg::SrcClosed);
        }
        if let Some(dst) = self.dst.take() {
            return file_close(dst).then(CopyMsg::DstClosed);
        }
        self.publish()
    }

    fn publish(&mut self) -> Effect<Self> {
        if self.issues.is_empty() {
            stop_with(Ok::<_, FileFailure>(CopyRun {
                dst_contents: std::mem::take(&mut self.dst_contents),
                report: self.report.take().expect("successful copy has report"),
                close_completions: self.close_completions,
            }))
        } else {
            stop_with(Err::<CopyRun, _>(FileFailure {
                issues: std::mem::take(&mut self.issues),
            }))
        }
    }
}

#[tina_runtime::isolate(message = CopyMsg, shard = IngestShard)]
impl CopyPump {
    fn handle(
        &mut self,
        msg: CopyMsg,
        _ctx: &mut Context<'_, IngestShard, Self::Reply>,
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
            CopyMsg::SrcOpened(Err(error)) => {
                self.issue(FileStage::SourceOpen, FileIssueKind::Call(error));
                self.publish()
            }
            CopyMsg::DstOpened(Ok(dst)) => {
                self.dst = Some(dst);
                self.pump = Some(FileCopyBounded::new(
                    self.src.expect("source open"),
                    dst,
                    0,
                    0,
                    self.chunk,
                    self.cap,
                ));
                self.drive()
            }
            CopyMsg::DstOpened(Err(error)) => {
                self.issue(FileStage::DestinationOpen, FileIssueKind::Call(error));
                self.cleanup()
            }
            CopyMsg::Read(reply) => {
                let pump = self.pump.as_mut().expect("copy pump armed");
                match pump.advance(FileCopyProgress::Read(reply), CopyMsg::Read, CopyMsg::Wrote) {
                    FileCopyStep::Pending(effect) => effect,
                    FileCopyStep::Done(report) => self.finish_copy(report),
                }
            }
            CopyMsg::Wrote(reply) => {
                let pump = self.pump.as_mut().expect("copy pump armed");
                match pump.advance(
                    FileCopyProgress::Write(reply),
                    CopyMsg::Read,
                    CopyMsg::Wrote,
                ) {
                    FileCopyStep::Pending(effect) => effect,
                    FileCopyStep::Done(report) => self.finish_copy(report),
                }
            }
            CopyMsg::DstReadBack(reply) => {
                let helper = self.readback.as_mut().expect("readback helper armed");
                match helper.advance::<Self, _, _>(reply, CopyMsg::DstReadBack) {
                    FileLoopStep::Pending(effect) => effect,
                    FileLoopStep::Done(bytes, report) => {
                        if matches!(report.end, FileLoopEnd::Error | FileLoopEnd::StuckWrite) {
                            self.issue(
                                FileStage::DestinationReadBack,
                                FileIssueKind::Loop {
                                    end: report.end,
                                    error: report.error,
                                },
                            );
                        } else if bytes.len() as u64
                            != self
                                .report
                                .as_ref()
                                .expect("copy report stored")
                                .bytes_transferred
                        {
                            self.issue(
                                FileStage::DestinationReadBack,
                                FileIssueKind::Call(CallError::InvariantViolation),
                            );
                        }
                        self.dst_contents = bytes;
                        self.cleanup()
                    }
                    FileLoopStep::Ended(report) => {
                        self.issue(
                            FileStage::DestinationReadBack,
                            FileIssueKind::Loop {
                                end: report.end,
                                error: report.error,
                            },
                        );
                        self.cleanup()
                    }
                }
            }
            CopyMsg::SrcClosed(result) => {
                self.close_completions += 1;
                if let Err(error) = result {
                    self.issue(FileStage::SourceClose, FileIssueKind::Call(error));
                }
                self.cleanup()
            }
            CopyMsg::DstClosed(result) => {
                self.close_completions += 1;
                if let Err(error) = result {
                    self.issue(FileStage::DestinationClose, FileIssueKind::Call(error));
                }
                self.cleanup()
            }
        }
    }
}

/// Result of a bounded copy run.
#[derive(Debug, Clone)]
pub struct CopyRun {
    /// Destination contents after the copy.
    pub dst_contents: Vec<u8>,
    /// Terminal copy report.
    pub report: FileLoopReport,
    /// Exact number of file-close continuations settled before publication.
    pub close_completions: usize,
}

/// Seed a source file and bounded-copy it to a destination file.
pub fn run_copy(payload: Vec<u8>, chunk: usize, cap: u64) -> Result<CopyRun, FileRunError> {
    if chunk == 0 {
        return Err(RunError::InvalidConfig("chunk must be greater than zero"));
    }
    let src_path = PathBuf::from("/tmp/specimen_file_copy_src.dat");
    let dst_path = PathBuf::from("/tmp/specimen_file_copy_dst.dat");
    let mut config = SimulatorConfig::default();
    config.storage.file_write_cap = Some(1);
    let mut sim = Simulator::new(IngestShard, config);
    seed_file(&mut sim, &src_path, payload)?;

    let pump = sim.register(CopyPump {
        src_path,
        dst_path,
        chunk,
        cap,
        src: None,
        dst: None,
        pump: None,
        readback: None,
        report: None,
        dst_contents: Vec::new(),
        close_completions: 0,
        issues: Vec::new(),
    });
    let waiter = sim
        .observe_result::<Result<CopyRun, FileFailure>, _, _>(pump)
        .map_err(|error| RunError::Observe {
            actor: "file copy",
            error,
        })?;
    map_start::<_, FileFailure>("file copy", sim.try_send(pump, CopyMsg::Start))?;
    sim.run_until_quiescent();
    if sim.has_in_flight_calls() {
        return Err(RunError::InFlightCalls);
    }
    wait_actor("file copy", waiter, Duration::ZERO)
}

/// Convenience wrapper for the ingest smoke command.
pub fn smoke() -> Result<SpecimenReport, FileRunError> {
    let result = run_ingest(
        PathBuf::from("/tmp/specimen_file_ingest.dat"),
        b"the quick brown fox jumps over the lazy dog".to_vec(),
        8,
        64,
    )?;
    Ok(SpecimenReport {
        name: "file_ingest",
        bytes: result.helper.bytes_transferred,
        frames: result.chunks,
        ok: matches!(result.helper.end, FileLoopEnd::Eof),
        note: format!(
            "end={:?} chunks={} high_water_chunk={} final_offset={}",
            result.helper.end,
            result.chunks,
            result.helper.high_water_chunk,
            result.helper.final_offset,
        ),
    })
}

/// Bad-input proof: a short cap remains distinct from EOF.
pub fn bad_input_cap_reached() -> Result<SpecimenReport, FileRunError> {
    let result = run_ingest(
        PathBuf::from("/tmp/specimen_file_ingest_cap.dat"),
        b"AAAAAAAAAAAAAAAAAAAAAAAA".to_vec(),
        4,
        8,
    )?;
    Ok(SpecimenReport {
        name: "file_ingest:cap_reached",
        bytes: result.helper.bytes_transferred,
        frames: result.chunks,
        ok: matches!(result.helper.end, FileLoopEnd::CapReached) && result.bytes.len() == 8,
        note: format!("end={:?} bytes={}", result.helper.end, result.bytes.len()),
    })
}

/// Smoke for the bounded two-file copy pump.
pub fn copy_smoke() -> Result<SpecimenReport, FileRunError> {
    let payload = b"copy me through a bounded two-FD pump".to_vec();
    let result = run_copy(payload.clone(), 8, 1024)?;
    Ok(SpecimenReport {
        name: "file_copy",
        bytes: result.report.bytes_transferred,
        frames: 1,
        ok: matches!(result.report.end, FileLoopEnd::Eof)
            && result.dst_contents == payload
            && result.close_completions == 2,
        note: format!(
            "end={:?} transferred={} dst_len={} closes={}",
            result.report.end,
            result.report.bytes_transferred,
            result.dst_contents.len(),
            result.close_completions,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_cap_ingest_finishes_and_closes_without_reading() {
        let report = run_ingest(PathBuf::from("zero-cap"), b"payload".to_vec(), 4, 0)
            .expect("zero cap is a valid bounded run");
        assert_eq!(report.helper.end, FileLoopEnd::CapReached);
        assert!(report.bytes.is_empty());
        assert_eq!(report.chunks, 0);
    }

    #[test]
    fn empty_file_reaches_eof_without_a_data_chunk() {
        let report =
            run_ingest(PathBuf::from("empty"), Vec::new(), 4, 16).expect("empty file ingest");
        assert_eq!(report.helper.end, FileLoopEnd::Eof);
        assert!(report.bytes.is_empty());
        assert_eq!(report.chunks, 0);
    }

    #[test]
    fn zero_chunk_is_a_fallible_config_error() {
        assert_eq!(
            run_ingest(PathBuf::from("unused"), Vec::new(), 0, 16).unwrap_err(),
            RunError::InvalidConfig("chunk must be greater than zero")
        );
        assert_eq!(
            run_copy(Vec::new(), 0, 16).unwrap_err(),
            RunError::InvalidConfig("chunk must be greater than zero")
        );
    }

    #[test]
    fn copy_waits_for_both_close_completions() {
        let payload = b"partial writes are deliberate".to_vec();
        let result = run_copy(payload.clone(), 3, 1024).expect("bounded copy");
        assert_eq!(result.dst_contents, payload);
        assert_eq!(result.report.end, FileLoopEnd::Eof);
        assert_eq!(result.close_completions, 2);
    }

    #[test]
    fn zero_cap_copy_closes_both_files_without_copying() {
        let result = run_copy(b"not copied".to_vec(), 4, 0).expect("zero-cap copy");
        assert!(result.dst_contents.is_empty());
        assert_eq!(result.report.end, FileLoopEnd::CapReached);
        assert_eq!(result.close_completions, 2);
    }

    #[test]
    fn copy_readback_is_bounded_but_not_truncated_at_an_arbitrary_page() {
        let payload = vec![b'x'; 5000];
        let result = run_copy(payload.clone(), 257, 6000).expect("large bounded copy");
        assert_eq!(result.dst_contents, payload);
        assert_eq!(result.report.bytes_transferred, 5000);
        assert_eq!(result.close_completions, 2);
    }
}
