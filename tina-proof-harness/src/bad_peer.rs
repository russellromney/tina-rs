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
    /// Open and immediately reset the stream with `SO_LINGER(0)`. No bytes
    /// sent. Servers must treat this as an abrupt peer reset, not as a normal
    /// request.
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
    /// Open a TLS port and write a deliberately invalid TLS record
    /// (e.g., truncated `ClientHello`, wrong content type, or bytes
    /// that decode as a plaintext request). A correctly-configured TLS
    /// listener must close the connection without writing application
    /// bytes back. The harness records `bytes_read`/`server_closed`
    /// so a specimen can assert "server closed without replying".
    TlsHandshakeFailure {
        client_hello: Vec<u8>,
        drain_for: Duration,
    },
    /// Open and close `count` times back-to-back with no traffic.
    ReconnectStorm { count: usize },
}

impl BadPeerScenario {
    /// Constructs a [`BadPeerScenario::TlsHandshakeFailure`] with a
    /// canned malformed TLS record. The bytes are a real TLS record
    /// header (type=Handshake, version=0x0303, length=10) followed by
    /// ten garbage bytes that do not form a valid `ClientHello`. A
    /// rustls/native TLS listener will close on `decoder error`.
    ///
    /// Use this for the default "TLS rejects bad client" proof. Pass
    /// your own bytes via the `TlsHandshakeFailure` variant when you
    /// want a specific failure mode (e.g., plain HTTP to a TLS port).
    pub fn tls_handshake_garbage(drain_for: Duration) -> Self {
        Self::TlsHandshakeFailure {
            client_hello: vec![
                // TLS record header: ContentType=Handshake(22),
                // ProtocolVersion=0x0303 (TLS 1.2), length=10.
                0x16, 0x03, 0x03, 0x00,
                0x0a, // Body: 10 garbage bytes (not a real ClientHello).
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            ],
            drain_for,
        }
    }

    /// Constructs a [`BadPeerScenario::TlsHandshakeFailure`] that
    /// sends a plain HTTP request to a TLS port. This catches the
    /// common ops misconfiguration of pointing an HTTP client at the
    /// TLS listener. A correctly-configured listener rejects.
    pub fn tls_plaintext_request(drain_for: Duration) -> Self {
        Self::TlsHandshakeFailure {
            client_hello: b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_vec(),
            drain_for,
        }
    }
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
    /// Number of connection attempts that failed during this scenario.
    ///
    /// Mostly useful for [`BadPeerScenario::ReconnectStorm`], where aggregate
    /// connection truth matters more than the final error.
    pub connects_failed: usize,
    /// First few connection errors observed during this scenario.
    pub connection_errors: Vec<String>,
    /// Per-scenario typed counter:
    /// - `ReconnectStorm`: total successful connects (the first plus
    ///   any retries).
    /// - all other scenarios: 1 when `connected` else 0.
    ///
    /// Use this instead of overloading `bytes_sent` to count connects.
    pub connects_ok: usize,
}

