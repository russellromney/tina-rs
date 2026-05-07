//! Tina-vs-Tokio: an outbound *client* that fetches a fixed payload
//! from a real loopback TCP server N times. Both sides connect to
//! the same `TestServer`; the comparison is the client shape.
//!
//! Read [`tokio_impl`] and [`tina_impl`] top-to-bottom; the README
//! compares feel.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub mod tina_impl;
pub mod tokio_impl;

pub const FETCH_COUNT: u32 = 4;
pub const RESPONSE: &[u8] = b"OK\n";
/// Per-`tcp_read` budget for the read-to-EOF loop helper.
pub const READ_CHUNK: usize = 64;
/// Cap for the read-to-EOF accumulated buffer.
pub const RESPONSE_MAX: usize = 1024;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub successful_fetches: u32,
    pub failed_fetches: u32,
    pub bytes_received: usize,
    pub exit_clean: bool,
}

/// Tiny single-threaded `std::net` server used as the test target by
/// both sides. Accepts `expected_connections` connections, drains
/// any client-sent bytes, writes [`RESPONSE`], closes the stream.
/// Shared between sides because it doesn't constrain client
/// implementation — both sides connect to it however they want.
pub struct TestServer {
    pub addr: SocketAddr,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    pub fn start(expected_connections: u32) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let handle = thread::spawn(move || {
            for _ in 0..expected_connections {
                let (mut stream, _peer) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let mut scratch = [0u8; 64];
                let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
                let _ = stream.read(&mut scratch);
                let _ = stream.write_all(RESPONSE);
                let _ = stream.flush();
                drop(stream);
            }
        });
        Ok(Self {
            addr,
            handle: Some(handle),
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Best-effort wake the listener thread if it's still on `accept`.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
