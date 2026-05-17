//! Reusable bad-peer TCP/HTTP clients.
//!
//! Each scenario opens a TCP connection to a `SocketAddr`, does the
//! bad thing, and returns a typed [`BadPeerOutcome`]. The outcome
//! records what the client did and what the server appeared to do —
//! never a log scrape.
//!
//! Scenarios are intentionally narrow. Forming a full HTTP request
//! belongs to the specimen — the bad-peer client only twists the
//! transport. Where an HTTP request is needed, the caller passes the
//! raw bytes. That keeps the harness protocol-agnostic.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

/// Which bad-peer story to run. Each variant is documented at the
/// constructor below.
#[derive(Debug, Clone)]
pub enum BadPeerScenario {
    /// Open, write `request`, shut down our write half, then drain the
    /// server's reply until close or `drain_for` elapses.
    HalfClose {
        request: Vec<u8>,
        drain_for: Duration,
    },
    /// Open and immediately drop the stream. No bytes sent. Most
    /// kernels send a FIN here; servers must treat this as a normal
    /// peer close and not as a request.
    ResetImmediately,
    /// Open, write `request` (often partial headers), then send the
    /// remaining bytes one at a time with `byte_delay` between bytes.
    /// Useful as a slowloris stand-in. Returns once all bytes are sent
    /// or `give_up_after` elapses.
    Slowloris {
        prelude: Vec<u8>,
        trailer: Vec<u8>,
        byte_delay: Duration,
        give_up_after: Duration,
    },
    /// Open, write `request`, then read nothing for `stall_for` while
    /// the server tries to send a reply. Drains briefly after.
    StalledReader {
        request: Vec<u8>,
        stall_for: Duration,
    },
    /// Open and start the bytes of `request`, then stop writing
    /// half-way for `stall_for` before continuing (or giving up).
    StalledWriter {
        first_chunk: Vec<u8>,
        rest: Vec<u8>,
        stall_for: Duration,
    },
    /// Open, write `bytes`, drain briefly. Use to send garbage HTTP
    /// frames, oversize bodies, or other malformed transport.
    MalformedFrame { bytes: Vec<u8>, drain_for: Duration },
    /// Open and close `count` times back-to-back with no traffic.
    ReconnectStorm { count: usize },
}

/// One observation of a bad-peer attempt.
///
/// `server_closed` is `true` when the next read returned 0 bytes
/// (graceful EOF). `peer_reset` is best-effort: it is `true` when we
/// observed an error consistent with the peer resetting the connection
/// (`ECONNRESET`, `EPIPE`, `BrokenPipe`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadPeerOutcome {
    /// Stable label for the run.
    pub label: &'static str,
    /// We actually completed the TCP connect.
    pub connected: bool,
    /// Bytes we wrote (best-effort tally; may include the partial
    /// write that triggered an error).
    pub bytes_sent: usize,
    /// Bytes we read from the server before close/timeout.
    pub bytes_read: usize,
    /// First few bytes of the server reply, decoded as UTF-8 with
    /// `\u{FFFD}` substitution; truncated to 128 bytes for the report.
    pub reply_prefix: String,
    /// The server closed gracefully during drain.
    pub server_closed: bool,
    /// The server (or our local stack) signalled a reset during drain.
    pub peer_reset: bool,
    /// Wall-clock elapsed for the scenario.
    pub elapsed_ms: u64,
    /// First error we observed during the scenario, as text. Always
    /// pair with `peer_reset`/`server_closed` to interpret.
    pub error: Option<String>,
}

impl BadPeerOutcome {
    pub fn summary_line(&self) -> String {
        format!(
            "bad_peer label={} connected={} bytes_sent={} bytes_read={} server_closed={} peer_reset={} elapsed_ms={} error={}",
            self.label,
            self.connected,
            self.bytes_sent,
            self.bytes_read,
            self.server_closed,
            self.peer_reset,
            self.elapsed_ms,
            self.error.as_deref().unwrap_or("none"),
        )
    }
}