impl BadPeerOutcome {
    pub fn summary_line(&self) -> String {
        format!(
            "bad_peer label={} connected={} connects_ok={} connects_failed={} bytes_sent={} bytes_read={} server_closed={} peer_reset={} elapsed_ms={} error={}",
            self.label,
            self.connected,
            self.connects_ok,
            self.connects_failed,
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
    if let BadPeerScenario::ReconnectStorm { count } = scenario {
        return run_storm(label, started, addr, connect_timeout, count);
    }

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
                connects_failed: 1,
                connection_errors: vec![format!("connect: {err}")],
                connects_ok: 0,
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
        BadPeerScenario::TlsHandshakeFailure {
            client_hello,
            drain_for,
        } => {
            // Same wire-level shape as MalformedFrame; distinct label
            // surface so the typed scenario name is "TLS failure".
            run_malformed(label, started, stream, client_hello, drain_for)
        }
        BadPeerScenario::ReconnectStorm { .. } => unreachable!("handled before initial connect"),
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
    outcome.connects_ok = 1;
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
    outcome.connects_ok = 1;
    let socket = socket2::Socket::from(stream);
    if let Err(err) = socket.set_linger(Some(Duration::ZERO)) {
        outcome.error = Some(format!("set_linger_zero: {err}"));
    }
    drop(socket);
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
    outcome.connects_ok = 1;
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
    outcome.connects_ok = 1;
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
    outcome.connects_ok = 1;
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
    outcome.connects_ok = 1;
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
    let mut connects_failed: usize = 0;
    let mut connection_errors = Vec::new();
    for _ in 0..count {
        match TcpStream::connect_timeout(&addr, connect_timeout) {
            Ok(stream) => {
                connects_ok += 1;
                drop(stream);
            }
            Err(err) => {
                connects_failed += 1;
                if connection_errors.len() < 8 {
                    connection_errors.push(format!("connect: {err}"));
                }
            }
        }
    }
    outcome.connected = connects_ok > 0;
    outcome.connects_ok = connects_ok;
    outcome.connects_failed = connects_failed;
    outcome.connection_errors = connection_errors;
    if connects_failed > 0 {
        outcome.error = Some(format!("{connects_failed} connect attempts failed"));
    }
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
        connects_failed: 0,
        connection_errors: Vec::new(),
        connects_ok: 0,
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
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, mpsc};

    /// Tiny single-thread accept loop, used to exercise the bad-peer
    /// scenarios without pulling in `tina-http`. Non-blocking accept
    /// with a tight poll so the listener exits within ~50ms after the
    /// last client; otherwise running 20+ tests would serialise to
    /// many seconds per test.
    fn echo_listener() -> (
        SocketAddr,
        std::sync::Arc<AtomicUsize>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        let handle = thread::spawn(move || {
            // Tight cap so a misbehaving test cannot wedge the suite.
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut idle_polls_after_first_client: u32 = 0;
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                        idle_polls_after_first_client = 0;
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                        let mut buf = [0u8; 1024];
                        // Drain whatever the client sent before half-close,
                        // then echo a tiny canned reply.
                        let _ = stream.read(&mut buf);
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                        let _ = stream.flush();
                        drop(stream);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // After we have served at least one client, exit
                        // once a short idle window passes (no new arrivals).
                        if counter.load(Ordering::Relaxed) > 0 {
                            idle_polls_after_first_client += 1;
                            if idle_polls_after_first_client >= 10 {
                                break;
                            }
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
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
        assert_eq!(outcome.connects_ok, 1, "{outcome:?}");
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
        // Join the listener first so its non-blocking accept loop has a
        // chance to observe the connect before we read the counter.
        let _ = server.join();
        // We do not require server_closed/peer_reset because the kernel
        // may RST the connection silently. The connect must succeed.
        assert!(accepted.load(Ordering::Relaxed) >= 1, "{outcome:?}");
    }

    #[test]
    fn reset_immediately_uses_linger_zero_reset_shape() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1];
            let reset = match stream.read(&mut buf) {
                Err(err) => is_reset_error(&err),
                Ok(_) => false,
            };
            tx.send(reset).expect("send reset observation");
        });

        let outcome = run(
            "reset",
            addr,
            Duration::from_secs(1),
            BadPeerScenario::ResetImmediately,
        );
        assert!(outcome.connected, "{outcome:?}");
        let reset_seen = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("server observation");
        let _ = server.join();
        assert!(
            reset_seen || outcome.error.is_some(),
            "server should see RST unless the platform rejected linger-zero setup: {outcome:?}"
        );
    }

