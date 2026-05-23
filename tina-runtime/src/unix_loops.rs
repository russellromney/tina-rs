//! Client-side helpers for boring Unix-domain stream loops.
//!
//! These mirror [`crate::tcp_loops`] for Unix-domain sockets. Each helper is a
//! tiny state machine that lives in isolate state and expands to one
//! `unix_read` / `unix_write` per step. Partial progress stays trace-visible;
//! there is no hidden read-whole-stream path.

use tina::Isolate;

use crate::call::{
    CallError, RuntimeCall, UnixReadReply, UnixStreamId, UnixWriteReply, unix_read, unix_write,
};
use crate::tcp_loops::LoopStep;

/// Write `bytes` to a Unix stream across as many `unix_write` calls as the
/// runtime needs. Resolves with the total bytes written.
#[derive(Debug)]
pub struct UnixWriteAll {
    stream: UnixStreamId,
    pending: Vec<u8>,
    written: usize,
}

impl UnixWriteAll {
    /// Builds a write-all helper.
    pub fn new(stream: UnixStreamId, bytes: Vec<u8>) -> Self {
        Self {
            stream,
            pending: bytes,
            written: 0,
        }
    }

    /// Returns the effect that issues the next `unix_write`. Returns `None` if
    /// the loop has nothing left to send.
    pub fn next_effect<I, M, F>(&self, on_progress: F) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(UnixWriteReply) -> M + Send + 'static,
        M: 'static,
    {
        if self.pending.is_empty() {
            None
        } else {
            Some(unix_write(self.stream, self.pending.clone()).then(on_progress))
        }
    }

    /// Records progress from a `unix_write` reply.
    ///
    /// `Ok(0)` with non-empty pending data is a stuck stream. Surface it as
    /// `Failed(CallError::Io)` instead of re-issuing the same write forever.
    pub fn advance<I, M, F>(&mut self, reply: UnixWriteReply, on_progress: F) -> LoopStep<I, usize>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(UnixWriteReply) -> M + Send + 'static,
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
                        unix_write(self.stream, self.pending.clone()).then(on_progress),
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

    /// Bytes still pending.
    pub fn remaining(&self) -> usize {
        self.pending.len()
    }
}

/// Read from a Unix stream until EOF or until `max` bytes have accumulated.
#[derive(Debug)]
pub struct UnixReadToEof {
    stream: UnixStreamId,
    max: usize,
    chunk: usize,
    buffer: Vec<u8>,
}

impl UnixReadToEof {
    /// Builds a read-to-EOF helper.
    ///
    /// # Panics
    ///
    /// Panics if `chunk == 0`. A zero-byte read can look like EOF; pick a
    /// positive per-call budget.
    pub fn new(stream: UnixStreamId, max: usize, chunk: usize) -> Self {
        assert!(
            chunk > 0,
            "UnixReadToEof requires chunk > 0; zero would issue unix_read(stream, 0)"
        );
        Self {
            stream,
            max,
            chunk,
            buffer: Vec::new(),
        }
    }

    /// Returns the effect that issues the next `unix_read`. Returns `None` if
    /// already at the cap.
    pub fn next_effect<I, M, F>(&self, on_progress: F) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(UnixReadReply) -> M + Send + 'static,
        M: 'static,
    {
        if self.buffer.len() >= self.max {
            None
        } else {
            let budget = (self.max - self.buffer.len()).min(self.chunk);
            Some(unix_read(self.stream, budget).then(on_progress))
        }
    }

    /// Records bytes from a `unix_read` reply. Empty bytes (EOF) finishes the
    /// loop with whatever was accumulated.
    pub fn advance<I, M, F>(&mut self, reply: UnixReadReply, on_progress: F) -> LoopStep<I, Vec<u8>>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(UnixReadReply) -> M + Send + 'static,
        M: 'static,
    {
        match reply {
            Ok(bytes) if bytes.is_empty() => LoopStep::Done(std::mem::take(&mut self.buffer)),
            Ok(bytes) => {
                let remaining = self.max - self.buffer.len();
                let take = bytes.len().min(remaining);
                self.buffer.extend_from_slice(&bytes[..take]);
                if self.buffer.len() >= self.max {
                    LoopStep::Done(std::mem::take(&mut self.buffer))
                } else {
                    let budget = (self.max - self.buffer.len()).min(self.chunk);
                    LoopStep::Pending(unix_read(self.stream, budget).then(on_progress))
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

    #[derive(Debug)]
    struct DummyIsolate;

    impl Isolate for DummyIsolate {
        type Message = Msg;
        type Reply = ();
        type Send = ();
        type Spawn = std::convert::Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<Msg>;
        type Fact = std::convert::Infallible;
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
        Read(UnixReadReply),
        Wrote(UnixWriteReply),
    }

    fn stream(id: u64) -> UnixStreamId {
        UnixStreamId::new(id)
    }

    #[test]
    fn write_all_handles_partial_writes() {
        let mut helper = UnixWriteAll::new(stream(1), b"abcdef".to_vec());
        let step: LoopStep<DummyIsolate, usize> = helper.advance(Ok(2), Msg::Wrote);
        assert!(matches!(step, LoopStep::Pending(_)));
        assert_eq!(helper.written(), 2);
        assert_eq!(helper.remaining(), 4);
        let step: LoopStep<DummyIsolate, usize> = helper.advance(Ok(4), Msg::Wrote);
        assert!(matches!(step, LoopStep::Done(6)));
    }

    #[test]
    fn write_all_rejects_zero_progress() {
        let mut helper = UnixWriteAll::new(stream(1), b"abc".to_vec());
        let step: LoopStep<DummyIsolate, usize> = helper.advance(Ok(0), Msg::Wrote);
        assert!(matches!(step, LoopStep::Failed(CallError::Io)));
    }

    #[test]
    fn read_to_eof_finishes_on_empty_read() {
        let mut helper = UnixReadToEof::new(stream(1), 8, 4);
        let step: LoopStep<DummyIsolate, Vec<u8>> = helper.advance(Ok(b"abc".to_vec()), Msg::Read);
        assert!(matches!(step, LoopStep::Pending(_)));
        let step: LoopStep<DummyIsolate, Vec<u8>> = helper.advance(Ok(Vec::new()), Msg::Read);
        match step {
            LoopStep::Done(bytes) => assert_eq!(bytes, b"abc"),
            other => panic!("unexpected step: {other:?}"),
        }
    }

    #[test]
    fn read_to_eof_stops_at_cap() {
        let mut helper = UnixReadToEof::new(stream(1), 5, 4);
        let step: LoopStep<DummyIsolate, Vec<u8>> = helper.advance(Ok(b"abcd".to_vec()), Msg::Read);
        assert!(matches!(step, LoopStep::Pending(_)));
        let step: LoopStep<DummyIsolate, Vec<u8>> = helper.advance(Ok(b"efgh".to_vec()), Msg::Read);
        match step {
            LoopStep::Done(bytes) => assert_eq!(bytes, b"abcde"),
            other => panic!("unexpected step: {other:?}"),
        }
    }
}
