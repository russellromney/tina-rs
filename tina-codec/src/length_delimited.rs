//! Length-prefixed framing.
//!
//! Frame layout:
//!
//! ```text
//! [length: N bytes big-endian unsigned][body: length bytes]
//! ```
//!
//! Where `N` is the configured [`LengthPrefix`] width. Bodies longer
//! than the configured cap are rejected before allocation: the
//! framer surfaces [`crate::FrameDecision::Full`] when the prefix
//! announces a length larger than the maximum, instead of growing a
//! buffer to fit it.

use crate::FrameDecision;

/// Width of the length prefix, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LengthPrefix {
    /// 1-byte length prefix (0..=255).
    U8,
    /// 2-byte big-endian unsigned length prefix.
    U16,
    /// 4-byte big-endian unsigned length prefix.
    U32,
}

impl LengthPrefix {
    /// Width of this prefix in bytes.
    pub const fn width(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
        }
    }

    /// Decode the body length from a prefix slice without owning a
    /// parser. Returns `None` if `bytes` is shorter than [`Self::width`].
    ///
    /// Symmetric peek companion to [`encode_into`]; useful for callers
    /// that want to inspect an announced length (e.g. to reject before
    /// buffering) without driving a [`LengthDelimitedFramer`].
    pub fn decode_length(self, bytes: &[u8]) -> Option<u64> {
        if bytes.len() < self.width() {
            return None;
        }
        Some(self.decode(bytes))
    }

    fn decode(self, bytes: &[u8]) -> u64 {
        match self {
            Self::U8 => bytes[0] as u64,
            Self::U16 => u16::from_be_bytes([bytes[0], bytes[1]]) as u64,
            Self::U32 => u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64,
        }
    }
}

/// Why a length-delimited stream is unrecoverably malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MalformedLengthReason {
    /// The byte stream ended in the middle of a frame: prefix or
    /// body bytes are still expected. Surfaced via
    /// [`LengthDelimitedFramer::finish`].
    UnexpectedEof,
}

/// Length-prefixed frame parser.
///
/// State machine over the byte stream: collect the prefix, decode
/// the body length, reject lengths over the cap before allocation,
/// then collect the body. There is no growth beyond the cap and no
/// hidden allocation: the caller chooses [`LengthPrefix`] and
/// `max_body_len`.
///
/// `feed` only buffers raw bytes. `next_frame` walks the parser
/// state machine: it consumes the prefix, checks the body length
/// against the configured cap *before* any body bytes are
/// allocated, then drains the body and returns it as a frame.
#[derive(Debug, Clone)]
pub struct LengthDelimitedFramer {
    prefix: LengthPrefix,
    max_body_len: usize,
    buffer: Vec<u8>,
    /// Either `None` (we are about to read a prefix) or
    /// `Some(body_len)` (prefix decoded, awaiting body bytes).
    expected_body_len: Option<usize>,
    overflowed: bool,
    poisoned: bool,
}

impl LengthDelimitedFramer {
    /// Build a length-delimited framer.
    ///
    /// `max_body_len` caps the announced body length. A frame whose
    /// declared length exceeds this cap is rejected before any body
    /// bytes are allocated.
    ///
    /// # Panics
    ///
    /// Panics if `max_body_len == 0`. A zero cap could not accept any
    /// non-empty frame.
    pub fn new(prefix: LengthPrefix, max_body_len: usize) -> Self {
        assert!(
            max_body_len > 0,
            "LengthDelimitedFramer requires max_body_len > 0"
        );
        Self {
            prefix,
            max_body_len,
            buffer: Vec::new(),
            expected_body_len: None,
            overflowed: false,
            poisoned: false,
        }
    }

    /// Push received bytes into the parser.
    ///
    /// Bytes are buffered; parsing happens in [`Self::next_frame`].
    /// The buffer is hard-capped at `prefix_width + max_body_len`:
    /// once that cap is reached, additional bytes are dropped and
    /// [`Self::next_frame`] will return [`FrameDecision::Full`] for
    /// the offending frame.
    ///
    /// Note on the cap: the bound includes one prefix width so a frame
    /// at exactly `max_body_len` plus its prefix fits. When residual
    /// bytes for a *following* frame are already buffered, the cap is
    /// measured against current buffer occupancy, so a maximal frame
    /// arriving while up to `prefix_width` trailing bytes are buffered
    /// can be truncated and reported `Full` `prefix_width` bytes early.
    /// This errs strict (never under-strict): a frame is never accepted
    /// past the cap. The real authority is the body-length check in
    /// [`Self::next_frame`], which rejects an oversized *declared*
    /// length before allocating any body bytes regardless of buffering.
    pub fn feed(&mut self, bytes: impl AsRef<[u8]>) {
        if self.overflowed || self.poisoned {
            return;
        }
        let chunk = bytes.as_ref();
        if chunk.is_empty() {
            return;
        }
        let max_buffered = self.prefix.width().saturating_add(self.max_body_len);
        let room = max_buffered.saturating_sub(self.buffer.len());
        if chunk.len() > room {
            self.buffer.extend_from_slice(&chunk[..room]);
            self.overflowed = true;
        } else {
            self.buffer.extend_from_slice(chunk);
        }
    }

