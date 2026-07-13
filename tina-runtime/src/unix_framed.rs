//! Bounded encode-and-write companions for Unix-domain streams.

use tina::Isolate;
use tina_codec::{LengthPrefix, encode_into};

use crate::{CallError, LoopStep, RuntimeCall, UnixStreamId, UnixWriteAll, UnixWriteOwnedReply};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FrameFormat {
    Lines {
        max_line_len: usize,
    },
    LengthDelimited {
        prefix: LengthPrefix,
        max_body_len: usize,
    },
}

/// Typed refusal from building a bounded framed write batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramedWriteError {
    /// The body exceeds the configured or prefix-representable body cap.
    BodyFull {
        /// Supplied body length.
        body_len: usize,
        /// Effective maximum body length.
        max_body_len: usize,
    },
    /// A line body contains a newline delimiter and would decode as more than
    /// one frame.
    LineContainsNewline,
    /// A line body ends in carriage return, which [`tina_codec::LineFramer`]
    /// would strip before returning the frame.
    LineEndsWithCarriageReturn,
    /// The encoded frame would exceed the writer's total byte cap.
    BatchFull {
        /// Bytes already encoded into this batch.
        encoded_len: usize,
        /// Bytes the refused frame would add, including framing.
        frame_len: usize,
        /// Configured maximum encoded bytes.
        max_encoded_len: usize,
    },
    /// Frames cannot be appended after the first write effect is issued.
    AlreadyStarted,
}

/// Bounded frame encoder plus partial-progress Unix write state machine.
///
/// Frames are encoded into one explicitly capped batch. Calling
/// [`Self::next_effect`] with a non-empty batch freezes it and delegates every
/// partial write to [`UnixWriteAll`], so each underlying I/O turn remains
/// visible in runtime and simulator traces.
#[derive(Debug)]
pub struct UnixFramedWriter {
    stream: UnixStreamId,
    format: FrameFormat,
    max_encoded_len: usize,
    encoded: Option<Vec<u8>>,
    frame_count: usize,
    write_all: Option<UnixWriteAll>,
}

impl UnixFramedWriter {
    /// Builds a newline-delimited writer with body and whole-batch caps.
    ///
    /// # Panics
    ///
    /// Panics if `max_line_len == 0`, matching
    /// [`tina_codec::LineFramer::new`].
    pub fn lines(stream: UnixStreamId, max_line_len: usize, max_encoded_len: usize) -> Self {
        assert!(
            max_line_len > 0,
            "UnixFramedWriter requires max_line_len > 0"
        );
        Self::new(stream, FrameFormat::Lines { max_line_len }, max_encoded_len)
    }

    /// Builds a length-delimited writer with body and whole-batch caps.
    pub fn length_delimited(
        stream: UnixStreamId,
        prefix: LengthPrefix,
        max_body_len: usize,
        max_encoded_len: usize,
    ) -> Self {
        Self::new(
            stream,
            FrameFormat::LengthDelimited {
                prefix,
                max_body_len,
            },
            max_encoded_len,
        )
    }

    fn new(stream: UnixStreamId, format: FrameFormat, max_encoded_len: usize) -> Self {
        Self {
            stream,
            format,
            max_encoded_len,
            encoded: Some(Vec::new()),
            frame_count: 0,
            write_all: None,
        }
    }

