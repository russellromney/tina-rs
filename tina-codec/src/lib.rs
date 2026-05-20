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
/// This is the "sync codec adapter pattern": `feed` bytes in, pull one
/// [`FrameDecision`] out per `next_frame`. The trait is sealed — only
/// [`LineFramer`] and [`LengthDelimitedFramer`] implement it — so it
/// exists to let generic code drive either framer, not as an extension
/// point. There is deliberately no async variant; codec state lives on
/// the caller's isolate and Tina owns the I/O that produces the bytes.
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
