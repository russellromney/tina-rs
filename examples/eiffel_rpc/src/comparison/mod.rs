use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use tina_rpc::{Frame, FrameError, FrameKind, FrameLimits, LENGTH_PREFIX_SIZE, encode, parse_length_prefix, decode_body};

mod tina_impl;
mod tokio_impl;

/// Comparison parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComparisonConfig {
    /// Number of requests the client sends in one batch.
    pub burst: usize,
}

impl Default for ComparisonConfig {
    fn default() -> Self {
        Self { burst: 4 }
    }
}

/// Per-side observable outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SideReport {
    /// Number of wire `Reply` frames the client received.
    pub ok: usize,
    /// Number of wire `Error(Full)` frames the client received.
    pub full: usize,
    /// Anything else: other error codes, decode failures, premature
    /// connection close, or unaccounted-for replies (the client tags
    /// missing frames as `other` rather than letting totals shrink
    /// silently).
    pub other: usize,
}

impl SideReport {
    /// Pins the contract for a Tina-side run: every request results in
    /// either a `Reply` (the one that grabbed the in-flight slot) or
    /// an `Error(Full)` (every over-cap request). With
    /// `Connection::max_in_flight = 1`, exactly one `ok` and the rest
    /// `full`. A regression that breaks this is a Rock-2/Rock-3
    /// regression.
    pub fn assert_tina_contract(&self, burst: usize) {
        assert_eq!(self.ok, 1, "expected ok=1 for tina; got {self:?}");
        assert_eq!(self.full, burst - 1, "expected full=burst-1 for tina; got {self:?}");
        assert_eq!(self.other, 0, "expected other=0 for tina; got {self:?}");
    }

    /// Pins the contract for a Tokio-side run: unbounded queue means
    /// every request gets a `Reply`.
    pub fn assert_tokio_contract(&self, burst: usize) {
        assert_eq!(self.ok, burst, "expected ok=burst for tokio; got {self:?}");
        assert_eq!(self.full, 0, "expected full=0 for tokio; got {self:?}");
        assert_eq!(self.other, 0, "expected other=0 for tokio; got {self:?}");
    }
}

pub fn run_tina_side(burst: usize) -> SideReport {
    tina_impl::run(ComparisonConfig { burst })
}

pub fn run_tokio_side(burst: usize) -> SideReport {
    tokio_impl::run(ComparisonConfig { burst })
}

/// Encodes `burst` request frames concatenated into one buffer with
/// monotonically increasing wire request ids starting at 1.
pub(crate) fn encode_burst(burst: usize) -> Vec<u8> {
    let limits = FrameLimits::default();
    let mut bytes = Vec::new();
    for id in 1..=burst as u64 {
        let frame = Frame::request(id, "echo", "ping", b"hi".to_vec());
        bytes.extend(encode(&frame, &limits).expect("encode request frame"));
    }
    bytes
}

/// Sends all `burst` request frames in one TCP write and reads `burst`
/// response frames back. Tallies the response shapes.
pub(crate) fn drive_client(addr: SocketAddr, burst: usize) -> SideReport {
    let mut stream = TcpStream::connect(addr).expect("client connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");
    let request_bytes = encode_burst(burst);
    stream
        .write_all(&request_bytes)
        .expect("client writes burst");

    let mut report = SideReport {
        ok: 0,
        full: 0,
        other: 0,
    };
    let limits = FrameLimits::default();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while report.ok + report.full + report.other < burst {
        match stream.read(&mut chunk) {
            Ok(0) => {
                // Premature EOF: any unreceived replies count as `other`
                // so a regression that drops frames cannot silently
                // shrink the totals.
                let missing = burst - (report.ok + report.full + report.other);
                report.other += missing;
                return report;
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => {
                // Read timeout or IO error: same accounting as EOF.
                let missing = burst - (report.ok + report.full + report.other);
                report.other += missing;
                return report;
            }
        }
        // Pull as many complete frames as we have.
        loop {
            if buf.len() < LENGTH_PREFIX_SIZE {
                break;
            }
            let mut prefix = [0u8; LENGTH_PREFIX_SIZE];
            prefix.copy_from_slice(&buf[..LENGTH_PREFIX_SIZE]);
            let body_len = match parse_length_prefix(prefix, &limits) {
                Ok(len) => len,
                Err(_) => {
                    report.other += 1;
                    return report;
                }
            };
            let total = LENGTH_PREFIX_SIZE + body_len;
            if buf.len() < total {
                break;
            }
            let body = buf[LENGTH_PREFIX_SIZE..total].to_vec();
            buf.drain(..total);
            match decode_body(&body) {
                Ok(frame) => match (frame.kind, frame.error) {
                    (FrameKind::Reply, _) => report.ok += 1,
                    (FrameKind::Error, Some(FrameError::Full)) => report.full += 1,
                    _ => report.other += 1,
                },
                Err(_) => report.other += 1,
            }
        }
    }
    report
}
