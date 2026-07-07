//! Client-side helpers for boring TCP loops.
//!
//! Each helper is a tiny state machine that lives inside an isolate's
//! state struct. The handler dispatches the next runtime call by
//! calling [`TcpWriteAll::next_effect`] / [`TcpReadExact::next_effect`]
//! / [`TcpReadToEof::next_effect`]; on each `tcp_write` or `tcp_read`
//! reply, the handler calls `advance` to update progress and either
//! gets a new effect to dispatch or a typed `Outcome`.
//!
//! Each helper expands to one underlying `tcp_write` / `tcp_read` per
//! step, so partial progress remains trace-visible. No hidden buffer
//! growth: byte caps are caller-supplied. Close/cancel behavior is
//! exactly what the underlying lane already does — the helper does
//! not own the stream.

use tina::Isolate;

use crate::call::{
    CallError, RuntimeCall, StreamId, TcpReadReply, TcpWriteReply, tcp_read, tcp_write,
};

/// Outcome produced by `advance` after a partial-progress reply.
///
/// `Pending` means the helper issued the next sub-call; the isolate
/// returns the effect from its handler. `Done` means the loop is
/// complete; the helper hands back the accumulated value. `Failed`
/// surfaces the underlying [`CallError`] without retrying — retry is
/// the caller's choice.
pub enum LoopStep<I: Isolate, T> {
    /// Helper issued the next underlying call. Return the effect.
    Pending(tina::Effect<I>),
    /// Loop finished. Use the value.
    Done(T),
    /// Underlying call failed. Caller decides whether to retry.
    Failed(CallError),
}

impl<I: Isolate, T: std::fmt::Debug> std::fmt::Debug for LoopStep<I, T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending(_) => formatter.write_str("Pending(<effect>)"),
            Self::Done(value) => formatter.debug_tuple("Done").field(value).finish(),
            Self::Failed(error) => formatter.debug_tuple("Failed").field(error).finish(),
        }
    }
}

/// `TcpReadExact::advance` outcome. Splits the generic `LoopStep` so
/// "peer closed before target len" can carry the partial buffer
/// instead of being lossy.
pub enum ReadExactStep<I: Isolate> {
    /// Helper issued the next `tcp_read`.
    Pending(tina::Effect<I>),
    /// Buffer filled to exactly the target length.
    Done(Vec<u8>),
    /// Peer signalled EOF before the target length. The bytes the
    /// helper did read are returned so the caller can decide whether
    /// the partial result is useful.
    EarlyEof(Vec<u8>),
    /// Underlying `tcp_read` returned an error.
    Failed(CallError),
}

impl<I: Isolate> std::fmt::Debug for ReadExactStep<I> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending(_) => formatter.write_str("Pending(<effect>)"),
            Self::Done(buffer) => formatter.debug_tuple("Done").field(&buffer.len()).finish(),
            Self::EarlyEof(buffer) => formatter
                .debug_tuple("EarlyEof")
                .field(&buffer.len())
                .finish(),
            Self::Failed(error) => formatter.debug_tuple("Failed").field(error).finish(),
        }
    }
}

/// Write `bytes` to `stream` across as many `tcp_write` calls as the
/// kernel needs. Resolves with the total bytes written.
#[derive(Debug)]
pub struct TcpWriteAll {
    stream: StreamId,
    pending: Vec<u8>,
    written: usize,
}

impl TcpWriteAll {
    /// Builds a write-all helper. Caller chooses the byte slice.
    pub fn new(stream: StreamId, bytes: Vec<u8>) -> Self {
        Self {
            stream,
            pending: bytes,
            written: 0,
        }
    }

    /// Returns the effect that issues the next `tcp_write`. Returns
    /// `None` if the loop has nothing left to send.
    pub fn next_effect<I, M, F>(&self, on_progress: F) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(TcpWriteReply) -> M + Send + 'static,
        M: 'static,
    {
        if self.pending.is_empty() {
            None
        } else {
            Some(tcp_write(self.stream, self.pending.clone()).then(on_progress))
        }
    }

