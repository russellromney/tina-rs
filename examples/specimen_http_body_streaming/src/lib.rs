//! Tina-vs-Tokio: a large response body served chunk-by-chunk to a
//! slow client. The shared client reads small slices with a pause
//! between each read so the server has to keep its body queue
//! bounded. Each side reports body wall-clock duration; the Tina
//! side also reports `BodyMetrics::response_body_high_water` — the
//! peak body bytes resident in the connection.
//!
//! The Tina side also exposes a `/big-chunked` route that uses
//! `Transfer-Encoding: chunked` instead of `Content-Length`. The
//! shared client hits both `/big` (known length) and
//! `/big-chunked` (unknown length) so the wire shape of each path
//! is exercised by the smoke run, not just by the unit tests.
//! The Tokio side has only `/big`; chunked is exercised on the
//! Tina side alone.
//!
//! - Tokio: `axum::body::Body::from(big_vec)`. Shorter source.
//! - Tina: `HttpResponse::stream_known_length(...)` /
//!   `HttpResponse::stream_chunked(...)` over `IterBodySource`.
//!   Each pull is named.
//!
//! Read [`tokio_impl`] and [`tina_impl`] top-to-bottom; the README
//! compares the two shapes.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub mod tina_impl;
pub mod tokio_impl;

/// Total response body the server is asked to produce, in bytes.
pub const RESPONSE_BODY_BYTES: usize = 256 * 1024;
/// Per-chunk size on the wire (and per-chunk produced by the
/// streaming source on the Tina side).
pub const CHUNK_BYTES: usize = 4 * 1024;
/// Per-read size on the *client* side. Smaller than `CHUNK_BYTES`
/// so the client deliberately reads less than a server chunk at a
/// time.
pub const CLIENT_READ_BYTES: usize = 1024;
/// Pause the client takes between reads. Forces the server's
/// outbound buffer to flush slowly.
pub const CLIENT_INTER_READ_PAUSE: Duration = Duration::from_millis(2);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub bytes_received: usize,
    pub status_ok: bool,
    pub wall_clock_ms: u128,
    pub exit_clean: bool,
    /// Tokio side only. Lower bound on bytes the body holder
    /// allocates: the `Vec<u8>` we hand to `Body::from(...)` is
    /// `RESPONSE_BODY_BYTES` long and must live until the response
    /// finishes streaming. Hyper internally also queues some bytes
    /// for the writer task; we do not measure those. The number
    /// reported here is "at least this much body is resident".
    pub tokio_response_alloc_floor: Option<usize>,
    /// Tina side only. Peak body bytes resident in the connection
    /// isolate at any time across *all* requests served, observed
    /// via `BodyMetrics`. There is no equivalent reading on the
    /// Tokio side because hyper's internal body queue is not
    /// exposed.
    pub tina_response_high_water: Option<usize>,
    /// Tina side only. Wire bytes received against `/big-chunked`,
    /// after decoding `Transfer-Encoding: chunked`. `None` when the
    /// side does not expose a chunked route (Tokio).
    pub tina_chunked_decoded_bytes: Option<usize>,
    /// Tina side only. Wire bytes received against `/big-chunked`,
    /// before decoding (raw chunked-framed bytes). Always larger
    /// than `tina_chunked_decoded_bytes` by the framing overhead.
    pub tina_chunked_wire_bytes: Option<usize>,
    /// Tina side only. Capacity discovery line for the fixed
    /// response-body weighted surface, showing measured high water.
    pub tina_capacity_discovery_line: Option<String>,
}

/// Connects to `addr`, requests `path`, and reads the response in
/// 1 KiB slices with a 2 ms pause between each read.
///
/// Returns `(body_bytes_received, status_was_200, wall_ms)`. The
/// pause makes the kernel's send buffer back up against the
/// server, which is the pressure both implementations have to
/// deal with. `body_bytes_received` is the raw bytes after the
/// CRLFCRLF separator — for chunked responses these are the
/// framed bytes, not the decoded content.
pub fn slow_reader_client(addr: SocketAddr, path: &str) -> (usize, bool, u128) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .expect("connect to streaming server");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("write request");
    stream.flush().expect("flush request");

    let start = Instant::now();
    let mut bytes_total = 0usize;
    let mut head_done = false;
    let mut head_buf: Vec<u8> = Vec::with_capacity(512);
    let mut status_ok = false;

    let mut buf = [0u8; CLIENT_READ_BYTES];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if !head_done {
            head_buf.extend_from_slice(&buf[..n]);
            if let Some(idx) = head_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head_slice = &head_buf[..idx];
                let head_text = std::str::from_utf8(head_slice).unwrap_or("");
                status_ok = head_text.starts_with("HTTP/1.1 200");
                let body_start = idx + 4;
                bytes_total += head_buf.len().saturating_sub(body_start);
                head_done = true;
            }
        } else {
            bytes_total += n;
        }
        std::thread::sleep(CLIENT_INTER_READ_PAUSE);
    }

    let wall_ms = start.elapsed().as_millis();
    (bytes_total, status_ok, wall_ms)
}

/// Decodes a `Transfer-Encoding: chunked` body. Chunks come as
/// `<size in hex>\r\n<bytes>\r\n` followed by `0\r\n\r\n`. Returns
/// the concatenated data bytes.
pub fn decode_chunked(mut bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let crlf = bytes
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| "chunked: missing size CRLF".to_string())?;
        let size_str = std::str::from_utf8(&bytes[..crlf])
            .map_err(|_| "chunked: size line is not ASCII".to_string())?;
        let size_str = size_str.split(';').next().unwrap_or("");
        let size = usize::from_str_radix(size_str.trim(), 16)
            .map_err(|e| format!("chunked: bad size {size_str:?}: {e}"))?;
        bytes = &bytes[crlf + 2..];
        if size == 0 {
            return Ok(out);
        }
        if bytes.len() < size + 2 {
            return Err(format!(
                "chunked: truncated chunk (size={size}, remaining={})",
                bytes.len()
            ));
        }
        out.extend_from_slice(&bytes[..size]);
        if &bytes[size..size + 2] != b"\r\n" {
            return Err("chunked: missing CRLF after data".to_string());
        }
        bytes = &bytes[size + 2..];
    }
}
