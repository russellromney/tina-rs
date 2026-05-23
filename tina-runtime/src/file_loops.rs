//! Client-side helpers for boring positional file loops.
//!
//! These mirror [`crate::tcp_loops`] for files. They are tiny state
//! machines that live inside an isolate's state struct. Each helper
//! expands to one underlying `file_read_at` / `file_write_at` per
//! step, so per-chunk progress remains trace-visible. There is no
//! hidden read-whole-file path: caller supplies max chunk and max
//! total, and partial progress reports name what completed before a
//! cancel/error.
//!
//! Files are offset-shaped. The helpers explicitly carry an `offset`
//! every step. They are not a pretend-TCP-stream wrapper.

use tina::Isolate;

use crate::call::{
    CallError, FileId, FileReadReply, FileWriteReply, RuntimeCall, file_read_at, file_write_at,
};

/// Why a file-loop helper stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileLoopEnd {
    /// All requested bytes were transferred.
    Done,
    /// EOF before reaching the cap.
    Eof,
    /// Reached the configured total cap without hitting EOF.
    CapReached,
    /// Underlying file call returned an error.
    Error,
    /// Stuck write: kernel accepted zero bytes with pending payload.
    StuckWrite,
}

/// Terminal report common to every file-loop helper.
///
/// Names what finished, what did not, and the final offset.
#[derive(Debug, Clone)]
pub struct FileLoopReport {
    /// Why the loop ended.
    pub end: FileLoopEnd,
    /// Bytes transferred so far.
    pub bytes_transferred: u64,
    /// Final file offset reached.
    pub final_offset: u64,
    /// Optional underlying call error, when `end == Error`.
    pub error: Option<CallError>,
    /// High-water-mark seen for the per-chunk buffer (bytes).
    pub high_water_chunk: usize,
    /// Configured total cap.
    pub cap: u64,
}

/// Outcome of a file-loop `advance`.
pub enum FileLoopStep<I: Isolate, T> {
    /// Helper issued the next underlying call. Return the effect.
    Pending(tina::Effect<I>),
    /// Loop finished. Use the payload (e.g. accumulated bytes) and the report.
    Done(T, FileLoopReport),
    /// Loop ended without producing a payload (writes, copies). Report only.
    Ended(FileLoopReport),
}

impl<I: Isolate, T: std::fmt::Debug> std::fmt::Debug for FileLoopStep<I, T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending(_) => formatter.write_str("Pending(<effect>)"),
            Self::Done(payload, report) => formatter
                .debug_tuple("Done")
                .field(payload)
                .field(report)
                .finish(),
            Self::Ended(report) => formatter.debug_tuple("Ended").field(report).finish(),
        }
    }
}

/// Read up to `max_total` bytes from `file` starting at `start_offset`,
/// one `file_read_at` per step. Bounded buffer growth: caller picks the
/// per-chunk budget and the total cap.
///
/// Reports back via [`FileLoopReport`] whether the helper ended by EOF,
/// reaching the cap, or an error. Cancellation outside the helper (the
/// caller decides to stop calling `next_effect` / `advance`) still leaves
/// the helper inspectable: [`Self::report`] returns a partial-progress
/// report carrying bytes read so far and the next offset.
#[derive(Debug)]
pub struct FileReadChunks {
    file: FileId,
    offset: u64,
    chunk: usize,
    max_total: u64,
    buffer: Vec<u8>,
    high_water_chunk: usize,
}

impl FileReadChunks {
    /// Build a positional-read helper.
    ///
    /// # Panics
    ///
    /// Panics if `chunk == 0`. A zero per-call budget would issue
    /// `file_read_at(file, 0, ...)`, which is a known sharp edge
    /// (zero-byte reads look like EOF). Pick a positive chunk.
    pub fn new(file: FileId, start_offset: u64, chunk: usize, max_total: u64) -> Self {
        assert!(
            chunk > 0,
            "FileReadChunks requires chunk > 0; zero would issue file_read_at(file, 0, ...)"
        );
        Self {
            file,
            offset: start_offset,
            chunk,
            max_total,
            buffer: Vec::new(),
            high_water_chunk: 0,
        }
    }