    /// Encodes one body onto the batch without exceeding either configured
    /// cap. A refusal leaves the existing batch unchanged.
    pub fn push_frame(&mut self, body: impl AsRef<[u8]>) -> Result<(), FramedWriteError> {
        let body = body.as_ref();
        let encoded = self
            .encoded
            .as_mut()
            .ok_or(FramedWriteError::AlreadyStarted)?;
        let frame_len = match self.format {
            FrameFormat::Lines { max_line_len } => {
                if body.len() > max_line_len {
                    return Err(FramedWriteError::BodyFull {
                        body_len: body.len(),
                        max_body_len: max_line_len,
                    });
                }
                if body.contains(&b'\n') {
                    return Err(FramedWriteError::LineContainsNewline);
                }
                if body.last() == Some(&b'\r') {
                    return Err(FramedWriteError::LineEndsWithCarriageReturn);
                }
                body.len().saturating_add(1)
            }
            FrameFormat::LengthDelimited {
                prefix,
                max_body_len,
            } => {
                let prefix_max = match prefix {
                    LengthPrefix::U8 => u8::MAX as usize,
                    LengthPrefix::U16 => u16::MAX as usize,
                    LengthPrefix::U32 => u32::MAX as usize,
                };
                let effective_max = max_body_len.min(prefix_max);
                if body.len() > effective_max {
                    return Err(FramedWriteError::BodyFull {
                        body_len: body.len(),
                        max_body_len: effective_max,
                    });
                }
                prefix.width().saturating_add(body.len())
            }
        };
        let next_len = encoded.len().checked_add(frame_len);
        if next_len.is_none_or(|next_len| next_len > self.max_encoded_len) {
            return Err(FramedWriteError::BatchFull {
                encoded_len: encoded.len(),
                frame_len,
                max_encoded_len: self.max_encoded_len,
            });
        }

        match self.format {
            FrameFormat::Lines { .. } => {
                encoded.extend_from_slice(body);
                encoded.push(b'\n');
            }
            FrameFormat::LengthDelimited { prefix, .. } => {
                let appended = encode_into(prefix, body, encoded);
                debug_assert_eq!(appended, Some(frame_len));
            }
        }
        self.frame_count += 1;
        Ok(())
    }

    /// Issues the first or next partial Unix write. An empty frame batch has
    /// no write effect and remains open for more frames.
    pub fn next_effect<I, M, F>(&mut self, on_progress: F) -> Option<tina::Effect<I>>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(UnixWriteOwnedReply) -> M + Send + 'static,
        M: 'static,
    {
        if self.write_all.is_none() {
            let encoded = self.encoded.take()?;
            if encoded.is_empty() {
                self.encoded = Some(encoded);
                return None;
            }
            self.write_all = Some(UnixWriteAll::new(self.stream, encoded));
        }
        self.write_all.as_mut()?.next_effect(on_progress)
    }