    /// Records progress from a `tcp_write` reply. Returns the next
    /// step: another `Pending` effect, `Done(total_written)`, or
    /// `Failed`.
    ///
    /// `Ok(0)` with non-empty `pending` is a stuck stream: zero
    /// bytes accepted means the next call would loop forever on the
    /// same buffer. Surface as `Failed(CallError::Io)` rather than
    /// re-issuing the write.
    pub fn advance<I, M, F>(&mut self, reply: TcpWriteReply, on_progress: F) -> LoopStep<I, usize>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(TcpWriteReply) -> M + Send + 'static,
        M: 'static,
    {
        match reply {
            Ok(0) if !self.pending.is_empty() => LoopStep::Failed(CallError::Io),
            Ok(count) => {
                let drained = count.min(self.pending.len());
                self.pending.drain(..drained);
                self.written += drained;
                if self.pending.is_empty() {
                    LoopStep::Done(self.written)
                } else {
                    LoopStep::Pending(
                        tcp_write(self.stream, self.pending.clone()).then(on_progress),
                    )
                }
            }
            Err(error) => LoopStep::Failed(error),
        }
    }

    /// Bytes successfully written so far.
    pub const fn written(&self) -> usize {
        self.written
    }

    /// Bytes that still need to ship.
    pub fn remaining(&self) -> usize {
        self.pending.len()
    }
}

/// Read exactly `len` bytes from `stream` across as many `tcp_read`
/// calls as needed. Treats early EOF as `CallError::Closed` because a
/// caller asking for `len` bytes wants exactly that.
#[derive(Debug)]
pub struct TcpReadExact {
    stream: StreamId,
    target_len: usize,
    buffer: Vec<u8>,
}

impl TcpReadExact {
    /// Build a read-exact helper. `len` is the exact byte count
    /// expected.
    pub fn new(stream: StreamId, len: usize) -> Self {
        Self {
            stream,
            target_len: len,
            buffer: Vec::with_capacity(len),
        }
    }

    /// Effect that issues the next `tcp_read`. Returns `None` once
    /// the buffer is full.
    pub fn next_effect<I, M, F>(&self, on_progress: F) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(TcpReadReply) -> M + Send + 'static,
        M: 'static,
    {
        if self.buffer.len() >= self.target_len {
            None
        } else {
            Some(tcp_read(self.stream, self.target_len - self.buffer.len()).then(on_progress))
        }
    }

    /// Records bytes from a `tcp_read` reply. Empty bytes (EOF)
    /// before the buffer fills up are surfaced as `EarlyEof` with
    /// the partial buffer attached so the caller can decide what to
    /// do with it.
    pub fn advance<I, M, F>(&mut self, reply: TcpReadReply, on_progress: F) -> ReadExactStep<I>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(TcpReadReply) -> M + Send + 'static,
        M: 'static,
    {
        match reply {
            Ok(bytes) if bytes.is_empty() => {
                ReadExactStep::EarlyEof(std::mem::take(&mut self.buffer))
            }
            Ok(bytes) => {
                let need = self.target_len - self.buffer.len();
                let take = bytes.len().min(need);
                self.buffer.extend_from_slice(&bytes[..take]);
                if self.buffer.len() >= self.target_len {
                    ReadExactStep::Done(std::mem::take(&mut self.buffer))
                } else {
                    ReadExactStep::Pending(
                        tcp_read(self.stream, self.target_len - self.buffer.len())
                            .then(on_progress),
                    )
                }
            }
            Err(error) => ReadExactStep::Failed(error),
        }
    }

    /// Bytes accumulated so far.
    pub fn so_far(&self) -> usize {
        self.buffer.len()
    }
}

/// Read from `stream` until the peer signals EOF (an empty-bytes
/// reply) or until accumulated bytes reach `max`. Bounded buffer
/// growth: caller picks `max`.
#[derive(Debug)]
pub struct TcpReadToEof {
    stream: StreamId,
    max: usize,
    chunk: usize,
    buffer: Vec<u8>,
}