    /// Returns the effect that issues the next `file_read_at`. Returns
    /// `None` once the cap is reached.
    pub fn next_effect<I, M, F>(&self, on_progress: F) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(FileReadReply) -> M + Send + 'static,
        M: 'static,
    {
        if self.buffer.len() as u64 >= self.max_total {
            None
        } else {
            let remaining = self.max_total - self.buffer.len() as u64;
            let budget = remaining.min(self.chunk as u64) as usize;
            Some(file_read_at(self.file, budget, self.offset).then(on_progress))
        }
    }

    /// Record bytes from a `file_read_at` reply. Empty bytes (EOF)
    /// finishes the loop with whatever was accumulated.
    pub fn advance<I, M, F>(
        &mut self,
        reply: FileReadReply,
        on_progress: F,
    ) -> FileLoopStep<I, Vec<u8>>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(FileReadReply) -> M + Send + 'static,
        M: 'static,
    {
        match reply {
            Ok(bytes) if bytes.is_empty() => {
                let report = self.terminal_report(FileLoopEnd::Eof, None);
                let payload = std::mem::take(&mut self.buffer);
                FileLoopStep::Done(payload, report)
            }
            Ok(bytes) => {
                self.high_water_chunk = self.high_water_chunk.max(bytes.len());
                let room = self.max_total - self.buffer.len() as u64;
                let take = (bytes.len() as u64).min(room) as usize;
                self.buffer.extend_from_slice(&bytes[..take]);
                self.offset += take as u64;
                if self.buffer.len() as u64 >= self.max_total {
                    let report = self.terminal_report(FileLoopEnd::CapReached, None);
                    let payload = std::mem::take(&mut self.buffer);
                    FileLoopStep::Done(payload, report)
                } else {
                    let remaining = self.max_total - self.buffer.len() as u64;
                    let budget = remaining.min(self.chunk as u64) as usize;
                    FileLoopStep::Pending(
                        file_read_at(self.file, budget, self.offset).then(on_progress),
                    )
                }
            }
            Err(error) => {
                let report = self.terminal_report(FileLoopEnd::Error, Some(error));
                let payload = std::mem::take(&mut self.buffer);
                FileLoopStep::Done(payload, report)
            }
        }
    }

    /// Bytes accumulated so far.
    pub fn so_far(&self) -> usize {
        self.buffer.len()
    }

    /// Current offset (the offset the next read will be issued at).
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Build a partial-progress report without finishing the loop.
    /// Useful when the caller decides to stop on cancellation/shutdown
    /// and needs to record what completed.
    pub fn report(&self, end: FileLoopEnd) -> FileLoopReport {
        FileLoopReport {
            end,
            bytes_transferred: self.buffer.len() as u64,
            final_offset: self.offset,
            error: None,
            high_water_chunk: self.high_water_chunk,
            cap: self.max_total,
        }
    }

    fn terminal_report(&self, end: FileLoopEnd, error: Option<CallError>) -> FileLoopReport {
        FileLoopReport {
            end,
            bytes_transferred: self.buffer.len() as u64,
            final_offset: self.offset,
            error,
            high_water_chunk: self.high_water_chunk,
            cap: self.max_total,
        }
    }
}

/// Write `bytes` to `file` starting at `start_offset`, one
/// `file_write_at` per step. Resolves via a [`FileLoopReport`].
///
/// Partial writes are honored: the helper advances the offset by the
/// kernel-accepted count and issues another write. A zero-progress
/// write with non-empty pending data is surfaced as `StuckWrite`
/// instead of looping forever.
#[derive(Debug)]
pub struct FileWriteAll {
    file: FileId,
    offset: u64,
    pending: Vec<u8>,
    written: u64,
    high_water_chunk: usize,
    /// Total bytes the helper was asked to write, captured at
    /// construction. Stays stable as `pending` drains so the report's
    /// `cap` does not collapse to `written` on completion.
    total: u64,
}