    /// Try to extract the next complete frame.
    pub fn next_frame(&mut self) -> FrameDecision<Vec<u8>, MalformedLengthReason> {
        if self.poisoned {
            return FrameDecision::Malformed(MalformedLengthReason::UnexpectedEof);
        }
        // First handle the case where the prefix is not yet decoded.
        if self.expected_body_len.is_none() {
            let width = self.prefix.width();
            if self.buffer.len() < width {
                if self.overflowed {
                    return FrameDecision::Full;
                }
                return FrameDecision::NeedMore;
            }
            let body_len = self.prefix.decode(&self.buffer[..width]);
            if body_len > self.max_body_len as u64 {
                self.overflowed = true;
                // Drop everything; further bytes go nowhere.
                self.buffer.clear();
                return FrameDecision::Full;
            }
            self.buffer.drain(..width);
            self.expected_body_len = Some(body_len as usize);
        }
        // We have a decoded body length; pull the body bytes if ready.
        let body_len = self
            .expected_body_len
            .expect("expected_body_len was just set");
        if self.buffer.len() < body_len {
            if self.overflowed {
                return FrameDecision::Full;
            }
            return FrameDecision::NeedMore;
        }
        let frame: Vec<u8> = self.buffer.drain(..body_len).collect();
        self.expected_body_len = None;
        FrameDecision::Frame(frame)
    }

    /// Signal EOF on the byte stream.
    ///
    /// Returns `NeedMore` only when the parser is sitting idle on a
    /// clean frame boundary (no partial prefix or body); otherwise
    /// surfaces the typed malformed reason.
    pub fn finish(&mut self) -> FrameDecision<Vec<u8>, MalformedLengthReason> {
        if self.overflowed {
            return FrameDecision::Full;
        }
        if self.poisoned {
            return FrameDecision::Malformed(MalformedLengthReason::UnexpectedEof);
        }
        match self.expected_body_len {
            None if self.buffer.is_empty() => FrameDecision::NeedMore,
            Some(0) => {
                // Empty trailing frame already ready; deliver it.
                self.next_frame()
            }
            _ => {
                self.poisoned = true;
                FrameDecision::Malformed(MalformedLengthReason::UnexpectedEof)
            }
        }
    }

    /// Configured prefix width.
    pub const fn prefix(&self) -> LengthPrefix {
        self.prefix
    }

    /// Configured body cap.
    pub const fn max_body_len(&self) -> usize {
        self.max_body_len
    }

    /// Bytes currently buffered.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }
}

