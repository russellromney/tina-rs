use crate::types::ChunkedError;

const MAX_SIZE_LINE_LEN: usize = 64;

#[derive(Debug)]
pub struct ChunkedDecoder {
    max_body_bytes: usize,
    decoded_total: usize,
    state: State,
    size_buf: [u8; MAX_SIZE_LINE_LEN],
    size_len: usize,
    trailer_partial: u8,
    has_trailer_partial: bool,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum State {
    Size,
    Data { remaining: usize },
    DataCrlf,
    Trailers,
    Done,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ChunkedProgress<'a> {
    NeedMore,
    Chunk(&'a [u8]),
    Complete,
    Failed(ChunkedError),
}

enum SizeResult {
    Complete {
        size: usize,
        consumed: usize,
    },
    NeedMore {
        consumed: usize,
    },
    Error {
        error: ChunkedError,
        consumed: usize,
    },
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum FeedAllResult {
    Complete,
    Failed(ChunkedError),
    NeedMore,
}

impl ChunkedDecoder {
    pub fn new(max_body_bytes: usize) -> Self {
        Self {
            max_body_bytes,
            decoded_total: 0,
            state: State::Size,
            size_buf: [0u8; MAX_SIZE_LINE_LEN],
            size_len: 0,
            trailer_partial: 0,
            has_trailer_partial: false,
        }
    }

    pub fn is_complete(&self) -> bool {
        matches!(self.state, State::Done)
    }

    pub fn feed<'a>(&mut self, input: &'a [u8]) -> (ChunkedProgress<'a>, usize) {
        let mut consumed = 0;
        let mut remaining = input;

        loop {
            if remaining.is_empty() && self.state != State::Done {
                return (ChunkedProgress::NeedMore, consumed);
            }

            match self.state {
                State::Size => match self.feed_size(remaining) {
                    SizeResult::Complete { size, consumed: n } => {
                        consumed += n;
                        remaining = &remaining[n..];
                        if size == 0 {
                            self.state = State::Trailers;
                            continue;
                        }
                        if self.decoded_total + size > self.max_body_bytes {
                            return (
                                ChunkedProgress::Failed(ChunkedError::BodyTooLarge),
                                consumed,
                            );
                        }
                        self.state = State::Data { remaining: size };
                        continue;
                    }
                    SizeResult::NeedMore { consumed: n } => {
                        return (ChunkedProgress::NeedMore, consumed + n);
                    }
                    SizeResult::Error { error, consumed: n } => {
                        return (ChunkedProgress::Failed(error), consumed + n);
                    }
                },
                State::Data {
                    remaining: data_remaining,
                } => {
                    let take = data_remaining.min(remaining.len());
                    if take == 0 {
                        return (ChunkedProgress::NeedMore, consumed);
                    }
                    self.decoded_total += take;
                    let chunk = &remaining[..take];
                    let new_remaining = data_remaining - take;
                    consumed += take;
                    if new_remaining == 0 {
                        self.state = State::DataCrlf;
                    } else {
                        self.state = State::Data {
                            remaining: new_remaining,
                        };
                    }
                    return (ChunkedProgress::Chunk(chunk), consumed);
                }
                State::DataCrlf => {
                    if input.len() < 2 {
                        return (ChunkedProgress::NeedMore, consumed);
                    }
                    if &remaining[..2] != b"\r\n" {
                        return (ChunkedProgress::Failed(ChunkedError::MissingCrlf), consumed);
                    }
                    consumed += 2;
                    remaining = &remaining[2..];
                    self.state = State::Size;
                    continue;
                }
                State::Trailers => {
                    if self.has_trailer_partial && self.trailer_partial == b'\r' {
                        if remaining.is_empty() {
                            return (ChunkedProgress::NeedMore, consumed);
                        }
                        if remaining[0] == b'\n' {
                            consumed += 1;
                            self.has_trailer_partial = false;
                            self.state = State::Done;
                            return (ChunkedProgress::Complete, consumed);
                        }
                        self.has_trailer_partial = false;
                        return (
                            ChunkedProgress::Failed(ChunkedError::TrailersNotSupported),
                            consumed,
                        );
                    }

                    if remaining.len() < 2 {
                        if remaining.len() == 1 && remaining[0] == b'\r' {
                            self.trailer_partial = b'\r';
                            self.has_trailer_partial = true;
                        } else if remaining.len() == 1 {
                            return (
                                ChunkedProgress::Failed(ChunkedError::TrailersNotSupported),
                                consumed,
                            );
                        }
                        return (ChunkedProgress::NeedMore, consumed + remaining.len());
                    }

                    if &remaining[..2] == b"\r\n" {
                        consumed += 2;
                        self.state = State::Done;
                        return (ChunkedProgress::Complete, consumed);
                    }

                    return (
                        ChunkedProgress::Failed(ChunkedError::TrailersNotSupported),
                        consumed,
                    );
                }
                State::Done => {
                    return (ChunkedProgress::Complete, consumed);
                }
            }
        }
    }

    /// Convenience: repeatedly feed until the decoder yields
    /// something other than `Chunk`. Appends every `Chunk` into
    /// `accumulator`. Returns the terminal progress and the total
    /// bytes consumed from `input`.
    pub fn feed_all(&mut self, input: &[u8], accumulator: &mut Vec<u8>) -> (FeedAllResult, usize) {
        let mut total_consumed = 0;
        loop {
            let (progress, consumed) = self.feed(&input[total_consumed..]);
            total_consumed += consumed;
            match progress {
                ChunkedProgress::Chunk(data) => accumulator.extend_from_slice(data),
                ChunkedProgress::Complete => return (FeedAllResult::Complete, total_consumed),
                ChunkedProgress::Failed(e) => return (FeedAllResult::Failed(e), total_consumed),
                ChunkedProgress::NeedMore => return (FeedAllResult::NeedMore, total_consumed),
            }
        }
    }

    fn feed_size(&mut self, input: &[u8]) -> SizeResult {
        if self.size_len > 0 && self.size_buf[self.size_len - 1] == b'\r' {
            if input.is_empty() {
                return SizeResult::NeedMore { consumed: 0 };
            }
            if input[0] == b'\n' {
                let line = &self.size_buf[..self.size_len - 1];
                match parse_chunk_size_line(line) {
                    Ok(size) => {
                        self.size_len = 0;
                        return SizeResult::Complete { size, consumed: 1 };
                    }
                    Err(e) => {
                        self.size_len = 0;
                        return SizeResult::Error {
                            error: e,
                            consumed: 1,
                        };
                    }
                }
            } else {
                self.size_len = 0;
                return SizeResult::Error {
                    error: ChunkedError::MissingCrlf,
                    consumed: 0,
                };
            }
        }

        // Scan input for \r\n.
        for i in 0..input.len().saturating_sub(1) {
            if input[i] == b'\r' && input[i + 1] == b'\n' {
                let line = if self.size_len == 0 {
                    &input[..i]
                } else {
                    let take = i.min(self.size_buf.len() - self.size_len);
                    self.size_buf[self.size_len..self.size_len + take]
                        .copy_from_slice(&input[..take]);
                    self.size_len += take;
                    &self.size_buf[..self.size_len]
                };
                match parse_chunk_size_line(line) {
                    Ok(size) => {
                        self.size_len = 0;
                        return SizeResult::Complete {
                            size,
                            consumed: i + 2,
                        };
                    }
                    Err(e) => {
                        self.size_len = 0;
                        return SizeResult::Error {
                            error: e,
                            consumed: i + 2,
                        };
                    }
                }
            }
        }

        // No \r\n found. Save what fits.
        let save_len = input.len().min(self.size_buf.len() - self.size_len);
        self.size_buf[self.size_len..self.size_len + save_len].copy_from_slice(&input[..save_len]);
        self.size_len += save_len;

        if self.size_len >= self.size_buf.len() {
            self.size_len = 0;
            return SizeResult::Error {
                error: ChunkedError::BadChunkSize,
                consumed: input.len(),
            };
        }

        SizeResult::NeedMore {
            consumed: input.len(),
        }
    }
}

fn parse_chunk_size_line(line: &[u8]) -> Result<usize, ChunkedError> {
    if line.is_empty() {
        return Err(ChunkedError::BadChunkSize);
    }
    let size_part = line
        .iter()
        .position(|&b| b == b';')
        .map_or(line, |pos| &line[..pos]);
    let trimmed = {
        let start = size_part
            .iter()
            .position(|&b| b != b' ' && b != b'\t')
            .unwrap_or(size_part.len());
        let end = size_part
            .iter()
            .rposition(|&b| b != b' ' && b != b'\t')
            .map_or(size_part.len(), |i| i + 1);
        &size_part[start..end]
    };
    if trimmed.is_empty() {
        return Err(ChunkedError::BadChunkSize);
    }
    for &b in trimmed {
        if !b.is_ascii_hexdigit() {
            return Err(ChunkedError::BadChunkSize);
        }
    }
    let s = std::str::from_utf8(trimmed).map_err(|_| ChunkedError::BadChunkSize)?;
    usize::from_str_radix(s, 16).map_err(|_| ChunkedError::BadChunkSize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_complete_chunk() {
        let mut dec = ChunkedDecoder::new(1024);
        let input = b"5\r\nhello\r\n0\r\n\r\n";
        let mut out = Vec::new();
        let (progress, consumed) = dec.feed_all(input, &mut out);
        assert!(matches!(progress, FeedAllResult::Complete));
        assert_eq!(consumed, input.len());
        assert_eq!(&out, b"hello");
    }

    #[test]
    fn multiple_chunks() {
        let mut dec = ChunkedDecoder::new(1024);
        let input = b"5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n";
        let mut out = Vec::new();
        let (progress, consumed) = dec.feed_all(input, &mut out);
        assert!(matches!(progress, FeedAllResult::Complete));
        assert_eq!(consumed, input.len());
        assert_eq!(&out, b"helloworld");
    }

    #[test]
    fn chunk_extensions_ignored() {
        let mut dec = ChunkedDecoder::new(1024);
        let input = b"5;ext=foo\r\nhello\r\n0\r\n\r\n";
        let mut out = Vec::new();
        let (progress, consumed) = dec.feed_all(input, &mut out);
        assert!(matches!(progress, FeedAllResult::Complete));
        assert_eq!(consumed, input.len());
        assert_eq!(&out, b"hello");
    }

    #[test]
    fn empty_chunked_body() {
        let mut dec = ChunkedDecoder::new(1024);
        let input = b"0\r\n\r\n";
        let mut out = Vec::new();
        let (progress, consumed) = dec.feed_all(input, &mut out);
        assert!(matches!(progress, FeedAllResult::Complete));
        assert_eq!(consumed, input.len());
        assert!(out.is_empty());
    }

    #[test]
    fn rejects_bad_hex() {
        let mut dec = ChunkedDecoder::new(1024);
        let (progress, _) = dec.feed(b"ZZ\r\n");
        assert!(matches!(
            progress,
            ChunkedProgress::Failed(ChunkedError::BadChunkSize)
        ));
    }

    #[test]
    fn rejects_missing_data_crlf() {
        let mut dec = ChunkedDecoder::new(1024);
        let mut out = Vec::new();
        let (p1, c1) = dec.feed(b"5\r\nhello");
        assert!(matches!(p1, ChunkedProgress::Chunk(_)));
        assert_eq!(c1, 8);
        out.extend_from_slice(match p1 {
            ChunkedProgress::Chunk(d) => d,
            _ => panic!(),
        });
        let (p2, _) = dec.feed(b"X\r\n0\r\n\r\n");
        assert!(matches!(
            p2,
            ChunkedProgress::Failed(ChunkedError::MissingCrlf)
        ));
    }

    #[test]
    fn rejects_body_too_large() {
        let mut dec = ChunkedDecoder::new(4);
        let (progress, _) = dec.feed(b"5\r\nhello\r\n0\r\n\r\n");
        assert!(matches!(
            progress,
            ChunkedProgress::Failed(ChunkedError::BodyTooLarge)
        ));
    }

    #[test]
    fn partial_size_line_needs_more() {
        let mut dec = ChunkedDecoder::new(1024);
        let (progress, consumed) = dec.feed(b"a");
        assert!(matches!(progress, ChunkedProgress::NeedMore));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn partial_data_across_feeds() {
        let mut dec = ChunkedDecoder::new(1024);
        let mut out = Vec::new();
        let (p1, c1) = dec.feed(b"5\r\nhel");
        assert!(matches!(p1, ChunkedProgress::Chunk(_)));
        assert_eq!(c1, 6);
        out.extend_from_slice(match p1 {
            ChunkedProgress::Chunk(d) => d,
            _ => panic!(),
        });
        let (p2, c2) = dec.feed(b"lo\r\n0\r\n\r\n");
        assert!(matches!(p2, ChunkedProgress::Chunk(_)));
        assert_eq!(c2, 2);
        out.extend_from_slice(match p2 {
            ChunkedProgress::Chunk(d) => d,
            _ => panic!(),
        });
        let (p3, c3) = dec.feed(b"\r\n0\r\n\r\n");
        assert!(matches!(p3, ChunkedProgress::Complete));
        assert_eq!(c3, 7);
        assert_eq!(&out, b"hello");
    }

    #[test]
    fn feed_all_consumes_everything() {
        let mut dec = ChunkedDecoder::new(1024);
        let input = b"5\r\nhello\r\n0\r\n\r\n";
        let mut out = Vec::new();
        let (progress, consumed) = dec.feed_all(input, &mut out);
        assert!(matches!(progress, FeedAllResult::Complete));
        assert_eq!(consumed, input.len());
    }

    #[test]
    fn split_crlf_after_data() {
        let mut dec = ChunkedDecoder::new(1024);
        let mut buf = b"5\r\nhello\r".to_vec();
        let mut out: Vec<u8> = Vec::new();
        let (p1, c1) = dec.feed_all(&buf, &mut out);
        assert!(matches!(p1, FeedAllResult::NeedMore));
        assert_eq!(c1, 8);
        assert_eq!(&out, b"hello");
        buf.drain(..c1);
        buf.extend_from_slice(b"\n0\r\n\r\n");
        let (p2, c2) = dec.feed_all(&buf, &mut out);
        assert!(matches!(p2, FeedAllResult::Complete));
        assert_eq!(c2, 7);
    }

    #[test]
    fn rejects_trailers() {
        let mut dec = ChunkedDecoder::new(1024);
        let (progress, _) = dec.feed(b"0\r\nX-Trailer: value\r\n\r\n");
        assert!(matches!(
            progress,
            ChunkedProgress::Failed(ChunkedError::TrailersNotSupported)
        ));
    }

    #[test]
    fn split_trailers_terminator() {
        let mut dec = ChunkedDecoder::new(1024);
        let (p1, c1) = dec.feed(b"0\r\n");
        assert!(matches!(p1, ChunkedProgress::NeedMore));
        assert_eq!(c1, 3);
        let (p2, c2) = dec.feed(b"\r\n");
        assert!(matches!(p2, ChunkedProgress::Complete));
        assert_eq!(c2, 2);
    }

    #[test]
    fn split_trailers_terminator_across_single_byte() {
        let mut dec = ChunkedDecoder::new(1024);
        let (p1, c1) = dec.feed(b"0\r\n\r");
        assert!(matches!(p1, ChunkedProgress::NeedMore));
        assert_eq!(c1, 4);
        let (p2, c2) = dec.feed(b"\n");
        assert!(matches!(p2, ChunkedProgress::Complete));
        assert_eq!(c2, 1);
    }

    #[test]
    fn max_size_line_rejected() {
        let mut dec = ChunkedDecoder::new(1024);
        let huge_size = "f".repeat(MAX_SIZE_LINE_LEN + 1);
        let input = format!("{huge_size}\r\n");
        let (progress, _) = dec.feed(input.as_bytes());
        assert!(matches!(
            progress,
            ChunkedProgress::Failed(ChunkedError::BadChunkSize)
        ));
    }
}