impl FileWriteAll {
    /// Build a positional-write helper.
    pub fn new(file: FileId, start_offset: u64, bytes: Vec<u8>) -> Self {
        let high_water_chunk = bytes.len();
        let total = bytes.len() as u64;
        Self {
            file,
            offset: start_offset,
            pending: bytes,
            written: 0,
            high_water_chunk,
            total,
        }
    }

    /// Effect that issues the next `file_write_at`. Returns `None` if
    /// nothing is left to send.
    pub fn next_effect<I, M, F>(&self, on_progress: F) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(FileWriteReply) -> M + Send + 'static,
        M: 'static,
    {
        if self.pending.is_empty() {
            None
        } else {
            Some(file_write_at(self.file, self.pending.clone(), self.offset).then(on_progress))
        }
    }

    /// Record bytes from a `file_write_at` reply. Zero progress on a
    /// non-empty buffer is treated as a stuck write.
    pub fn advance<I, M, F>(&mut self, reply: FileWriteReply, on_progress: F) -> FileLoopStep<I, ()>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(FileWriteReply) -> M + Send + 'static,
        M: 'static,
    {
        match reply {
            Ok(0) if !self.pending.is_empty() => {
                let report = self.terminal_report(FileLoopEnd::StuckWrite, Some(CallError::Io));
                FileLoopStep::Ended(report)
            }
            Ok(count) => {
                let drained = count.min(self.pending.len());
                self.pending.drain(..drained);
                self.written += drained as u64;
                self.offset += drained as u64;
                if self.pending.is_empty() {
                    let report = self.terminal_report(FileLoopEnd::Done, None);
                    FileLoopStep::Ended(report)
                } else {
                    FileLoopStep::Pending(
                        file_write_at(self.file, self.pending.clone(), self.offset)
                            .then(on_progress),
                    )
                }
            }
            Err(error) => {
                let report = self.terminal_report(FileLoopEnd::Error, Some(error));
                FileLoopStep::Ended(report)
            }
        }
    }

    /// Bytes written so far.
    pub const fn written(&self) -> u64 {
        self.written
    }

    /// Bytes that still need to ship.
    pub fn remaining(&self) -> usize {
        self.pending.len()
    }

    /// Current offset (start_offset + written).
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Build a partial-progress report without finishing the loop.
    pub fn report(&self, end: FileLoopEnd) -> FileLoopReport {
        self.report_with(end, None)
    }

    fn terminal_report(&self, end: FileLoopEnd, error: Option<CallError>) -> FileLoopReport {
        self.report_with(end, error)
    }

    fn report_with(&self, end: FileLoopEnd, error: Option<CallError>) -> FileLoopReport {
        FileLoopReport {
            end,
            bytes_transferred: self.written,
            final_offset: self.offset,
            error,
            high_water_chunk: self.high_water_chunk,
            // For writes, `cap` is the total bytes requested at
            // construction — it does not collapse to `written` as the
            // buffer drains.
            cap: self.total,
        }
    }
}

/// Bounded read-from-`src`-and-write-to-`dst` pump. State machine that
/// reads up to `max_total` bytes via [`FileReadChunks`] and writes
/// each accumulated chunk via [`FileWriteAll`].
///
/// This is a two-phase machine: each turn either reads or writes; the
/// caller's continuation message must carry which leg fired. The
/// helper exposes [`Self::next_effect_read`] / [`Self::next_effect_write`]
/// so the caller can drive the right leg.
#[derive(Debug)]
pub struct FileCopyBounded {
    src: FileId,
    dst: FileId,
    src_offset: u64,
    dst_offset: u64,
    chunk: usize,
    max_total: u64,
    transferred: u64,
    high_water_chunk: usize,
    in_flight: Option<Vec<u8>>,
    pending_write_offset: u64,
}

/// Which leg of the copy pump should be driven next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyLeg {
    /// Read from the source.
    Read,
    /// Write the buffered chunk to the destination.
    Write,
    /// Nothing left to do; consult [`FileCopyBounded::report`].
    Done,
}

/// Reply routed back into [`FileCopyBounded::advance`].
#[derive(Debug)]
pub enum FileCopyProgress {
    /// Result of the last source `file_read_at`.
    Read(FileReadReply),
    /// Result of the last destination `file_write_at`.
    Write(FileWriteReply),
}