    /// Issues the first or next partial write as a split-service event.
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
        if self.write_all.is_none() {
            let encoded = self.encoded.take()?;
            if encoded.is_empty() {
                self.encoded = Some(encoded);
                return None;
            }
            self.write_all = Some(UnixWriteAll::new(self.stream, encoded));
        }
        self.write_all.as_mut()?.next_service_event(on_progress)
    }

    /// Records a raw owned-write completion and returns the next partial
    /// write, total encoded bytes written, or exact runtime failure.
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
        let Some(write_all) = self.write_all.as_mut() else {
            return LoopStep::Failed(CallError::InvariantViolation);
        };
        write_all.advance(reply, on_progress)
    }

    /// Records an owned-write completion and routes any next partial write as
    /// a split-service event.
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
        let Some(write_all) = self.write_all.as_mut() else {
            return LoopStep::Failed(CallError::InvariantViolation);
        };
        write_all.advance_service_event(reply, on_progress)
    }

    /// Number of frames accepted into the batch.
    pub const fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Total encoded bytes in the batch.
    pub fn encoded_len(&self) -> usize {
        self.encoded.as_ref().map_or_else(
            || {
                self.write_all
                    .as_ref()
                    .map_or(0, |write_all| write_all.written() + write_all.remaining())
            },
            Vec::len,
        )
    }

    /// Whether a non-empty batch has issued its first write effect.
    pub const fn is_started(&self) -> bool {
        self.write_all.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::{Context, Effect, Outbound, SingleShard};

    #[derive(Debug)]
    struct Dummy;

    #[allow(dead_code)]
    #[derive(Debug)]
    enum Msg {
        Wrote(UnixWriteOwnedReply),
    }

    impl Isolate for Dummy {
        type Message = Msg;
        type Reply = ();
        type Send = Outbound<std::convert::Infallible>;
        type Spawn = std::convert::Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Io = RuntimeCall<Msg>;
        type Fact = std::convert::Infallible;
        type Shard = SingleShard;

        fn handle(
            &mut self,
            _msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            tina::noop()
        }
    }

    fn stream() -> UnixStreamId {
        UnixStreamId::new(1)
    }

    #[test]
    fn line_writer_encodes_bounded_frames() {
        let mut writer = UnixFramedWriter::lines(stream(), 8, 16);
        writer.push_frame(b"ping").unwrap();
        writer.push_frame(b"ok").unwrap();

        assert_eq!(writer.encoded.as_deref(), Some(&b"ping\nok\n"[..]));
        assert_eq!(writer.frame_count(), 2);
        assert_eq!(writer.encoded_len(), 8);
    }

    #[test]
    fn line_writer_rejects_non_roundtripping_bodies_without_mutation() {
        let mut writer = UnixFramedWriter::lines(stream(), 4, 8);
        writer.push_frame(b"ok").unwrap();
        let before = writer.encoded.clone();

        assert_eq!(
            writer.push_frame(b"abcde"),
            Err(FramedWriteError::BodyFull {
                body_len: 5,
                max_body_len: 4,
            })
        );
        assert_eq!(
            writer.push_frame(b"x\ny"),
            Err(FramedWriteError::LineContainsNewline)
        );
        assert_eq!(
            writer.push_frame(b"x\r"),
            Err(FramedWriteError::LineEndsWithCarriageReturn)
        );
        assert_eq!(writer.encoded, before);
    }

    #[test]
    fn length_writer_encodes_prefixes_and_enforces_both_caps() {
        let mut writer = UnixFramedWriter::length_delimited(stream(), LengthPrefix::U8, 4, 8);
        writer.push_frame(b"abc").unwrap();
        assert_eq!(writer.encoded.as_deref(), Some(&[3, b'a', b'b', b'c'][..]));
        assert_eq!(
            writer.push_frame(b"12345"),
            Err(FramedWriteError::BodyFull {
                body_len: 5,
                max_body_len: 4,
            })
        );
        assert_eq!(
            writer.push_frame(b"wxyz"),
            Err(FramedWriteError::BatchFull {
                encoded_len: 4,
                frame_len: 5,
                max_encoded_len: 8,
            })
        );
        writer
            .push_frame(b"xyz")
            .expect("smaller frame refills cap");
        assert_eq!(
            writer.encoded.as_deref(),
            Some(&[3, b'a', b'b', b'c', 3, b'x', b'y', b'z'][..])
        );
    }

    #[test]
    fn length_writer_clamps_body_cap_to_prefix_range() {
        let mut writer =
            UnixFramedWriter::length_delimited(stream(), LengthPrefix::U8, usize::MAX, usize::MAX);
        assert_eq!(
            writer.push_frame(vec![0; 256]),
            Err(FramedWriteError::BodyFull {
                body_len: 256,
                max_body_len: 255,
            })
        );
    }

    #[test]
    fn empty_batch_stays_appendable_and_started_batch_is_frozen() {
        let mut writer = UnixFramedWriter::lines(stream(), 8, 16);
        assert!(writer.next_effect::<Dummy, _, _>(Msg::Wrote).is_none());
        assert!(!writer.is_started());

        writer.push_frame(b"ping").unwrap();
        assert!(writer.next_effect::<Dummy, _, _>(Msg::Wrote).is_some());
        assert!(writer.is_started());
        assert_eq!(
            writer.push_frame(b"later"),
            Err(FramedWriteError::AlreadyStarted)
        );
    }

    #[test]
    fn unarmed_advance_fails_as_an_invariant_violation() {
        let mut writer = UnixFramedWriter::lines(stream(), 8, 16);
        let reply = Ok(crate::call::WriteOwnedReply {
            bytes: b"ping\n".to_vec(),
            written: 5,
        });
        let step: LoopStep<Dummy, usize> = writer.advance(reply, Msg::Wrote);
        assert!(matches!(
            step,
            LoopStep::Failed(CallError::InvariantViolation)
        ));
    }

    #[test]
    #[should_panic(expected = "max_line_len > 0")]
    fn zero_line_cap_is_rejected_like_the_decoder() {
        let _ = UnixFramedWriter::lines(stream(), 0, 16);
    }
}