    #[test]
    fn reconnect_storm_preserves_aggregate_connection_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);

        let outcome = run(
            "storm",
            addr,
            Duration::from_millis(100),
            BadPeerScenario::ReconnectStorm { count: 4 },
        );
        // Closed port: every attempted reconnect is counted as a failure,
        // and the OS error strings are preserved. No listen backlog timing
        // participates in this proof.
        assert!(!outcome.connected, "{outcome:?}");
        assert_eq!(outcome.connects_ok, 0, "{outcome:?}");
        assert_eq!(outcome.connects_failed, 4, "{outcome:?}");
        assert_eq!(outcome.connects_ok + outcome.connects_failed, 4);
        assert!(!outcome.connection_errors.is_empty(), "{outcome:?}");
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|e| e.contains("failed"))
        );
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
        assert_eq!(outcome.connects_ok, 3, "{outcome:?}");
        assert_eq!(outcome.bytes_sent, 0, "storm sends no bytes: {outcome:?}");
        let _ = server.join();
        assert!(accepted.load(Ordering::Relaxed) >= 3, "{outcome:?}");
    }

    #[test]
    fn slowloris_writes_byte_by_byte_then_drains() {
        let (addr, _accepted, server) = echo_listener();
        let trailer = b"X-Slow: 1\r\n\r\n".to_vec();
        let trailer_len = trailer.len();
        let outcome = run(
            "slowloris",
            addr,
            Duration::from_secs(1),
            BadPeerScenario::Slowloris {
                prelude: b"GET / HTTP/1.1\r\nHost: x\r\n".to_vec(),
                trailer,
                byte_delay: Duration::from_millis(1),
                give_up_after: Duration::from_millis(500),
            },
        );
        assert!(outcome.connected, "{outcome:?}");
        assert_eq!(outcome.connects_ok, 1);
        // The slowloris write loop should have completed before give_up:
        // prelude + every trailer byte.
        assert!(
            outcome.bytes_sent >= trailer_len,
            "expected slowloris to write at least the trailer; got {}: {outcome:?}",
            outcome.bytes_sent,
        );
        let _ = server.join();
    }

    #[test]
    fn stalled_writer_pauses_between_chunks() {
        let (addr, _accepted, server) = echo_listener();
        let outcome = run(
            "stalled_writer",
            addr,
            Duration::from_secs(1),
            BadPeerScenario::StalledWriter {
                first_chunk: b"GET / HTTP/1.1\r\n".to_vec(),
                rest: b"Host: x\r\nConnection: close\r\n\r\n".to_vec(),
                stall_for: Duration::from_millis(50),
            },
        );
        assert!(outcome.connected, "{outcome:?}");
        assert_eq!(outcome.connects_ok, 1);
        // The scenario should record at least the stall plus the writes.
        assert!(
            outcome.elapsed_ms >= 40,
            "stall window should be observable: {outcome:?}"
        );
        let _ = server.join();
    }

    #[test]
    fn tls_handshake_failure_helper_constructs_invalid_record() {
        // The helper is purely constructive; we assert the canned bytes
        // are non-empty and that the dispatcher accepts the scenario
        // via the standard run() entrypoint against the echo listener
        // (the echo listener replies plaintext; that is fine because
        // the test only checks the wire path and outcome shape).
        let (addr, _accepted, server) = echo_listener();
        let scenario = BadPeerScenario::tls_handshake_garbage(Duration::from_millis(200));
        match &scenario {
            BadPeerScenario::TlsHandshakeFailure { client_hello, .. } => {
                assert_eq!(client_hello[0], 0x16, "TLS Handshake content type");
                assert!(client_hello.len() >= 5, "must include TLS record header");
            }
            _ => panic!("helper must produce TlsHandshakeFailure"),
        }
        let outcome = run("tls_garbage", addr, Duration::from_secs(1), scenario);
        assert!(outcome.connected, "{outcome:?}");
        assert_eq!(outcome.connects_ok, 1);
        assert!(outcome.bytes_sent > 0, "{outcome:?}");
        let _ = server.join();
    }

    #[test]
    fn connect_failure_returns_typed_outcome() {
        // 127.0.0.1 with a port we never bind: the connect must fail,
        // and the outcome should report that without panic.
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let outcome = run(
            "connect_fail",
            addr,
            Duration::from_millis(200),
            BadPeerScenario::ResetImmediately,
        );
        assert!(!outcome.connected, "{outcome:?}");
        assert_eq!(outcome.connects_ok, 0);
        assert!(outcome.error.is_some(), "{outcome:?}");
    }
}