/// Unified copy-pump step.
pub enum FileCopyStep<I: Isolate> {
    /// Helper issued the next read or write effect. Return it.
    Pending(tina::Effect<I>),
    /// Copy stopped with a terminal report.
    Done(FileLoopReport),
}

impl<I: Isolate> std::fmt::Debug for FileCopyStep<I> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending(_) => formatter.write_str("Pending(<effect>)"),
            Self::Done(report) => formatter.debug_tuple("Done").field(report).finish(),
        }
    }
}

impl FileCopyBounded {
    /// Build a bounded copy pump.
    ///
    /// # Panics
    ///
    /// Panics if `chunk == 0`. A zero per-call budget would issue
    /// `file_read_at(src, 0, ...)`, which is a known sharp edge.
    pub fn new(
        src: FileId,
        dst: FileId,
        src_offset: u64,
        dst_offset: u64,
        chunk: usize,
        max_total: u64,
    ) -> Self {
        assert!(
            chunk > 0,
            "FileCopyBounded requires chunk > 0; zero would issue file_read_at(src, 0, ...)"
        );
        Self {
            src,
            dst,
            src_offset,
            dst_offset,
            chunk,
            max_total,
            transferred: 0,
            high_water_chunk: 0,
            in_flight: None,
            pending_write_offset: dst_offset,
        }
    }

    /// Which leg of the pump is ready to fire next.
    pub fn next_leg(&self) -> CopyLeg {
        if self.in_flight.is_some() {
            CopyLeg::Write
        } else if self.transferred >= self.max_total {
            CopyLeg::Done
        } else {
            CopyLeg::Read
        }
    }

    /// Build the next `file_read_at` effect when [`Self::next_leg`] is
    /// [`CopyLeg::Read`]. Returns `None` otherwise.
    pub fn next_effect_read<I, M, F>(&self, on_progress: F) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(FileReadReply) -> M + Send + 'static,
        M: 'static,
    {
        if self.next_leg() != CopyLeg::Read {
            return None;
        }
        let remaining = self.max_total - self.transferred;
        let budget = remaining.min(self.chunk as u64) as usize;
        Some(file_read_at(self.src, budget, self.src_offset).then(on_progress))
    }

    /// Build the next `file_write_at` effect when [`Self::next_leg`]
    /// is [`CopyLeg::Write`]. Returns `None` otherwise.
    pub fn next_effect_write<I, M, F>(&self, on_progress: F) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(FileWriteReply) -> M + Send + 'static,
        M: 'static,
    {
        let buffer = self.in_flight.as_ref()?;
        Some(file_write_at(self.dst, buffer.clone(), self.pending_write_offset).then(on_progress))
    }

    /// Build the next effect for the current leg.
    ///
    /// This is the copy path for new callers. The older
    /// [`next_leg`](Self::next_leg) / [`next_effect_read`](Self::next_effect_read)
    /// / [`next_effect_write`](Self::next_effect_write) trio remains available
    /// when a service wants to branch manually.
    pub fn next_effect<I, M, FR, FW>(&self, on_read: FR, on_write: FW) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        FR: FnOnce(FileReadReply) -> M + Send + 'static,
        FW: FnOnce(FileWriteReply) -> M + Send + 'static,
        M: 'static,
    {
        match self.next_leg() {
            CopyLeg::Read => self.next_effect_read(on_read),
            CopyLeg::Write => self.next_effect_write(on_write),
            CopyLeg::Done => None,
        }
    }

