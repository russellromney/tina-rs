//! Client-side helpers for boring Unix-domain stream loops.
//!
//! These mirror [`crate::tcp_loops`] for Unix-domain sockets. Each helper is a
//! tiny state machine that lives in isolate state and expands to one
//! `unix_read` / `unix_write` per step. Partial progress stays trace-visible;
//! there is no hidden read-whole-stream path.

use tina::Isolate;

use crate::call::{
    CallError, RuntimeCall, UnixReadReply, UnixStreamId, UnixWriteOwnedReply, unix_read,
    unix_write_owned_from,
};
use crate::tcp_loops::LoopStep;

/// Write `bytes` to a Unix stream across as many `unix_write` calls as the
/// runtime needs. Resolves with the total bytes written.
#[derive(Debug)]
pub struct UnixWriteAll {
    stream: UnixStreamId,
    buffer: Option<Vec<u8>>,
    allocation: usize,
    in_flight: bool,
    written: usize,
    total: usize,
}

impl UnixWriteAll {
    /// Builds a write-all helper.
    pub fn new(stream: UnixStreamId, bytes: Vec<u8>) -> Self {
        let total = bytes.len();
        let allocation = bytes.as_ptr() as usize;
        Self {
            stream,
            buffer: Some(bytes),
            allocation,
            in_flight: false,
            written: 0,
            total,
        }
    }

    /// Returns the effect that issues the next `unix_write`. Returns `None` if
    /// the loop is complete or already has a write in flight.
    pub fn next_effect<I, M, F>(&mut self, on_progress: F) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(UnixWriteOwnedReply) -> M + Send + 'static,
        M: 'static,
    {
        if self.written >= self.total || self.in_flight {
            return None;
        }
        let bytes = self.buffer.take()?;
        if bytes.as_ptr() as usize != self.allocation {
            self.buffer = Some(bytes);
            return None;
        }
        self.in_flight = true;
        Some(unix_write_owned_from(self.stream, bytes, self.written).then(on_progress))
    }

    /// Issues the next write and delivers its full owned-buffer reply as a
    /// split service event.
    ///
    /// This is the service-envelope sibling of [`Self::next_effect`]. It
    /// hides only [`tina::ServiceMessage::Event`]; buffer ownership and the
    /// raw [`UnixWriteOwnedReply`] remain unchanged.
    /// Returns `None` when the loop is complete or already armed.
    pub fn next_service_event<I, Event, Request, F>(
        &mut self,
        on_progress: F,
    ) -> Option<tina::Effect<I>>
    where
        I: Isolate<
                Message = tina::ServiceMessage<Event, Request>,
                Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
            >,
        F: FnOnce(UnixWriteOwnedReply) -> Event + Send + 'static,
        Event: 'static,
        Request: 'static,
    {
        self.next_effect(move |reply| tina::ServiceMessage::Event(on_progress(reply)))
    }

    /// Records progress from a `unix_write` reply.
    ///
    /// `Ok(0)` with non-empty pending data is a stuck stream. Surface it as
    /// `Failed(CallError::Io)` instead of re-issuing the same write forever.
    pub fn advance<I, M, F>(
        &mut self,
        reply: UnixWriteOwnedReply,
        on_progress: F,
    ) -> LoopStep<I, usize>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(UnixWriteOwnedReply) -> M + Send + 'static,
        M: 'static,
    {
        let returned_allocation = match &reply {
            Ok(reply) => reply.bytes.as_ptr() as usize,
            Err(error) => error.bytes.as_ptr() as usize,
        };
        if !self.in_flight || returned_allocation != self.allocation {
            return LoopStep::Failed(CallError::InvariantViolation);
        }
        self.in_flight = false;

        match reply {
            Ok(reply) => {
                if reply.bytes.len() != self.total {
                    self.buffer = Some(reply.bytes);
                    return LoopStep::Failed(CallError::InvariantViolation);
                }
                let remaining = reply.bytes.len().saturating_sub(self.written);
                if reply.written > remaining {
                    self.buffer = Some(reply.bytes);
                    return LoopStep::Failed(CallError::InvariantViolation);
                }
                if reply.written == 0 && remaining > 0 {
                    self.buffer = Some(reply.bytes);
                    return LoopStep::Failed(CallError::Io);
                }
                self.written += reply.written;
                if self.written == reply.bytes.len() {
                    self.buffer = Some(reply.bytes);
                    LoopStep::Done(self.written)
                } else {
                    self.in_flight = true;
                    LoopStep::Pending(
                        unix_write_owned_from(self.stream, reply.bytes, self.written)
                            .then(on_progress),
                    )
                }
            }
            Err(error) => {
                self.buffer = Some(error.bytes);
                LoopStep::Failed(error.error)
            }
        }
    }

