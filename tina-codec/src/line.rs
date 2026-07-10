//! Line-delimited framing.

use crate::FrameDecision;

/// Why a line is malformed and the connection cannot recover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MalformedLineReason {
    /// The buffer reached EOF without a trailing newline. Only
    /// surfaced via [`LineFramer::finish`].
    UnterminatedFinalLine,

    /// The framer was configured to reject embedded NUL bytes and one
    /// was seen.
    EmbeddedNul,
}

/// Newline-delimited line framer.
///
/// Frames are terminated by `\n`. A trailing `\r` before `\n` is
/// stripped from the produced frame. The framer enforces a
/// configured maximum line length: once any unframed prefix grows
/// past `max_line_len`, [`Self::next_frame`] returns
/// [`FrameDecision::Full`] before allocating any further.
///
/// `feed` stops at one newline and reports bytes consumed. The current
/// unframed prefix is capped at `max_line_len + 2` (payload plus optional CR
/// and newline), so the framer never grows with the number of frames coalesced
/// into one transport read.
#[derive(Debug, Clone)]
pub struct LineFramer {
    buffer: Vec<u8>,
    max_line_len: usize,
    reject_nul: bool,
    poisoned: bool,
    overflowed: bool,
}

impl LineFramer {
    /// Build a line framer with the given maximum line length (in
    /// bytes, excluding the `\n` terminator).
    ///
    /// # Panics
    ///
    /// Panics if `max_line_len == 0`. A zero cap could not accept any
    /// non-empty line.
    pub fn new(max_line_len: usize) -> Self {
        assert!(max_line_len > 0, "LineFramer requires max_line_len > 0");
        Self {
            buffer: Vec::new(),
            max_line_len,
            reject_nul: false,
            poisoned: false,
            overflowed: false,
        }
    }

    /// Configure the framer to treat embedded NUL bytes as malformed
    /// instead of passing them through. Off by default.
    pub fn reject_embedded_nul(mut self) -> Self {
        self.reject_nul = true;
        self
    }

