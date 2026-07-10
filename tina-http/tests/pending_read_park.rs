//! Pending-TCP-read proof for explicit-step I/O.
//!
//! A connected peer that sends nothing leaves the server's `recv` pending. The
//! worker observes that read by repeatedly calling the explicit runtime
//! `step()` after a bounded idle sleep. This intentionally accepts the explicit-step
//! style latency/CPU tradeoff instead of using an out-of-band readiness wake.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use common::TestHarness;

#[test]
fn pending_tcp_read_is_serviced_by_bounded_explicit_repoll() {
    let harness = TestHarness::start();
    let addr = harness.addr;

    // Open a connection and send nothing: the server accepts and arms a recv
    // that now waits for request bytes.
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");

    // Let the accept settle so the worker is idling with a pending recv.
    std::thread::sleep(Duration::from_millis(50));

    // While the peer is silent the worker re-polls explicitly. The pending
    // read is not serviced by a hidden cross-thread I/O wake channel.
    let before = harness.runtime_handle().park_wakeups();
    std::thread::sleep(Duration::from_millis(80));
    let idle_wakeups = harness.runtime_handle().park_wakeups() - before;
    assert!(
        idle_wakeups > 0,
        "worker did not perform bounded explicit re-polls while a read was pending"
    );

    // Now send the request. The worker should serve it within the bounded
    // re-poll policy, not wait for the long socket read timeout.
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
        "pending read serviced too slowly ({elapsed:?}); bounded re-poll is not advancing I/O"
    );

    harness.shutdown();
}