impl TcpReadToEof {
    /// Build a read-to-EOF helper. `max` caps the accumulated
    /// buffer; `chunk` is the per-call read budget.
    ///
    /// # Panics
    ///
    /// Panics if `chunk == 0`. A zero per-call budget would have
    /// `next_effect` issue `tcp_read(stream, 0)`, which Tina
    /// already treats as a sharp edge (zero-byte reads can look
    /// like EOF). Pick a non-zero chunk that matches the expected
    /// message size.
    pub fn new(stream: StreamId, max: usize, chunk: usize) -> Self {
        assert!(
            chunk > 0,
            "TcpReadToEof requires chunk > 0; zero would issue tcp_read(stream, 0)"
        );
        Self {
            stream,
            max,
            chunk,
            buffer: Vec::new(),
        }
    }

    /// Effect that issues the next `tcp_read`. Returns `None` if
    /// already at the cap.
    pub fn next_effect<I, M, F>(&self, on_progress: F) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(TcpReadReply) -> M + Send + 'static,
        M: 'static,
    {
        if self.buffer.len() >= self.max {
            None
        } else {
            let budget = (self.max - self.buffer.len()).min(self.chunk);
            Some(tcp_read(self.stream, budget).then(on_progress))
        }
    }

    /// Records bytes from a `tcp_read` reply. Empty bytes (EOF)
    /// finishes the loop with whatever was accumulated.
    pub fn advance<I, M, F>(&mut self, reply: TcpReadReply, on_progress: F) -> LoopStep<I, Vec<u8>>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(TcpReadReply) -> M + Send + 'static,
        M: 'static,
    {
        match reply {
            Ok(bytes) if bytes.is_empty() => LoopStep::Done(std::mem::take(&mut self.buffer)),
            Ok(bytes) => {
                let room = self.max - self.buffer.len();
                let take = bytes.len().min(room);
                self.buffer.extend_from_slice(&bytes[..take]);
                if self.buffer.len() >= self.max {
                    LoopStep::Done(std::mem::take(&mut self.buffer))
                } else {
                    let budget = (self.max - self.buffer.len()).min(self.chunk);
                    LoopStep::Pending(tcp_read(self.stream, budget).then(on_progress))
                }
            }
            Err(error) => LoopStep::Failed(error),
        }
    }