    /// Feeds received bytes up to the end of the current line.
    ///
    /// If the framer already saw a [`FrameDecision::Full`] or
    /// [`FrameDecision::Malformed`] outcome, additional bytes are
    /// ignored and the next [`Self::next_frame`] keeps returning the
    /// same terminal decision. This keeps the framer self-clamping:
    /// the caller cannot accidentally make the buffer grow past the
    /// cap by pushing more bytes after a fatal decision.
    ///
    /// Returns the number of input bytes consumed. The method stops after one
    /// newline so memory remains bounded by one frame even when a transport
    /// read coalesces many frames. Use [`crate::decode_chunk`] to consume and
    /// drain an entire transport chunk in one call.
    #[must_use = "retain and resubmit unconsumed bytes, or use decode_chunk"]
    pub fn feed(&mut self, bytes: impl AsRef<[u8]>) -> usize {
        if self.poisoned || self.overflowed {
            return 0;
        }
        let chunk = bytes.as_ref();
        if chunk.is_empty() {
            return 0;
        }
        if self.buffer.last() == Some(&b'\n') {
            return 0;
        }
        // The +2 admits CRLF after an exact-cap line. It also bounds the proof
        // that a non-CRLF line exceeded its cap to two extra bytes.
        let room = self
            .max_line_len
            .saturating_add(2)
            .saturating_sub(self.buffer.len());
        let through_newline = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |index| index + 1);
        let consumed = room.min(through_newline);
        self.buffer.extend_from_slice(&chunk[..consumed]);
        if consumed == room && self.buffer.last() != Some(&b'\n') {
            self.overflowed = true;
        }
        consumed
    }

    /// Try to extract the next complete frame.
    pub fn next_frame(&mut self) -> FrameDecision<Vec<u8>, MalformedLineReason> {
        if self.poisoned {
            return FrameDecision::Malformed(MalformedLineReason::EmbeddedNul);
        }
        if !self.buffer.is_empty() {
            if self.reject_nul {
                if let Some(abs) = self.buffer.iter().position(|&b| b == 0) {
                    // Did the NUL come before the next newline?
                    let nl_abs = self.buffer.iter().position(|&b| b == b'\n');
                    if nl_abs.is_none_or(|nl| abs < nl) {
                        self.poisoned = true;
                        return FrameDecision::Malformed(MalformedLineReason::EmbeddedNul);
                    }
                }
            }
            if let Some(nl) = self.buffer.iter().position(|&b| b == b'\n') {
                // Strip optional CR.
                let body_end = if nl > 0 && self.buffer[nl - 1] == b'\r' {
                    nl - 1
                } else {
                    nl
                };
                if body_end > self.max_line_len {
                    // Frame body alone exceeds cap.
                    self.overflowed = true;
                    return FrameDecision::Full;
                }
                let mut frame = std::mem::take(&mut self.buffer);
                frame.truncate(body_end);
                return FrameDecision::Frame(frame);
            }
        }
        if self.overflowed {
            FrameDecision::Full
        } else {
            FrameDecision::NeedMore
        }
    }

    /// Signal EOF on the byte stream.
    ///
    /// Returns the leftover buffer as a final frame if any non-empty
    /// bytes are still pending. Some protocols define the last
    /// message as unterminated; others reject it. The caller chooses:
    /// pass `allow_unterminated_final = true` to receive the trailing
    /// bytes as a final frame; pass `false` to mark the connection
    /// malformed when there are leftover bytes without a `\n`.
    pub fn finish(
        &mut self,
        allow_unterminated_final: bool,
    ) -> FrameDecision<Vec<u8>, MalformedLineReason> {
        if self.poisoned {
            return FrameDecision::Malformed(MalformedLineReason::EmbeddedNul);
        }
        if self.overflowed {
            return FrameDecision::Full;
        }
        if self.buffer.is_empty() {
            return FrameDecision::NeedMore;
        }
        let body_end = if self.buffer.last() == Some(&b'\r') {
            self.buffer.len() - 1
        } else {
            self.buffer.len()
        };
        if body_end > self.max_line_len {
            self.overflowed = true;
            return FrameDecision::Full;
        }
        if allow_unterminated_final {
            let mut body = std::mem::take(&mut self.buffer);
            body.truncate(body_end);
            FrameDecision::Frame(body)
        } else {
            FrameDecision::Malformed(MalformedLineReason::UnterminatedFinalLine)
        }
    }

    /// Bytes currently buffered, awaiting a newline.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DecodeStatus, decode_chunk};

    #[test]
    fn extracts_one_line_per_call() {
        let mut framer = LineFramer::new(64);
        let mut frames = Vec::new();
        let status = decode_chunk(&mut framer, b"hello\nworld\n", |frame| frames.push(frame));
        assert_eq!(frames, [b"hello".to_vec(), b"world".to_vec()]);
        assert_eq!(status, DecodeStatus::NeedMore);
    }

    #[test]
    fn strips_trailing_cr() {
        let mut framer = LineFramer::new(64);
        let _ = framer.feed(b"hello\r\n");
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"hello".to_vec()));
    }

    #[test]
    fn split_across_feeds() {
        let mut framer = LineFramer::new(64);
        let mut frames = Vec::new();
        assert_eq!(
            decode_chunk(&mut framer, b"he", |frame| frames.push(frame)),
            DecodeStatus::NeedMore
        );
        assert_eq!(
            decode_chunk(&mut framer, b"llo\nwo", |frame| frames.push(frame)),
            DecodeStatus::NeedMore
        );
        assert_eq!(frames, [b"hello".to_vec()]);
        assert_eq!(
            decode_chunk(&mut framer, b"rld\n", |frame| frames.push(frame)),
            DecodeStatus::NeedMore
        );
        assert_eq!(frames, [b"hello".to_vec(), b"world".to_vec()]);
    }

    #[test]
    fn rejects_line_too_long_before_growing() {
        let mut framer = LineFramer::new(4);
        // Push 8 bytes — the framer must hard-cap at 6 (max + CRLF) and
        // surface Full instead of expanding further.
        let _ = framer.feed(b"abcdefgh");
        assert!(framer.buffered() <= 6, "buffer must not grow past cap");
        assert_eq!(framer.next_frame(), FrameDecision::Full);
        // Feeding more after Full is a no-op.
        let _ = framer.feed(b"ijkl");
        assert!(framer.buffered() <= 6);
        assert_eq!(framer.next_frame(), FrameDecision::Full);
    }

    #[test]
    fn rejects_line_exact_max_plus_one() {
        let mut framer = LineFramer::new(3);
        let _ = framer.feed(b"abcd\n");
        // 4-byte line + newline. Body > 3, so Full.
        assert_eq!(framer.next_frame(), FrameDecision::Full);
    }

    #[test]
    fn accepts_line_exact_max() {
        let mut framer = LineFramer::new(3);
        let _ = framer.feed(b"abc\n");
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"abc".to_vec()));
    }

    #[test]
    fn accepts_crlf_line_at_exact_max_across_partitions() {
        let bytes = b"abcd\r\n";
        for split in 0..=bytes.len() {
            let mut framer = LineFramer::new(4);
            let mut frames = Vec::new();
            assert_eq!(
                decode_chunk(&mut framer, &bytes[..split], |frame| frames.push(frame)),
                DecodeStatus::NeedMore,
            );
            assert_eq!(
                decode_chunk(&mut framer, &bytes[split..], |frame| frames.push(frame)),
                DecodeStatus::NeedMore,
            );
            assert_eq!(frames, [b"abcd".to_vec()], "split at {split}");
        }
    }

    #[test]
    fn empty_line_is_a_frame() {
        let mut framer = LineFramer::new(64);
        let _ = framer.feed(b"\n");
        assert_eq!(framer.next_frame(), FrameDecision::Frame(Vec::new()));
    }

    #[test]
    fn rejects_embedded_nul_when_configured() {
        let mut framer = LineFramer::new(64).reject_embedded_nul();
        let _ = framer.feed(b"he\0llo\n");
        assert_eq!(
            framer.next_frame(),
            FrameDecision::Malformed(MalformedLineReason::EmbeddedNul)
        );
        // Stays malformed.
        let _ = framer.feed(b"world\n");
        assert_eq!(
            framer.next_frame(),
            FrameDecision::Malformed(MalformedLineReason::EmbeddedNul)
        );
    }

    #[test]
    fn finish_returns_unterminated_when_allowed() {
        let mut framer = LineFramer::new(64);
        let _ = framer.feed(b"final");
        assert_eq!(framer.finish(true), FrameDecision::Frame(b"final".to_vec()));
    }

    #[test]
    fn finish_rejects_unterminated_when_disallowed() {
        let mut framer = LineFramer::new(64);
        let _ = framer.feed(b"final");
        assert_eq!(
            framer.finish(false),
            FrameDecision::Malformed(MalformedLineReason::UnterminatedFinalLine)
        );
    }

    #[test]
    fn finish_on_empty_returns_need_more() {
        let mut framer = LineFramer::new(64);
        assert_eq!(framer.finish(true), FrameDecision::NeedMore);
        assert_eq!(framer.finish(false), FrameDecision::NeedMore);
    }

    #[test]
    fn repeated_small_feeds_scan_the_complete_line() {
        // Stress: many small feeds, only the last contains the newline.
        let mut framer = LineFramer::new(1024);
        for _ in 0..8 {
            let _ = framer.feed(b"....");
            assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
        }
        let _ = framer.feed(b"end\n");
        match framer.next_frame() {
            FrameDecision::Frame(body) => {
                assert_eq!(body.len(), 8 * 4 + 3);
            }
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn embedded_nul_split_across_feeds_is_caught() {
        // Feed a no-newline prefix, then a chunk whose NUL sits before the
        // eventual newline. The complete current line must be validated.
        let mut framer = LineFramer::new(1024).reject_embedded_nul();
        let _ = framer.feed(b"abc");
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
        let _ = framer.feed(b"de\0fg\n");
        assert_eq!(
            framer.next_frame(),
            FrameDecision::Malformed(MalformedLineReason::EmbeddedNul),
        );
    }

    #[test]
    fn overflow_then_more_feeds_stays_full_and_bounded() {
        // Once a line exceeds the cap, the buffer must not keep growing
        // across subsequent feeds, and the decision stays `Full`.
        let mut framer = LineFramer::new(4);
        let _ = framer.feed(b"abcd"); // exactly at cap, no newline yet
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
        let _ = framer.feed(b"efgh"); // pushes past cap
        let before = framer.buffered();
        assert!(before <= 6, "buffer must not grow past cap+2");
        assert_eq!(framer.next_frame(), FrameDecision::Full);
        // Flooding more bytes does not grow the buffer or change the verdict.
        for _ in 0..100 {
            let _ = framer.feed(b"zzzzzzzzzz");
        }
        assert!(framer.buffered() <= 6);
        assert_eq!(framer.next_frame(), FrameDecision::Full);
    }
}