    /// Records progress and routes any next partial write as a split service
    /// event.
    ///
    /// Validation, buffer recovery, and [`LoopStep`] terminal outcomes are
    /// identical to [`Self::advance`]. Only the continuation envelope is
    /// supplied here.
    pub fn advance_service_event<I, Event, Request, F>(
        &mut self,
        reply: UnixWriteOwnedReply,
        on_progress: F,
    ) -> LoopStep<I, usize>
    where
        I: Isolate<
                Message = tina::ServiceMessage<Event, Request>,
                Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
            >,
        F: FnOnce(UnixWriteOwnedReply) -> Event + Send + 'static,
        Event: 'static,
        Request: 'static,
    {
        self.advance(reply, move |reply| {
            tina::ServiceMessage::Event(on_progress(reply))
        })
    }

    /// Bytes successfully written so far.
    pub const fn written(&self) -> usize {
        self.written
    }

    /// Bytes still pending.
    pub fn remaining(&self) -> usize {
        self.total.saturating_sub(self.written)
    }

    /// Returns whether one owned write currently holds the buffer.
    pub const fn is_in_flight(&self) -> bool {
        self.in_flight
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
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
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
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
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
        type Io = RuntimeCall<Msg>;
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
        Wrote(UnixWriteOwnedReply),
    }

    #[derive(Debug)]
    struct DummyEventService;

    impl Isolate for DummyEventService {
        type Message = tina::ServiceMessage<ServiceEvent, std::convert::Infallible>;
        type Reply = ();
        type Send = ();
        type Spawn = std::convert::Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Io = RuntimeCall<Self::Message>;
        type Fact = std::convert::Infallible;
        type Shard = tina::SingleShard;

        fn handle(
            &mut self,
            _: Self::Message,
            _: &mut tina::Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            tina::noop()
        }
    }

    #[allow(dead_code)]
    #[derive(Debug)]
    enum ServiceEvent {
        Wrote(UnixWriteOwnedReply),
    }

    fn stream(id: u64) -> UnixStreamId {
        UnixStreamId::new(id)
    }

    fn arm_write(helper: &mut UnixWriteAll, written: usize) -> UnixWriteOwnedReply {
        let bytes = helper.buffer.take().expect("buffer stored");
        helper.in_flight = true;
        Ok(crate::call::WriteOwnedReply { bytes, written })
    }

    #[test]
    fn write_all_handles_partial_writes() {
        let mut helper = UnixWriteAll::new(stream(1), b"abcdef".to_vec());
        let reply = arm_write(&mut helper, 2);
        let step: LoopStep<DummyIsolate, usize> = helper.advance(reply, Msg::Wrote);
        assert!(matches!(step, LoopStep::Pending(_)));
        assert_eq!(helper.written(), 2);
        assert_eq!(helper.remaining(), 4);
        assert!(helper.is_in_flight());
    }

    #[test]
    fn write_all_keeps_the_owned_allocation() {
        let mut helper = UnixWriteAll::new(stream(1), b"abcdef".to_vec());
        let bytes = helper.buffer.take().expect("buffer stored");
        let allocation = bytes.as_ptr();
        helper.in_flight = true;
        let step: LoopStep<DummyIsolate, usize> = helper.advance(
            Ok(crate::call::WriteOwnedReply { bytes, written: 6 }),
            Msg::Wrote,
        );
        assert!(matches!(step, LoopStep::Done(6)));
        assert_eq!(
            helper.buffer.as_ref().expect("buffer returned").as_ptr(),
            allocation
        );
    }

