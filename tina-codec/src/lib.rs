#![deny(unsafe_code)]
#![deny(missing_docs)]

//! Sync codec helpers for Tina isolates.
//!
//! Codecs in Tina turn bytes into frames and frames into bytes. They
//! do not own sockets, files, or any I/O at all. Tina owns I/O,
//! capacity, cancellation, and replay; codec state lives inside the
//! isolate's own state struct.
//!
//! This crate ships two helpers:
//!
//! - [`LineFramer`] — newline-delimited text frames with an explicit
//!   maximum line length.
//! - [`LengthDelimitedFramer`] — a fixed-width unsigned big-endian
//!   length prefix followed by an opaque payload, with an explicit
//!   maximum frame body length.
//!
//! Both follow the same shape: push bytes in with
//! [`LineFramer::feed`] / [`LengthDelimitedFramer::feed`]; pull frames
//! out one at a time with [`LineFramer::next_frame`] /
//! [`LengthDelimitedFramer::next_frame`]. Each pull returns a
//! [`FrameDecision`]:
//!
//! - [`FrameDecision::NeedMore`] — keep reading from the rail.
//! - [`FrameDecision::Frame`] — here is one complete frame.
//! - [`FrameDecision::Malformed`] — the byte stream cannot ever
//!   produce a valid frame again; surface the typed reason and tear
//!   the connection down.
//! - [`FrameDecision::Full`] — the byte stream tried to exceed the
//!   configured cap; tear the connection down instead of growing
//!   unbounded.
//!
//! There is no async trait. There are no background tasks. Codec
//! authors stay honest about pressure: the framer's buffer is
//! caller-bounded and the "this stream exceeded the cap" outcome is
//! a typed variant, not an error string.
//!
//! ## Usage
//!
//! Hold the framer on your isolate's state and feed it incoming bytes:
//!
//! ```no_run
//! use tina_codec::{FrameDecision, LineFramer};
//!
//! struct Connection {
//!     framer: LineFramer,
//!     // ...
//! }
//!
//! impl Connection {
//!     fn on_bytes(&mut self, bytes: Vec<u8>) {
//!         self.framer.feed(bytes);
//!         loop {
//!             match self.framer.next_frame() {
//!                 FrameDecision::NeedMore => break,
//!                 FrameDecision::Frame(line) => {
//!                     // hand `line` to application logic
//!                     let _ = line;
//!                 }
//!                 FrameDecision::Malformed(reason) => {
//!                     // close the connection and tag the trace
//!                     let _ = reason;
//!                     break;
//!                 }
//!                 FrameDecision::Full => {
//!                     // line longer than cap; tear connection down
//!                     break;
//!                 }
//!             }
//!         }
//!     }
//! }
//! ```
//!
//! Tina's runtime hands `bytes` in via the ordinary `tcp_read` /
//! `tls_read` / `unix_read` continuation message. There is no second
//! reactor — codec state lives on the isolate itself.

mod length_delimited;
mod line;

pub use length_delimited::{
    LengthDelimitedFramer, LengthPrefix, MalformedLengthReason, encode_into,
};
pub use line::{LineFramer, MalformedLineReason};

/// Outcome of asking a framer for the next frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameDecision<F, M> {
    /// The current buffer does not yet contain a complete frame; the
    /// caller should issue another `tcp_read` / `tls_read` /
    /// `unix_read` and feed the result back to the framer.
    NeedMore,

    /// A complete frame is available.
    Frame(F),

    /// The byte stream can no longer produce a valid frame. Surface
    /// the typed reason and tear the connection down.
    Malformed(M),

    /// The byte stream tried to exceed the configured maximum frame
    /// body length. Tear the connection down instead of growing
    /// unbounded.
    Full,
}

impl<F, M> FrameDecision<F, M> {
    /// Returns whether this decision is `NeedMore`.
    pub const fn is_need_more(&self) -> bool {
        matches!(self, Self::NeedMore)
    }

    /// Returns whether this decision is `Frame`.
    pub const fn is_frame(&self) -> bool {
        matches!(self, Self::Frame(_))
    }

    /// Returns whether this decision is `Malformed`.
    pub const fn is_malformed(&self) -> bool {
        matches!(self, Self::Malformed(_))
    }

    /// Returns whether this decision is `Full`.
    pub const fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::LineFramer {}
    impl Sealed for super::LengthDelimitedFramer {}
}