/// Run one bad-peer scenario against `addr`. Returns a typed outcome.
pub fn run(
    label: &'static str,
    addr: SocketAddr,
    connect_timeout: Duration,
    scenario: BadPeerScenario,
) -> BadPeerOutcome {
    let started = Instant::now();
    let stream = match TcpStream::connect_timeout(&addr, connect_timeout) {
        Ok(s) => s,
        Err(err) => {
            return BadPeerOutcome {
                label,
                connected: false,
                bytes_sent: 0,
                bytes_read: 0,
                reply_prefix: String::new(),
                server_closed: false,
                peer_reset: false,
                elapsed_ms: started.elapsed().as_millis() as u64,
                error: Some(format!("connect: {err}")),
            };
        }
    };
    // Per-call deadlines so a misbehaving server never hangs the harness.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    match scenario {
        BadPeerScenario::HalfClose { request, drain_for } => {
            run_half_close(label, started, stream, request, drain_for)
        }
        BadPeerScenario::ResetImmediately => run_reset(label, started, stream),
        BadPeerScenario::Slowloris {
            prelude,
            trailer,
            byte_delay,
            give_up_after,
        } => run_slowloris(
            label,
            started,
            stream,
            prelude,
            trailer,
            byte_delay,
            give_up_after,
        ),
        BadPeerScenario::StalledReader { request, stall_for } => {
            run_stalled_reader(label, started, stream, request, stall_for)
        }
        BadPeerScenario::StalledWriter {
            first_chunk,
            rest,
            stall_for,
        } => run_stalled_writer(label, started, stream, first_chunk, rest, stall_for),
        BadPeerScenario::MalformedFrame { bytes, drain_for } => {
            run_malformed(label, started, stream, bytes, drain_for)
        }
        BadPeerScenario::ReconnectStorm { count } => {
            // The stream we already opened counts as one connection; spin
            // `count - 1` more.
            drop(stream);
            run_storm(label, started, addr, connect_timeout, count)
        }
    }
}

fn run_half_close(
    label: &'static str,
    started: Instant,
    mut stream: TcpStream,
    request: Vec<u8>,
    drain_for: Duration,
) -> BadPeerOutcome {
    let mut outcome = base_outcome(label, started);
    outcome.connected = true;
    match stream.write_all(&request) {
        Ok(()) => outcome.bytes_sent = request.len(),
        Err(err) => return finish_with_error(outcome, started, err),
    }
    if let Err(err) = stream.flush() {
        return finish_with_error(outcome, started, err);
    }
    // Close write half. The server should drain the request, reply, and
    // close.
    if let Err(err) = stream.shutdown(Shutdown::Write) {
        outcome.error = Some(format!("shutdown_write: {err}"));
    }
    drain_into(&mut stream, drain_for, &mut outcome);
    finish(outcome, started)
}

fn run_reset(label: &'static str, started: Instant, stream: TcpStream) -> BadPeerOutcome {
    let mut outcome = base_outcome(label, started);
    outcome.connected = true;
    // Plain drop sends a FIN. We do not try SO_LINGER(0) because
    // `TcpStream::set_linger` is still unstable; raw socket access
    // would pull libc in for one bad-peer story. A graceful close is
    // enough to prove the server does not treat empty traffic as a
    // request — the typed scenario name is the contract.
    drop(stream);
    finish(outcome, started)
}

fn run_slowloris(
    label: &'static str,
    started: Instant,
    mut stream: TcpStream,
    prelude: Vec<u8>,
    trailer: Vec<u8>,
    byte_delay: Duration,
    give_up_after: Duration,
) -> BadPeerOutcome {
    let mut outcome = base_outcome(label, started);
    outcome.connected = true;
    if let Err(err) = stream.write_all(&prelude) {
        return finish_with_error(outcome, started, err);
    }
    outcome.bytes_sent = prelude.len();
    let _ = stream.flush();
    let deadline = started + give_up_after;
    for byte in trailer {
        if Instant::now() >= deadline {
            outcome.error = Some("slowloris: give_up_after elapsed".to_string());
            break;
        }
        if let Err(err) = stream.write_all(&[byte]) {
            outcome.error = Some(format!("slowloris write: {err}"));
            outcome.peer_reset = is_reset_error(&err);
            break;
        }
        outcome.bytes_sent += 1;
        let _ = stream.flush();
        thread::sleep(byte_delay);
    }
    // Drain whatever the server already wrote (often a 400 Bad Request).
    drain_into(&mut stream, Duration::from_millis(200), &mut outcome);
    finish(outcome, started)
}