    #[test]
    fn write_all_service_event_shape_keeps_owned_allocation() {
        let mut shape = UnixWriteAll::new(stream(1), b"abcdef".to_vec());
        assert!(
            shape
                .next_service_event::<DummyEventService, _, _, _>(ServiceEvent::Wrote)
                .is_some()
        );

        let mut helper = UnixWriteAll::new(stream(1), b"abcdef".to_vec());
        let bytes = helper.buffer.take().expect("buffer stored");
        let allocation = bytes.as_ptr();
        helper.in_flight = true;
        let step: LoopStep<DummyEventService, usize> = helper.advance_service_event(
            Ok(crate::call::WriteOwnedReply { bytes, written: 6 }),
            ServiceEvent::Wrote,
        );
        assert!(matches!(step, LoopStep::Done(6)));
        assert_eq!(
            helper.buffer.as_ref().expect("buffer returned").as_ptr(),
            allocation
        );
    }

    #[test]
    fn write_all_rejects_zero_progress() {
        let mut helper = UnixWriteAll::new(stream(1), b"abc".to_vec());
        let reply = arm_write(&mut helper, 0);
        let step: LoopStep<DummyIsolate, usize> = helper.advance(reply, Msg::Wrote);
        assert!(matches!(step, LoopStep::Failed(CallError::Io)));
    }

    #[test]
    fn write_all_rejects_impossible_completion_count() {
        let mut helper = UnixWriteAll::new(stream(1), b"abc".to_vec());
        let reply = arm_write(&mut helper, 4);
        let step: LoopStep<DummyIsolate, usize> = helper.advance(reply, Msg::Wrote);
        assert!(matches!(
            step,
            LoopStep::Failed(CallError::InvariantViolation)
        ));
    }

    #[test]
    fn write_all_rejects_a_changed_owned_buffer_length() {
        let mut helper = UnixWriteAll::new(stream(1), b"abcdef".to_vec());
        let _ = helper.buffer.take();
        let bytes = b"abc".to_vec();
        helper.allocation = bytes.as_ptr() as usize;
        helper.in_flight = true;
        let step: LoopStep<DummyIsolate, usize> = helper.advance(
            Ok(crate::call::WriteOwnedReply { bytes, written: 3 }),
            Msg::Wrote,
        );
        assert!(matches!(
            step,
            LoopStep::Failed(CallError::InvariantViolation)
        ));
        assert_eq!(helper.remaining(), 6);
    }

    #[test]
    fn write_all_rejects_unarmed_and_stale_owned_replies() {
        let mut unarmed = UnixWriteAll::new(stream(1), b"abcdef".to_vec());
        let step: LoopStep<DummyIsolate, usize> = unarmed.advance(
            Ok(crate::call::WriteOwnedReply {
                bytes: b"abcdef".to_vec(),
                written: 6,
            }),
            Msg::Wrote,
        );
        assert!(matches!(
            step,
            LoopStep::Failed(CallError::InvariantViolation)
        ));

        let mut pending = UnixWriteAll::new(stream(1), b"abcdef".to_vec());
        let live_effect = pending
            .next_effect::<DummyIsolate, _, _>(Msg::Wrote)
            .expect("write arms");
        assert!(pending.is_in_flight());
        assert!(
            pending
                .next_effect::<DummyIsolate, _, _>(Msg::Wrote)
                .is_none()
        );
        let step: LoopStep<DummyIsolate, usize> = pending.advance(
            Ok(crate::call::WriteOwnedReply {
                bytes: b"abcdef".to_vec(),
                written: 1,
            }),
            Msg::Wrote,
        );
        assert!(matches!(
            step,
            LoopStep::Failed(CallError::InvariantViolation)
        ));
        assert!(
            pending.is_in_flight(),
            "stale reply cannot disarm live work"
        );
        drop(live_effect);
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
