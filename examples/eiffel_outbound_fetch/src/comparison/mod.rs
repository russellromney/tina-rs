use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread::{self, JoinHandle};

pub(crate) mod tina_impl;
pub(crate) mod tokio_impl;

pub(crate) const FETCH_COUNT: u32 = 4;
pub(crate) const RESPONSE: &[u8] = b"OK\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SideReport {
    pub addr_used: SocketAddr,
    pub successful_fetches: u32,
    pub failed_fetches: u32,
    pub bytes_received: usize,
    pub exit_clean: bool,
}

pub(crate) fn print_report(side: &str, report: &SideReport) {
    println!(
        "side={side} addr={} successful={} failed={} bytes={} exit_clean={}",
        report.addr_used,
        report.successful_fetches,
        report.failed_fetches,
        report.bytes_received,
        report.exit_clean,
    );
}

pub(crate) fn assert_equivalent(tokio: &SideReport, tina: &SideReport) {
    assert_eq!(
        tokio.successful_fetches, FETCH_COUNT,
        "tokio fetched every endpoint"
    );
    assert_eq!(
        tina.successful_fetches, FETCH_COUNT,
        "tina fetched every endpoint"
    );
    assert_eq!(tokio.failed_fetches, 0, "tokio had no failures");
    assert_eq!(tina.failed_fetches, 0, "tina had no failures");
    assert_eq!(
        tokio.bytes_received,
        RESPONSE.len() * FETCH_COUNT as usize,
        "tokio received the full payload from every endpoint"
    );
    assert_eq!(
        tina.bytes_received,
        RESPONSE.len() * FETCH_COUNT as usize,
        "tina received the full payload from every endpoint"
    );
    assert!(tokio.exit_clean, "tokio exited cleanly");
    assert!(tina.exit_clean, "tina exited cleanly");
}

pub(crate) struct TestServer {
    pub addr: SocketAddr,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    pub fn start(expected_connections: u32) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            for _ in 0..expected_connections {
                let (mut stream, _peer) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                // Drain any client-sent bytes so the read side does not block.
                let mut scratch = [0u8; 64];
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(50)));
                let _ = stream.read(&mut scratch);
                let _ = stream.write_all(RESPONSE);
                let _ = stream.flush();
                drop(stream);
            }
        });
        Self {
            addr,
            handle: Some(handle),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Best-effort wake the listener thread if it is still waiting on
        // `accept`. The server is ours so we know the count, but if a side
        // stops early we want the test to not hang.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