fn run_stalled_reader(
    label: &'static str,
    started: Instant,
    mut stream: TcpStream,
    request: Vec<u8>,
    stall_for: Duration,
) -> BadPeerOutcome {
    let mut outcome = base_outcome(label, started);
    outcome.connected = true;
    if let Err(err) = stream.write_all(&request) {
        return finish_with_error(outcome, started, err);
    }
    outcome.bytes_sent = request.len();
    let _ = stream.flush();
    // Sleep without reading. The server's send buffer will fill if the
    // reply is big enough; otherwise this scenario asserts the server
    // does not hang waiting for us to read.
    thread::sleep(stall_for);
    drain_into(&mut stream, Duration::from_millis(500), &mut outcome);
    finish(outcome, started)
}

fn run_stalled_writer(
    label: &'static str,
    started: Instant,
    mut stream: TcpStream,
    first_chunk: Vec<u8>,
    rest: Vec<u8>,
    stall_for: Duration,
) -> BadPeerOutcome {
    let mut outcome = base_outcome(label, started);
    outcome.connected = true;
    if let Err(err) = stream.write_all(&first_chunk) {
        return finish_with_error(outcome, started, err);
    }
    outcome.bytes_sent = first_chunk.len();
    let _ = stream.flush();
    thread::sleep(stall_for);
    match stream.write_all(&rest) {
        Ok(()) => outcome.bytes_sent += rest.len(),
        Err(err) => {
            outcome.peer_reset = is_reset_error(&err);
            outcome.error = Some(format!("stalled_writer rest: {err}"));
        }
    }
    let _ = stream.flush();
    drain_into(&mut stream, Duration::from_millis(500), &mut outcome);
    finish(outcome, started)
}

fn run_malformed(
    label: &'static str,
    started: Instant,
    mut stream: TcpStream,
    bytes: Vec<u8>,
    drain_for: Duration,
) -> BadPeerOutcome {
    let mut outcome = base_outcome(label, started);
    outcome.connected = true;
    match stream.write_all(&bytes) {
        Ok(()) => outcome.bytes_sent = bytes.len(),
        Err(err) => {
            outcome.peer_reset = is_reset_error(&err);
            outcome.error = Some(format!("malformed write: {err}"));
        }
    }
    let _ = stream.flush();
    drain_into(&mut stream, drain_for, &mut outcome);
    finish(outcome, started)
}

fn run_storm(
    label: &'static str,
    started: Instant,
    addr: SocketAddr,
    connect_timeout: Duration,
    count: usize,
) -> BadPeerOutcome {
    let mut outcome = base_outcome(label, started);
    let mut connects_ok: usize = 0;
    let mut last_error: Option<String> = None;
    for _ in 0..count.saturating_sub(1) {
        match TcpStream::connect_timeout(&addr, connect_timeout) {
            Ok(stream) => {
                connects_ok += 1;
                drop(stream);
            }
            Err(err) => last_error = Some(format!("connect: {err}")),
        }
    }
    // We opened the first stream in the dispatcher; count it.
    outcome.connected = true;
    // Reuse `bytes_sent` to store the total successful connects (= the
    // first plus subsequent), so the caller has a typed number to assert
    // on without parsing the label.
    outcome.bytes_sent = connects_ok + 1;
    outcome.error = last_error;
    finish(outcome, started)
}

fn base_outcome(label: &'static str, started: Instant) -> BadPeerOutcome {
    let _ = started;
    BadPeerOutcome {
        label,
        connected: false,
        bytes_sent: 0,
        bytes_read: 0,
        reply_prefix: String::new(),
        server_closed: false,
        peer_reset: false,
        elapsed_ms: 0,
        error: None,
    }
}

fn finish(mut outcome: BadPeerOutcome, started: Instant) -> BadPeerOutcome {
    outcome.elapsed_ms = started.elapsed().as_millis() as u64;
    outcome
}

fn finish_with_error(
    mut outcome: BadPeerOutcome,
    started: Instant,
    err: std::io::Error,
) -> BadPeerOutcome {
    outcome.peer_reset = is_reset_error(&err);
    outcome.error = Some(err.to_string());
    finish(outcome, started)
}