    /// Record one read or write reply and return the next effect or terminal
    /// report.
    pub fn advance<I, M, FR, FW>(
        &mut self,
        progress: FileCopyProgress,
        on_read: FR,
        on_write: FW,
    ) -> FileCopyStep<I>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        FR: FnOnce(FileReadReply) -> M + Send + 'static,
        FW: FnOnce(FileWriteReply) -> M + Send + 'static,
        M: 'static,
    {
        let leg = match progress {
            FileCopyProgress::Read(reply) => self.record_read(reply),
            FileCopyProgress::Write(reply) => self.record_write(reply),
        };
        match leg {
            Ok(CopyLeg::Read) => self
                .next_effect_read(on_read)
                .map(FileCopyStep::Pending)
                .unwrap_or_else(|| {
                    FileCopyStep::Done(self.terminal_report(FileLoopEnd::Done, None))
                }),
            Ok(CopyLeg::Write) => self
                .next_effect_write(on_write)
                .map(FileCopyStep::Pending)
                .unwrap_or_else(|| {
                    FileCopyStep::Done(self.terminal_report(FileLoopEnd::Done, None))
                }),
            Ok(CopyLeg::Done) => FileCopyStep::Done(self.terminal_report(FileLoopEnd::Done, None)),
            Err(report) => FileCopyStep::Done(report),
        }
    }

    /// Record a `file_read_at` reply.
    ///
    /// Must be called only when [`Self::next_leg`] is [`CopyLeg::Read`].
    /// Calling it while a write chunk is in flight would discard
    /// unwritten bytes, so that is a programmer error and panics —
    /// the same sharp-edge discipline as the zero-chunk constructor.
    pub fn record_read(&mut self, reply: FileReadReply) -> Result<CopyLeg, FileLoopReport> {
        assert!(
            self.in_flight.is_none(),
            "FileCopyBounded::record_read called while a write chunk is in flight; \
             dispatch via next_leg() (expected CopyLeg::Write)"
        );
        match reply {
            Ok(bytes) if bytes.is_empty() => Err(self.terminal_report(FileLoopEnd::Eof, None)),
            Ok(bytes) => {
                self.high_water_chunk = self.high_water_chunk.max(bytes.len());
                let room = self.max_total - self.transferred;
                let take = (bytes.len() as u64).min(room) as usize;
                let chunk_bytes = bytes[..take].to_vec();
                self.src_offset += take as u64;
                self.pending_write_offset = self.dst_offset + self.transferred;
                self.in_flight = Some(chunk_bytes);
                Ok(CopyLeg::Write)
            }
            Err(error) => Err(self.terminal_report(FileLoopEnd::Error, Some(error))),
        }
    }

    /// Record a `file_write_at` reply.
    ///
    /// Must be called only when [`Self::next_leg`] is [`CopyLeg::Write`]
    /// (i.e. a read chunk is buffered). Calling it on the read leg is a
    /// programmer error and panics.
    pub fn record_write(&mut self, reply: FileWriteReply) -> Result<CopyLeg, FileLoopReport> {
        assert!(
            self.in_flight.is_some(),
            "FileCopyBounded::record_write called with no chunk in flight; \
             dispatch via next_leg() (expected CopyLeg::Read)"
        );
        match reply {
            Ok(0) => Err(self.terminal_report(FileLoopEnd::StuckWrite, Some(CallError::Io))),
            Ok(count) => {
                let buffer = self
                    .in_flight
                    .as_mut()
                    .expect("in_flight checked by the assert above");
                let drained = count.min(buffer.len());
                buffer.drain(..drained);
                self.transferred += drained as u64;
                self.pending_write_offset += drained as u64;
                if buffer.is_empty() {
                    self.in_flight = None;
                    if self.transferred >= self.max_total {
                        Err(self.terminal_report(FileLoopEnd::CapReached, None))
                    } else {
                        Ok(CopyLeg::Read)
                    }
                } else {
                    Ok(CopyLeg::Write)
                }
            }
            Err(error) => Err(self.terminal_report(FileLoopEnd::Error, Some(error))),
        }
    }

    /// Bytes transferred so far.
    pub const fn transferred(&self) -> u64 {
        self.transferred
    }

    /// Build a partial-progress report without finishing the loop.
    pub fn report(&self, end: FileLoopEnd) -> FileLoopReport {
        FileLoopReport {
            end,
            bytes_transferred: self.transferred,
            final_offset: self.dst_offset + self.transferred,
            error: None,
            high_water_chunk: self.high_water_chunk,
            cap: self.max_total,
        }
    }