    /// Bytes accumulated so far.
    pub fn so_far(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::Effect;

    // Pure-state tests: drive `advance` directly with synthesized
    // replies. The runtime is not involved; the helpers' state
    // machines are the unit under test.

    #[derive(Debug)]
    struct DummyIsolate;

    impl Isolate for DummyIsolate {
        type Message = Msg;
        type Reply = ();
        type Send = ();
        type Spawn = std::convert::Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Io = RuntimeCall<Msg>;
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

    /// The payload fields are never read directly inside these
    /// tests — the helpers' `next_effect` / `advance` calls only need
    /// the variants as continuation targets — but they have to carry
    /// the reply types so `Msg::Wrote` / `Msg::Read` typecheck as
    /// `FnOnce(TcpWriteReply) -> Msg` / `FnOnce(TcpReadReply) -> Msg`.
    #[allow(dead_code)]
    #[derive(Debug)]
    enum Msg {
        Wrote(TcpWriteReply),
        Read(TcpReadReply),
    }

    fn stream(id: u64) -> StreamId {
        // StreamId is an opaque newtype constructed by the runtime; for
        // these unit tests we only care that the helpers do not look at
        // its bits.
        StreamId::new(id)
    }

    #[test]
    fn write_all_completes_in_one_step_when_kernel_writes_everything() {
        let mut helper = TcpWriteAll::new(stream(1), b"hello".to_vec());
        let step: LoopStep<DummyIsolate, usize> = helper.advance(Ok(5), Msg::Wrote);
        assert!(matches!(step, LoopStep::Done(5)));
        assert_eq!(helper.written(), 5);
        assert_eq!(helper.remaining(), 0);
    }

    #[test]
    fn write_all_loops_on_partial_writes() {
        let mut helper = TcpWriteAll::new(stream(1), b"hello".to_vec());
        let step: LoopStep<DummyIsolate, usize> = helper.advance(Ok(2), Msg::Wrote);
        assert!(matches!(step, LoopStep::Pending(_)));
        assert_eq!(helper.written(), 2);
        assert_eq!(helper.remaining(), 3);

        let step: LoopStep<DummyIsolate, usize> = helper.advance(Ok(3), Msg::Wrote);
        assert!(matches!(step, LoopStep::Done(5)));
        assert_eq!(helper.remaining(), 0);
    }

    #[test]
    fn write_all_surfaces_call_error() {
        let mut helper = TcpWriteAll::new(stream(1), b"hello".to_vec());
        let step: LoopStep<DummyIsolate, usize> = helper.advance(Err(CallError::Io), Msg::Wrote);
        assert!(matches!(step, LoopStep::Failed(CallError::Io)));
    }

    #[test]
    fn read_exact_loops_until_target_filled() {
        let mut helper = TcpReadExact::new(stream(1), 5);
        let step: ReadExactStep<DummyIsolate> = helper.advance(Ok(b"he".to_vec()), Msg::Read);
        assert!(matches!(step, ReadExactStep::Pending(_)));
        assert_eq!(helper.so_far(), 2);

        let step: ReadExactStep<DummyIsolate> = helper.advance(Ok(b"llo".to_vec()), Msg::Read);
        match step {
            ReadExactStep::Done(buffer) => assert_eq!(buffer, b"hello"),
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn read_exact_returns_early_eof_with_partial_buffer() {
        let mut helper = TcpReadExact::new(stream(1), 5);
        let step: ReadExactStep<DummyIsolate> = helper.advance(Ok(b"hi".to_vec()), Msg::Read);
        assert!(matches!(step, ReadExactStep::Pending(_)));
        let step: ReadExactStep<DummyIsolate> = helper.advance(Ok(Vec::new()), Msg::Read);
        match step {
            ReadExactStep::EarlyEof(partial) => assert_eq!(partial, b"hi"),
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn read_to_eof_finishes_on_empty_bytes() {
        let mut helper = TcpReadToEof::new(stream(1), 1024, 64);
        let step: LoopStep<DummyIsolate, Vec<u8>> = helper.advance(Ok(b"abc".to_vec()), Msg::Read);
        assert!(matches!(step, LoopStep::Pending(_)));
        let step: LoopStep<DummyIsolate, Vec<u8>> = helper.advance(Ok(Vec::new()), Msg::Read);
        match step {
            LoopStep::Done(buffer) => assert_eq!(buffer, b"abc"),
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn write_all_treats_zero_progress_with_pending_data_as_io_failure() {
        let mut helper = TcpWriteAll::new(stream(1), b"hello".to_vec());
        let step: LoopStep<DummyIsolate, usize> = helper.advance(Ok(0), Msg::Wrote);
        assert!(
            matches!(step, LoopStep::Failed(CallError::Io)),
            "zero-progress write on non-empty pending must be a typed failure",
        );
    }

    #[test]
    #[should_panic(expected = "chunk > 0")]
    fn read_to_eof_rejects_zero_chunk() {
        // `next_effect` would issue tcp_read(stream, 0), a known
        // sharp edge. Reject at construction.
        let _ = TcpReadToEof::new(stream(1), 1024, 0);
    }

    #[test]
    fn read_to_eof_caps_at_max() {
        let mut helper = TcpReadToEof::new(stream(1), 4, 64);
        let step: LoopStep<DummyIsolate, Vec<u8>> =
            helper.advance(Ok(b"hello world".to_vec()), Msg::Read);
        match step {
            // Truncated to max bytes; helper finishes without
            // continuing to ask for more.
            LoopStep::Done(buffer) => assert_eq!(buffer, b"hell"),
            other => panic!("unexpected step: {other:?}"),
        }
    }
}