/// Shared sync-codec surface for the framers in this crate.
///
/// `feed` bytes in, pull one [`FrameDecision`] out per `next_frame`. The
/// trait is sealed — only [`LineFramer`] and [`LengthDelimitedFramer`]
/// implement it — so it exists to let generic code drive either built-in
/// framer, not as an extension point.
///
/// Third-party codecs do **not** implement `Framer`. They implement
/// [`SyncCodec`], the open extension seam with the same shape. Both
/// built-in framers also implement [`SyncCodec`], so generic code that
/// wants to accept a built-in *or* a custom codec should be written
/// against [`SyncCodec`].
pub trait Framer: sealed::Sealed {
    /// The frame value produced on success.
    type Frame;
    /// The typed reason a stream is unrecoverably malformed.
    type Malformed;

    /// Push received bytes into the framer.
    fn feed(&mut self, bytes: &[u8]);

    /// Try to extract the next complete frame.
    fn next_frame(&mut self) -> FrameDecision<Self::Frame, Self::Malformed>;
}

impl Framer for LineFramer {
    type Frame = Vec<u8>;
    type Malformed = MalformedLineReason;

    fn feed(&mut self, bytes: &[u8]) {
        LineFramer::feed(self, bytes);
    }

    fn next_frame(&mut self) -> FrameDecision<Self::Frame, Self::Malformed> {
        LineFramer::next_frame(self)
    }
}

impl Framer for LengthDelimitedFramer {
    type Frame = Vec<u8>;
    type Malformed = MalformedLengthReason;

    fn feed(&mut self, bytes: &[u8]) {
        LengthDelimitedFramer::feed(self, bytes);
    }

    fn next_frame(&mut self) -> FrameDecision<Self::Frame, Self::Malformed> {
        LengthDelimitedFramer::next_frame(self)
    }
}

/// The public sync-codec extension seam.
///
/// This is the "sync codec adapter pattern" as an open trait: a codec is
/// synchronous parser state that turns bytes into frames. `feed` bytes
/// in, pull one [`FrameDecision`] out per `next_frame`. Unlike
/// [`Framer`], `SyncCodec` is **not** sealed — third-party crates
/// implement it for their own codec types.
///
/// The contract a custom codec must keep, so it stays a good Tina
/// citizen:
///
/// - **No I/O.** A codec never reads or writes a socket, file, or pipe.
///   Tina owns I/O, capacity, cancellation, and replay; the codec is
///   plain state that lives on the caller's isolate. There is
///   deliberately no async variant.
/// - **Bounded.** The codec's own buffer must be caller-bounded. When a
///   stream tries to exceed the configured cap, return
///   [`FrameDecision::Full`] *before* allocating further, instead of
///   growing without limit. "This stream exceeded the cap" is a typed
///   outcome, not an error string and not an unbounded buffer.
/// - **Replayable.** `feed` + `next_frame` are pure functions of the
///   bytes seen so far. The same byte sequence always produces the same
///   frame sequence, so a codec on a simulated socket replays exactly
///   like one on a live socket.
///
/// Both [`LineFramer`] and [`LengthDelimitedFramer`] implement this
/// trait, so generic code can accept any built-in or custom codec:
///
/// ```
/// use tina_codec::{FrameDecision, SyncCodec};
///
/// fn drain_all<C: SyncCodec>(codec: &mut C, bytes: &[u8]) -> usize {
///     codec.feed(bytes);
///     let mut frames = 0;
///     loop {
///         match codec.next_frame() {
///             FrameDecision::Frame(_) => frames += 1,
///             // NeedMore / Malformed / Full all stop this drain.
///             _ => break,
///         }
///     }
///     frames
/// }
///
/// let mut framer = tina_codec::LineFramer::new(1024);
/// assert_eq!(drain_all(&mut framer, b"a\nb\n"), 2);
/// ```
pub trait SyncCodec {
    /// The frame value produced on success.
    type Frame;
    /// The typed reason a stream is unrecoverably malformed.
    type Malformed;

    /// Push received bytes into the codec. Must not block, allocate
    /// unboundedly, or perform I/O.
    fn feed(&mut self, bytes: &[u8]);

