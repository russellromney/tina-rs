//! Pending-TCP-read proof for the readiness-driven worker park.
//!
//! A connected peer that sends nothing leaves the server's `recv` pending. The
//! worker must park on the kernel (zero wakeups while no bytes arrive) and then
//! service the read promptly the instant bytes do arrive — not on a poll timer.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use common::TestHarness;

#[test]
fn pending_tcp_read_parks_with_zero_wakeups_then_wakes_on_bytes() {
    let harness = TestHarness::start();
    let addr = harness.addr;

    // Open a connection and send nothing: the server accepts and arms a recv
    // that now waits for request bytes.
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");

    // Let the accept settle so the worker is parked on the pending recv.
    std::thread::sleep(Duration::from_millis(50));

    // While the peer is silent the worker must be asleep on the kernel: ~0
    // wakeups across the window. A timer-poll park would wake ~250 times here.
    let before = harness.runtime_handle().park_wakeups();
    std::thread::sleep(Duration::from_millis(250));
    let idle_wakeups = harness.runtime_handle().park_wakeups() - before;
    assert!(
        idle_wakeups <= 3,
        "worker woke {idle_wakeups} times with a pending read and no bytes; the park is not blocking on the kernel"
    );

    // Now send the request. The worker must wake at kernel-readiness latency and
    // serve the response promptly (well under the 2s read timeout).
    let started = Instant::now();
    stream
        .write_all(b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("write request");
    stream.flush().expect("flush");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let elapsed = started.elapsed();

    let text = std::str::from_utf8(&response).expect("utf8");
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "expected 200, got: {text:?}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "pending read serviced slowly ({elapsed:?}); not waking on kernel readiness"
    );

    harness.shutdown();
}