/// Encode one length-prefixed frame onto the end of `into`.
///
/// Returns the number of bytes appended. Returns `None` if `body.len()`
/// exceeds the prefix's representable range.
///
/// Pure data helper for symmetry with the parser. The caller chooses
/// the destination buffer.
pub fn encode_into(prefix: LengthPrefix, body: &[u8], into: &mut Vec<u8>) -> Option<usize> {
    let max = match prefix {
        LengthPrefix::U8 => u8::MAX as usize,
        LengthPrefix::U16 => u16::MAX as usize,
        LengthPrefix::U32 => u32::MAX as usize,
    };
    if body.len() > max {
        return None;
    }
    let start = into.len();
    match prefix {
        LengthPrefix::U8 => into.push(body.len() as u8),
        LengthPrefix::U16 => into.extend_from_slice(&(body.len() as u16).to_be_bytes()),
        LengthPrefix::U32 => into.extend_from_slice(&(body.len() as u32).to_be_bytes()),
    }
    into.extend_from_slice(body);
    Some(into.len() - start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_one_frame() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U16, 64);
        framer.feed([0u8, 5, b'h', b'e', b'l', b'l', b'o']);
        match framer.next_frame() {
            FrameDecision::Frame(body) => assert_eq!(body, b"hello"),
            other => panic!("unexpected decision: {other:?}"),
        }
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
    }

    #[test]
    fn decodes_back_to_back_frames() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U8, 64);
        framer.feed([3, b'f', b'o', b'o', 4, b'q', b'u', b'u', b'x']);
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"foo".to_vec()));
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"quux".to_vec()));
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
    }

    #[test]
    fn split_prefix_and_body() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U16, 64);
        framer.feed([0u8]);
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
        framer.feed([5u8]);
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
        framer.feed(b"he");
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
        framer.feed(b"llo");
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"hello".to_vec()));
    }

    #[test]
    fn rejects_frame_too_large_before_allocation() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U16, 4);
        // Announces 8-byte body; cap is 4. Reject before any body byte is read.
        framer.feed([0u8, 8]);
        assert_eq!(framer.next_frame(), FrameDecision::Full);
        // Further feeds are dropped.
        framer.feed(b"AAAAAAAA");
        assert_eq!(framer.next_frame(), FrameDecision::Full);
    }

    #[test]
    fn valid_frame_before_oversize_frame_is_delivered_intact() {
        // A complete valid frame, then an oversize declaration (5 > cap 4).
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U8, 4);
        framer.feed([3u8, b'a', b'b', b'c', 5u8, b'X']);
        // The good frame comes out clean first...
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"abc".to_vec()));
        // ...and only the oversize frame is rejected. The malformed tail did
        // not corrupt the valid frame that preceded it.
        assert_eq!(framer.next_frame(), FrameDecision::Full);
    }

    #[test]
    fn rejects_frame_at_cap_plus_one() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U8, 4);
        framer.feed([5u8]);
        assert_eq!(framer.next_frame(), FrameDecision::Full);
    }

    #[test]
    fn accepts_frame_at_exact_cap() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U8, 4);
        framer.feed([4u8, b'a', b'b', b'c', b'd']);
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"abcd".to_vec()));
    }

    #[test]
    fn empty_body_frame_is_valid() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U8, 8);
        framer.feed([0u8]);
        assert_eq!(framer.next_frame(), FrameDecision::Frame(Vec::new()));
    }

    #[test]
    fn finish_clean_boundary_returns_need_more() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U16, 64);
        assert_eq!(framer.finish(), FrameDecision::NeedMore);
    }

    #[test]
    fn finish_partial_prefix_is_malformed() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U16, 64);
        framer.feed([1u8]); // only half a u16 prefix
        assert_eq!(
            framer.finish(),
            FrameDecision::Malformed(MalformedLengthReason::UnexpectedEof)
        );
    }

    #[test]
    fn finish_partial_body_is_malformed() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U16, 64);
        framer.feed([0u8, 5, b'h', b'e']);
        assert_eq!(
            framer.finish(),
            FrameDecision::Malformed(MalformedLengthReason::UnexpectedEof)
        );
    }

    #[test]
    fn encode_into_round_trips() {
        let mut buffer = Vec::new();
        let n = encode_into(LengthPrefix::U16, b"hello", &mut buffer).expect("encode ok");
        assert_eq!(n, 7);
        assert_eq!(buffer, [0u8, 5, b'h', b'e', b'l', b'l', b'o']);

        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U16, 64);
        framer.feed(&buffer);
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"hello".to_vec()));
    }

    #[test]
    fn encode_into_rejects_body_too_large_for_prefix() {
        let body = vec![0u8; 300];
        let mut buffer = Vec::new();
        assert!(encode_into(LengthPrefix::U8, &body, &mut buffer).is_none());
        // Buffer not touched on rejection.
        assert!(buffer.is_empty());
    }

    #[test]
    fn buffer_does_not_grow_past_cap_under_flood() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U8, 4);
        for _ in 0..10 {
            framer.feed(b"AAAAAAAAA");
        }
        // Cap = prefix(1) + body(4) = 5
        assert!(framer.buffered() <= 5);
    }

    #[test]
    fn decode_length_peeks_without_a_parser() {
        assert_eq!(LengthPrefix::U16.decode_length(&[0u8, 5]), Some(5));
        assert_eq!(LengthPrefix::U8.decode_length(&[200u8]), Some(200));
        assert_eq!(LengthPrefix::U32.decode_length(&[0u8, 0, 1, 0]), Some(256));
        // Short slice: cannot decode yet.
        assert_eq!(LengthPrefix::U16.decode_length(&[0u8]), None);
        assert_eq!(LengthPrefix::U32.decode_length(&[]), None);
    }

    #[test]
    fn framer_trait_drives_length_delimited() {
        use crate::Framer;
        fn drain_first<F: Framer<Frame = Vec<u8>>>(
            framer: &mut F,
            bytes: &[u8],
        ) -> Option<Vec<u8>> {
            framer.feed(bytes);
            match framer.next_frame() {
                FrameDecision::Frame(frame) => Some(frame),
                _ => None,
            }
        }
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U8, 16);
        assert_eq!(
            drain_first(&mut framer, &[3, b'f', b'o', b'o']),
            Some(b"foo".to_vec())
        );
    }
}