    /// Try to extract the next complete frame. Returns
    /// [`FrameDecision::Full`] instead of growing past the configured
    /// cap.
    fn next_frame(&mut self) -> FrameDecision<Self::Frame, Self::Malformed>;
}

impl SyncCodec for LineFramer {
    type Frame = Vec<u8>;
    type Malformed = MalformedLineReason;

    fn feed(&mut self, bytes: &[u8]) {
        LineFramer::feed(self, bytes);
    }

    fn next_frame(&mut self) -> FrameDecision<Self::Frame, Self::Malformed> {
        LineFramer::next_frame(self)
    }
}

impl SyncCodec for LengthDelimitedFramer {
    type Frame = Vec<u8>;
    type Malformed = MalformedLengthReason;

    fn feed(&mut self, bytes: &[u8]) {
        LengthDelimitedFramer::feed(self, bytes);
    }

    fn next_frame(&mut self) -> FrameDecision<Self::Frame, Self::Malformed> {
        LengthDelimitedFramer::next_frame(self)
    }
}

#[cfg(test)]
mod sync_codec_tests {
    use super::*;

    /// A custom codec defined outside the built-in set. Splits on a
    /// single-byte delimiter and refuses to buffer past `cap`.
    struct ByteSplitCodec {
        buf: Vec<u8>,
        delim: u8,
        cap: usize,
        full: bool,
    }

    impl ByteSplitCodec {
        fn new(delim: u8, cap: usize) -> Self {
            Self {
                buf: Vec::new(),
                delim,
                cap,
                full: false,
            }
        }
    }

    impl SyncCodec for ByteSplitCodec {
        type Frame = Vec<u8>;
        type Malformed = ();

        fn feed(&mut self, bytes: &[u8]) {
            // Bounded: stop appending once the unframed prefix would
            // exceed the cap, and latch the Full outcome.
            if self.full {
                return;
            }
            if self.buf.len() + bytes.len() > self.cap && !self.buf.contains(&self.delim) {
                self.full = true;
                return;
            }
            self.buf.extend_from_slice(bytes);
        }

        fn next_frame(&mut self) -> FrameDecision<Self::Frame, Self::Malformed> {
            if let Some(idx) = self.buf.iter().position(|b| *b == self.delim) {
                let mut frame = self.buf.split_off(idx + 1);
                std::mem::swap(&mut frame, &mut self.buf);
                frame.pop(); // drop the delimiter
                return FrameDecision::Frame(frame);
            }
            if self.full {
                return FrameDecision::Full;
            }
            FrameDecision::NeedMore
        }
    }

    fn drain<C: SyncCodec<Frame = Vec<u8>>>(
        codec: &mut C,
        bytes: &[u8],
    ) -> Vec<FrameDecision<Vec<u8>, C::Malformed>> {
        codec.feed(bytes);
        let mut out = Vec::new();
        loop {
            let d = codec.next_frame();
            let stop = !d.is_frame();
            out.push(d);
            if stop {
                break;
            }
        }
        out
    }

    #[test]
    fn external_codec_implements_sync_codec() {
        // The whole point of the seam: a type that is not LineFramer or
        // LengthDelimitedFramer can still be a SyncCodec and be driven
        // by generic code.
        let mut codec = ByteSplitCodec::new(b'|', 8);
        let out = drain(&mut codec, b"ab|cd|");
        assert!(matches!(out[0], FrameDecision::Frame(ref f) if f == b"ab"));
        assert!(matches!(out[1], FrameDecision::Frame(ref f) if f == b"cd"));
        assert!(matches!(out[2], FrameDecision::NeedMore));
    }

    #[test]
    fn external_codec_reports_full_when_bounded() {
        let mut codec = ByteSplitCodec::new(b'|', 4);
        // No delimiter inside the cap window: bounded refusal, not an
        // unbounded buffer.
        let out = drain(&mut codec, b"abcdefgh");
        assert!(matches!(out.last(), Some(FrameDecision::Full)));
    }

    #[test]
    fn builtin_framers_drive_through_sync_codec() {
        // Generic SyncCodec code accepts the built-ins too.
        let mut line = LineFramer::new(64);
        let out = drain(&mut line, b"x\ny\n");
        assert!(matches!(out[0], FrameDecision::Frame(ref f) if f == b"x"));
        assert!(matches!(out[1], FrameDecision::Frame(ref f) if f == b"y"));
    }
}
