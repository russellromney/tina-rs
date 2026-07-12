//! Theoretical floor for `http1_close_request` on localhost.
//!
//! A single HTTP/1.1 request/response cycle on the loopback interface costs
//! at minimum: connect → write request → read response → close. Anything an
//! HTTP framework adds (parsing, framing, mailbox routing, async runtime) is
//! pure overhead on top of that. This row measures the floor with a hand-
//! rolled threaded server that just `read`s then `write`s a fixed reply, so
//! `make perf` reports "best case for localhost Rust" alongside the framework
//! rows.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const ITERS: usize = 200;
const WARMUP: usize = 40;
const REPLY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n";

#[test]
fn raw_tcp_localhost_floor_close_request() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind floor server");
    let addr: SocketAddr = listener.local_addr().expect("floor local addr");
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let server = thread::spawn(move || {
        listener
            .set_nonblocking(false)
            .expect("blocking server socket");
        for incoming in listener.incoming() {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            let mut stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Read just enough to find the request head terminator. The
            // perf client always sends "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
            // — at most ~64 bytes — and never sends a body, so a small
            // fixed-size read is enough.
            let mut buf = [0u8; 256];
            let mut read = 0;
            loop {
                match stream.read(&mut buf[read..]) {
                    Ok(0) => break,
                    Ok(n) => {
                        read += n;
                        if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                        if read == buf.len() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = stream.write_all(REPLY);
            let _ = stream.flush();
        }
    });

    // Warmup
    for _ in 0..WARMUP {
        one_request(addr);
    }
    let mut totals_ns: Vec<u64> = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t0 = Instant::now();
        one_request(addr);
        totals_ns.push(t0.elapsed().as_nanos() as u64);
    }
    totals_ns.sort_unstable();
    let p50 = totals_ns[totals_ns.len() / 2];
    let p99 = totals_ns[(totals_ns.len() * 99) / 100];
    let min = *totals_ns.first().unwrap();
    let max = *totals_ns.last().unwrap();
    println!(
        "perf-floor label=raw_tcp_http1_close_request iterations={ITERS} p50_us={} p50_ns={} min_ns={} max_ns={} p99_ns={}",
        p50 / 1000,
        p50,
        min,
        max,
        p99
    );

    // The floor must be below `http1_close_request` in perf row terms — if a
    // framework ever lands below this, the floor is wrong or the framework
    // is breaking semantics (e.g. coalescing across connections).
    assert!(
        p50 < 500_000,
        "raw TCP localhost floor regressed beyond 500us p50: {p50}ns"
    );

    stop_tx.send(()).expect("raw TCP stop receiver is live");
    // Best-effort: kick the listener with a connect so the accept loop
    // notices the stop signal.
    let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
    server.join().expect("raw TCP server joins");
}

fn one_request(addr: SocketAddr) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(1)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .expect("write request");
    stream.flush().expect("flush");
    let mut buf = Vec::with_capacity(REPLY.len());
    let _ = stream.read_to_end(&mut buf);
}
