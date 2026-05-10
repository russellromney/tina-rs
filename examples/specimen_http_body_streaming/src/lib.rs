//! Tina-vs-Tokio: a large response body served chunk-by-chunk to a
//! deliberately slow client. The shared client reads small slices
//! with a pause between each read so the server has to keep its
//! body queue bounded. Each side reports the body wall-clock
//! duration; the Tina side also reports its `BodyMetrics`
//! high-water — the number of body bytes resident in the connection
//! at peak.
//!
//! The point is *feel*: how big does the in-flight body get, and
//! is that visible to you when something goes wrong?
//!
//! - Tokio: `axum::body::Body::from(big_vec)`. Shorter source.
//! - Tina: `HttpResponse::with_stream(...)` + a chunk-source
//!   isolate. Each pull is named.
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub bytes_received: usize,
    pub status_ok: bool,
    pub wall_clock_ms: u128,
    pub exit_clean: bool,
    /// Tina-only: peak body bytes resident in the connection
    /// isolate at any point. `None` for the Tokio side, which has
    /// no equivalent — its body is `Body::from(big_vec)`, fully
    /// resident.
    pub tina_response_high_water: Option<usize>,
}

/// Connects to `addr`, requests `/big`, and reads the response body
/// in 1 KiB slices with a 2 ms pause between each read.
///
/// Returns `(bytes_read, status_was_200, wall_ms)`. The pause makes
/// the kernel's send buffer back up against the server, which is
/// the pressure both implementations have to deal with.
pub fn slow_reader_client(addr: SocketAddr) -> (usize, bool, u128) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .expect("connect to streaming server");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    stream
        .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .expect("write request");
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
            if let Some(idx) = head_buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
            {
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