    fn terminal_report(&self, end: FileLoopEnd, error: Option<CallError>) -> FileLoopReport {
        FileLoopReport {
            end,
            bytes_transferred: self.transferred,
            final_offset: self.dst_offset + self.transferred,
            error,
            high_water_chunk: self.high_water_chunk,
            cap: self.max_total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::Effect;

    #[derive(Debug)]
    struct DummyIsolate;

    impl Isolate for DummyIsolate {
        type Message = Msg;
        type Reply = ();
        type Send = ();
        type Spawn = std::convert::Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<Msg>;
        type Fact = ::std::convert::Infallible;
        type Shard = tina::SingleShard;

        fn handle(
            &mut self,
            _: Msg,
            _: &mut tina::Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            tina::noop()
        }
    }

    #[allow(dead_code)]
    #[derive(Debug)]
    enum Msg {
        Read(FileReadReply),
        Wrote(FileWriteReply),
    }

    fn file(id: u64) -> FileId {
        FileId::new(id)
    }

    // -- FileReadChunks -----------------------------------------------------

    #[test]
    fn read_chunks_finishes_on_eof() {
        let mut helper = FileReadChunks::new(file(1), 0, 4, 1024);
        let step: FileLoopStep<DummyIsolate, Vec<u8>> =
            helper.advance(Ok(b"abc".to_vec()), Msg::Read);
        assert!(matches!(step, FileLoopStep::Pending(_)));
        let step: FileLoopStep<DummyIsolate, Vec<u8>> = helper.advance(Ok(Vec::new()), Msg::Read);
        match step {
            FileLoopStep::Done(buffer, report) => {
                assert_eq!(buffer, b"abc");
                assert_eq!(report.end, FileLoopEnd::Eof);
                assert_eq!(report.bytes_transferred, 3);
                assert_eq!(report.final_offset, 3);
                assert_eq!(report.high_water_chunk, 3);
            }
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn read_chunks_stops_at_cap() {
        let mut helper = FileReadChunks::new(file(1), 0, 4, 5);
        let step: FileLoopStep<DummyIsolate, Vec<u8>> =
            helper.advance(Ok(b"abcd".to_vec()), Msg::Read);
        assert!(matches!(step, FileLoopStep::Pending(_)));
        let step: FileLoopStep<DummyIsolate, Vec<u8>> =
            helper.advance(Ok(b"efgh".to_vec()), Msg::Read);
        match step {
            FileLoopStep::Done(buffer, report) => {
                assert_eq!(buffer, b"abcde");
                assert_eq!(report.end, FileLoopEnd::CapReached);
                assert_eq!(report.bytes_transferred, 5);
                assert_eq!(report.final_offset, 5);
                assert_eq!(report.cap, 5);
            }
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn read_chunks_reports_partial_on_cancel() {
        let mut helper = FileReadChunks::new(file(1), 100, 4, 1024);
        let _step: FileLoopStep<DummyIsolate, Vec<u8>> =
            helper.advance(Ok(b"ab".to_vec()), Msg::Read);
        // Caller decides to stop without finishing the loop.
        let partial = helper.report(FileLoopEnd::CapReached);
        assert_eq!(partial.bytes_transferred, 2);
        assert_eq!(partial.final_offset, 102);
    }

    #[test]
    fn read_chunks_propagates_error() {
        let mut helper = FileReadChunks::new(file(1), 0, 4, 1024);
        let step: FileLoopStep<DummyIsolate, Vec<u8>> =
            helper.advance(Err(CallError::Io), Msg::Read);
        match step {
            FileLoopStep::Done(_buffer, report) => {
                assert_eq!(report.end, FileLoopEnd::Error);
                assert_eq!(report.error, Some(CallError::Io));
            }
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    #[should_panic(expected = "chunk > 0")]
    fn read_chunks_rejects_zero_chunk() {
        let _ = FileReadChunks::new(file(1), 0, 0, 1024);
    }

    #[test]
    fn read_chunks_empty_file_done_immediately() {
        let mut helper = FileReadChunks::new(file(1), 0, 8, 1024);
        let step: FileLoopStep<DummyIsolate, Vec<u8>> = helper.advance(Ok(Vec::new()), Msg::Read);
        match step {
            FileLoopStep::Done(buffer, report) => {
                assert!(buffer.is_empty());
                assert_eq!(report.end, FileLoopEnd::Eof);
                assert_eq!(report.bytes_transferred, 0);
            }
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn read_chunks_exact_chunk_boundary() {
        let mut helper = FileReadChunks::new(file(1), 0, 4, 12);
        let _ = helper.advance::<DummyIsolate, _, _>(Ok(b"AAAA".to_vec()), Msg::Read);
        let _ = helper.advance::<DummyIsolate, _, _>(Ok(b"BBBB".to_vec()), Msg::Read);
        let step: FileLoopStep<DummyIsolate, Vec<u8>> =
            helper.advance(Ok(b"CCCC".to_vec()), Msg::Read);
        match step {
            FileLoopStep::Done(buffer, report) => {
                assert_eq!(buffer, b"AAAABBBBCCCC");
                assert_eq!(report.end, FileLoopEnd::CapReached);
                assert_eq!(report.final_offset, 12);
            }
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn read_chunks_kernel_overflow_truncates_to_cap() {
        let mut helper = FileReadChunks::new(file(1), 0, 32, 5);
        let step: FileLoopStep<DummyIsolate, Vec<u8>> =
            helper.advance(Ok(b"hello world".to_vec()), Msg::Read);
        match step {
            FileLoopStep::Done(buffer, report) => {
                assert_eq!(buffer, b"hello");
                assert_eq!(report.end, FileLoopEnd::CapReached);
                assert_eq!(report.final_offset, 5);
            }
            other => panic!("unexpected step: {other:?}"),
        }
    }

    // -- FileWriteAll ------------------------------------------------------

    #[test]
    fn write_all_finishes_in_one_step() {
        let mut helper = FileWriteAll::new(file(1), 0, b"hello".to_vec());
        let step: FileLoopStep<DummyIsolate, ()> = helper.advance(Ok(5), Msg::Wrote);
        match step {
            FileLoopStep::Ended(report) => {
                assert_eq!(report.end, FileLoopEnd::Done);
                assert_eq!(report.bytes_transferred, 5);
                assert_eq!(report.final_offset, 5);
                // `cap` reflects the total requested at construction and
                // does not collapse to `written` when the buffer drains.
                assert_eq!(report.cap, 5);
            }
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn write_all_report_cap_is_stable_across_partial_writes() {
        let mut helper = FileWriteAll::new(file(1), 0, b"hello world".to_vec());
        // Mid-flight partial-progress report.
        let _ = helper.advance::<DummyIsolate, _, _>(Ok(4), Msg::Wrote);
        let partial = helper.report(FileLoopEnd::Done);
        assert_eq!(partial.cap, 11, "cap stays at total requested");
        assert_eq!(partial.bytes_transferred, 4);
        // Terminal report keeps the same cap.
        let step: FileLoopStep<DummyIsolate, ()> = helper.advance(Ok(7), Msg::Wrote);
        match step {
            FileLoopStep::Ended(report) => {
                assert_eq!(report.cap, 11);
                assert_eq!(report.bytes_transferred, 11);
            }
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn write_all_loops_on_partial_writes() {
        let mut helper = FileWriteAll::new(file(1), 100, b"hello".to_vec());
        let step: FileLoopStep<DummyIsolate, ()> = helper.advance(Ok(2), Msg::Wrote);
        assert!(matches!(step, FileLoopStep::Pending(_)));
        assert_eq!(helper.written(), 2);
        assert_eq!(helper.offset(), 102);

        let step: FileLoopStep<DummyIsolate, ()> = helper.advance(Ok(3), Msg::Wrote);
        match step {
            FileLoopStep::Ended(report) => {
                assert_eq!(report.end, FileLoopEnd::Done);
                assert_eq!(report.bytes_transferred, 5);
                assert_eq!(report.final_offset, 105);
            }
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn write_all_stuck_write_is_typed() {
        let mut helper = FileWriteAll::new(file(1), 0, b"hello".to_vec());
        let step: FileLoopStep<DummyIsolate, ()> = helper.advance(Ok(0), Msg::Wrote);
        match step {
            FileLoopStep::Ended(report) => {
                assert_eq!(report.end, FileLoopEnd::StuckWrite);
                assert_eq!(report.error, Some(CallError::Io));
                assert_eq!(report.bytes_transferred, 0);
            }
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn write_all_propagates_error() {
        let mut helper = FileWriteAll::new(file(1), 0, b"hello".to_vec());
        let step: FileLoopStep<DummyIsolate, ()> = helper.advance(Err(CallError::Io), Msg::Wrote);
        match step {
            FileLoopStep::Ended(report) => {
                assert_eq!(report.end, FileLoopEnd::Error);
                assert_eq!(report.error, Some(CallError::Io));
            }
            other => panic!("unexpected step: {other:?}"),
        }
    }

    // -- FileCopyBounded ----------------------------------------------------

    #[test]
    fn copy_bounded_pumps_read_then_write() {
        let mut pump = FileCopyBounded::new(file(1), file(2), 0, 0, 4, 1024);
        assert_eq!(pump.next_leg(), CopyLeg::Read);
        let leg = pump.record_read(Ok(b"abcd".to_vec())).expect("read ok");
        assert_eq!(leg, CopyLeg::Write);
        let leg = pump.record_write(Ok(4)).expect("write ok");
        assert_eq!(leg, CopyLeg::Read);
        match pump.record_read(Ok(Vec::new())) {
            Err(report) => {
                assert_eq!(report.end, FileLoopEnd::Eof);
                assert_eq!(report.bytes_transferred, 4);
                assert_eq!(report.final_offset, 4);
            }
            Ok(_) => panic!("expected EOF report"),
        }
    }

    #[test]
    fn copy_bounded_stops_at_cap() {
        let mut pump = FileCopyBounded::new(file(1), file(2), 0, 0, 4, 4);
        let _ = pump.record_read(Ok(b"abcd".to_vec())).expect("read ok");
        match pump.record_write(Ok(4)) {
            Err(report) => {
                assert_eq!(report.end, FileLoopEnd::CapReached);
                assert_eq!(report.bytes_transferred, 4);
            }
            Ok(_) => panic!("expected cap report"),
        }
    }

    #[test]
    fn copy_bounded_stuck_write_is_typed() {
        let mut pump = FileCopyBounded::new(file(1), file(2), 0, 0, 4, 1024);
        let _ = pump.record_read(Ok(b"abcd".to_vec())).expect("read ok");
        match pump.record_write(Ok(0)) {
            Err(report) => {
                assert_eq!(report.end, FileLoopEnd::StuckWrite);
                assert_eq!(report.error, Some(CallError::Io));
            }
            Ok(_) => panic!("expected stuck-write report"),
        }
    }

    #[test]
    #[should_panic(expected = "chunk > 0")]
    fn copy_bounded_rejects_zero_chunk() {
        let _ = FileCopyBounded::new(file(1), file(2), 0, 0, 0, 1024);
    }

    #[test]
    #[should_panic(expected = "record_write called with no chunk in flight")]
    fn copy_bounded_record_write_on_read_leg_panics() {
        let mut pump = FileCopyBounded::new(file(1), file(2), 0, 0, 4, 1024);
        assert_eq!(pump.next_leg(), CopyLeg::Read);
        // Misuse: feeding a write reply while the pump expects a read.
        let _ = pump.record_write(Ok(4));
    }

    #[test]
    #[should_panic(expected = "record_read called while a write chunk is in flight")]
    fn copy_bounded_record_read_on_write_leg_panics() {
        let mut pump = FileCopyBounded::new(file(1), file(2), 0, 0, 4, 1024);
        let _ = pump.record_read(Ok(b"abcd".to_vec())).expect("read ok");
        assert_eq!(pump.next_leg(), CopyLeg::Write);
        // Misuse: feeding a read reply while a chunk awaits writing.
        let _ = pump.record_read(Ok(b"efgh".to_vec()));
    }
}