fn drain_into(stream: &mut TcpStream, drain_for: Duration, outcome: &mut BadPeerOutcome) {
    let _ = stream.set_read_timeout(Some(drain_for.min(Duration::from_secs(2))));
    let mut buffer = [0u8; 1024];
    let mut reply = Vec::new();
    let deadline = Instant::now() + drain_for;
    loop {
        if Instant::now() >= deadline {
            break;
        }
        match stream.read(&mut buffer) {
            Ok(0) => {
                outcome.server_closed = true;
                break;
            }
            Ok(n) => {
                outcome.bytes_read += n;
                if reply.len() < 128 {
                    let take = (128 - reply.len()).min(n);
                    reply.extend_from_slice(&buffer[..take]);
                }
            }
            Err(err) if would_block_or_timeout(&err) => break,
            Err(err) => {
                outcome.peer_reset = is_reset_error(&err);
                outcome.error.get_or_insert_with(|| format!("drain: {err}"));
                break;
            }
        }
    }
    if !reply.is_empty() {
        outcome.reply_prefix = String::from_utf8_lossy(&reply).into_owned();
    }
}

fn is_reset_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe
    )
}

fn would_block_or_timeout(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// Tiny single-thread accept loop, used to exercise the bad-peer
    /// scenarios without pulling in `tina-http`.
    fn echo_listener() -> (
        SocketAddr,
        std::sync::Arc<AtomicUsize>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        let handle = thread::spawn(move || {
            listener.set_nonblocking(false).expect("blocking accept");
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                        let mut buf = [0u8; 1024];
                        // Drain whatever the client sent before half-close,
                        // then echo a tiny canned reply.
                        let _ = stream.read(&mut buf);
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                        let _ = stream.flush();
                        drop(stream);
                    }
                    Err(_) => {
                        if counter.load(Ordering::Relaxed) > 0 {
                            break;
                        }
                    }
                }
            }
        });
        // Give the listener a beat to be ready before clients arrive.
        thread::sleep(Duration::from_millis(20));
        (addr, accepted, handle)
    }

    use std::sync::atomic::Ordering;

    #[test]
    fn half_close_observes_server_reply_and_close() {
        let (addr, _accepted, server) = echo_listener();
        let outcome = run(
            "half_close",
            addr,
            Duration::from_secs(1),
            BadPeerScenario::HalfClose {
                request: b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_vec(),
                drain_for: Duration::from_millis(500),
            },
        );
        assert!(outcome.connected, "{outcome:?}");
        assert!(outcome.bytes_sent > 0, "{outcome:?}");
        assert!(outcome.bytes_read > 0, "{outcome:?}");
        assert!(outcome.server_closed, "{outcome:?}");
        let _ = server.join();
    }

    #[test]
    fn reset_immediately_records_attempt() {
        let (addr, accepted, server) = echo_listener();
        let outcome = run(
            "reset",
            addr,
            Duration::from_secs(1),
            BadPeerScenario::ResetImmediately,
        );
        assert!(outcome.connected);
        // We do not require server_closed/peer_reset because the kernel
        // may RST the connection silently. The connect must succeed.
        assert!(accepted.load(Ordering::Relaxed) >= 1, "{outcome:?}");
        let _ = server.join();
    }

    #[test]
    fn malformed_frame_records_error_or_close() {
        let (addr, _accepted, server) = echo_listener();
        let outcome = run(
            "malformed",
            addr,
            Duration::from_secs(1),
            BadPeerScenario::MalformedFrame {
                bytes: vec![0xff, 0x00, 0xff, 0x00],
                drain_for: Duration::from_millis(500),
            },
        );
        assert!(outcome.connected, "{outcome:?}");
        // Echo listener replies regardless; we just assert we got bytes
        // back or the server closed.
        assert!(
            outcome.bytes_read > 0 || outcome.server_closed,
            "{outcome:?}"
        );
        let _ = server.join();
    }

    #[test]
    fn reconnect_storm_counts_connects() {
        let (addr, accepted, server) = echo_listener();
        let outcome = run(
            "storm",
            addr,
            Duration::from_secs(1),
            BadPeerScenario::ReconnectStorm { count: 3 },
        );
        assert_eq!(outcome.bytes_sent, 3, "{outcome:?}");
        assert!(accepted.load(Ordering::Relaxed) >= 3, "{outcome:?}");
        let _ = server.join();
    }
}
