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
/// The buffer is bounded by the caller: `feed` adds bytes, but once
/// the unframed prefix exceeds `max_line_len`, subsequent calls
/// return `Full` without enlarging beyond the cap. The framer never
/// grows unboundedly.
#[derive(Debug, Clone)]
pub struct LineFramer {
    buffer: Vec<u8>,
    /// Position the search resumes from. Scanning is O(N) per byte
    /// without this hint when newlines arrive late.
    search_from: usize,
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
            search_from: 0,
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

    /// Feed received bytes into the framer.
    ///
    /// If the framer already saw a [`FrameDecision::Full`] or
    /// [`FrameDecision::Malformed`] outcome, additional bytes are
    /// dropped and the next [`Self::next_frame`] keeps returning the
    /// same terminal decision. This keeps the framer self-clamping:
    /// the caller cannot accidentally make the buffer grow past the
    /// cap by pushing more bytes after a fatal decision.
    pub fn feed(&mut self, bytes: impl AsRef<[u8]>) {
        if self.poisoned || self.overflowed {
            return;
        }
        let chunk = bytes.as_ref();
        if chunk.is_empty() {
            return;
        }
        // Hard cap: never let the unframed buffer grow past max_line_len + 1
        // (the +1 leaves room for the newline terminator). A chunk that
        // would push us past the cap flips the overflowed flag and
        // discards the rest of the chunk; subsequent calls return Full
        // without allocating.
        let room = self
            .max_line_len
            .saturating_add(1)
            .saturating_sub(self.buffer.len());
        if chunk.len() > room {
            self.buffer.extend_from_slice(&chunk[..room]);
            self.overflowed = true;
        } else {
            self.buffer.extend_from_slice(chunk);
        }
    }

    /// Try to extract the next complete frame.
    pub fn next_frame(&mut self) -> FrameDecision<Vec<u8>, MalformedLineReason> {
        if self.poisoned {
            return FrameDecision::Malformed(MalformedLineReason::EmbeddedNul);
        }
        // Scan from the saved hint forward for the next newline.
        let scan_start = self.search_from;
        if scan_start < self.buffer.len() {
            if self.reject_nul {
                if let Some(rel) = self.buffer[scan_start..].iter().position(|&b| b == 0) {
                    let abs = scan_start + rel;
                    // Did the NUL come before the next newline?
                    let nl_pos = self.buffer[scan_start..].iter().position(|&b| b == b'\n');
                    let nl_abs = nl_pos.map(|rel| scan_start + rel);
                    if nl_abs.is_none_or(|nl| abs < nl) {
                        self.poisoned = true;
                        return FrameDecision::Malformed(MalformedLineReason::EmbeddedNul);
                    }
                }
            }
            if let Some(rel) = self.buffer[scan_start..].iter().position(|&b| b == b'\n') {
                let nl = scan_start + rel;
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
                let frame: Vec<u8> = self.buffer.drain(..=nl).take(body_end).collect();
                self.search_from = 0;
                return FrameDecision::Frame(frame);
            }
            self.search_from = self.buffer.len();
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
            let body: Vec<u8> = self.buffer.drain(..).take(body_end).collect();
            self.search_from = 0;
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

    #[test]
    fn extracts_one_line_per_call() {
        let mut framer = LineFramer::new(64);
        framer.feed(b"hello\nworld\n");
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"hello".to_vec()));
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"world".to_vec()));
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
    }

    #[test]
    fn strips_trailing_cr() {
        let mut framer = LineFramer::new(64);
        framer.feed(b"hello\r\n");
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"hello".to_vec()));
    }

    #[test]
    fn split_across_feeds() {
        let mut framer = LineFramer::new(64);
        framer.feed(b"he");
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
        framer.feed(b"llo\nwo");
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"hello".to_vec()));
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
        framer.feed(b"rld\n");
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"world".to_vec()));
    }

    #[test]
    fn rejects_line_too_long_before_growing() {
        let mut framer = LineFramer::new(4);
        // Push 8 bytes — the framer must hard-cap at 5 (max + newline) and
        // surface Full instead of expanding further.
        framer.feed(b"abcdefgh");
        assert!(framer.buffered() <= 5, "buffer must not grow past cap");
        assert_eq!(framer.next_frame(), FrameDecision::Full);
        // Feeding more after Full is a no-op.
        framer.feed(b"ijkl");
        assert!(framer.buffered() <= 5);
        assert_eq!(framer.next_frame(), FrameDecision::Full);
    }

    #[test]
    fn rejects_line_exact_max_plus_one() {
        let mut framer = LineFramer::new(3);
        framer.feed(b"abcd\n");
        // 4-byte line + newline. Body > 3, so Full.
        assert_eq!(framer.next_frame(), FrameDecision::Full);
    }

    #[test]
    fn accepts_line_exact_max() {
        let mut framer = LineFramer::new(3);
        framer.feed(b"abc\n");
        assert_eq!(framer.next_frame(), FrameDecision::Frame(b"abc".to_vec()));
    }

    #[test]
    fn empty_line_is_a_frame() {
        let mut framer = LineFramer::new(64);
        framer.feed(b"\n");
        assert_eq!(framer.next_frame(), FrameDecision::Frame(Vec::new()));
    }

    #[test]
    fn rejects_embedded_nul_when_configured() {
        let mut framer = LineFramer::new(64).reject_embedded_nul();
        framer.feed(b"he\0llo\n");
        assert_eq!(
            framer.next_frame(),
            FrameDecision::Malformed(MalformedLineReason::EmbeddedNul)
        );
        // Stays malformed.
        framer.feed(b"world\n");
        assert_eq!(
            framer.next_frame(),
            FrameDecision::Malformed(MalformedLineReason::EmbeddedNul)
        );
    }

    #[test]
    fn finish_returns_unterminated_when_allowed() {
        let mut framer = LineFramer::new(64);
        framer.feed(b"final");
        assert_eq!(framer.finish(true), FrameDecision::Frame(b"final".to_vec()));
    }

    #[test]
    fn finish_rejects_unterminated_when_disallowed() {
        let mut framer = LineFramer::new(64);
        framer.feed(b"final");
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
    fn search_resumes_from_hint() {
        // Stress: many small feeds, only the last contains the newline.
        let mut framer = LineFramer::new(1024);
        for _ in 0..8 {
            framer.feed(b"....");
            assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
        }
        framer.feed(b"end\n");
        match framer.next_frame() {
            FrameDecision::Frame(body) => {
                assert_eq!(body.len(), 8 * 4 + 3);
            }
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn embedded_nul_split_across_feeds_with_search_hint_still_caught() {
        // Regression guard for the search_from optimization: feed a
        // no-newline prefix (advancing the search hint), then feed a
        // chunk whose NUL sits *before* the eventual newline. The NUL
        // must still be reported as malformed even though scanning
        // resumes from the cached hint.
        let mut framer = LineFramer::new(1024).reject_embedded_nul();
        framer.feed(b"abc");
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
        framer.feed(b"de\0fg\n");
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
        framer.feed(b"abcd"); // exactly at cap, no newline yet
        assert_eq!(framer.next_frame(), FrameDecision::NeedMore);
        framer.feed(b"efgh"); // pushes past cap
        let before = framer.buffered();
        assert!(before <= 5, "buffer must not grow past cap+1");
        assert_eq!(framer.next_frame(), FrameDecision::Full);
        // Flooding more bytes does not grow the buffer or change the verdict.
        for _ in 0..100 {
            framer.feed(b"zzzzzzzzzz");
        }
        assert!(framer.buffered() <= 5);
        assert_eq!(framer.next_frame(), FrameDecision::Full);
    }
}
