//! Substrate guard: the live TLS driver must ride Tina's
//! Betelgeuse TCP rail with rustls in sans-I/O mode — no private socket stack
//! and no per-op worker thread. This grep guard is the portable, decisive
//! "no `tina-tls-*` worker thread is spawned" proof: you cannot spawn a worker
//! without `thread::spawn`, and you cannot open a side-door socket without a
//! `std::net` listener/stream. The behavioural proof that TLS runs entirely on
//! the shard thread is the same-runtime client+server tests in `local_system`
//! and the lane unit tests in `driver::tls` (a single-threaded runtime drives a
//! full handshake to completion with no worker thread in sight).

const TLS_DRIVER: &str = include_str!("../src/driver/tls.rs");

#[test]
fn tls_driver_spawns_no_worker_thread() {
    assert!(
        !TLS_DRIVER.contains("thread::spawn"),
        "the TLS driver must not spawn a worker thread; TLS runs on the shard"
    );
    assert!(
        !TLS_DRIVER.contains("tina-tls-"),
        "no `tina-tls-*` worker thread name may exist in the TLS driver"
    );
    // The old worker lane carried its completions over an mpsc channel; the
    // on-shard pump uses Betelgeuse completion slots instead.
    assert!(
        !TLS_DRIVER.contains("SyncSender") && !TLS_DRIVER.contains("JoinHandle"),
        "no worker-lane command/completion channel may remain in the TLS driver"
    );
}

#[test]
fn tls_driver_opens_no_private_socket_stack() {
    assert!(
        !TLS_DRIVER.contains("std::net::TcpStream"),
        "the TLS driver must not own a std TcpStream; bytes come from the TCP rail"
    );
    assert!(
        !TLS_DRIVER.contains("std::net::TcpListener"),
        "the TLS driver must not bind a std TcpListener; it uses the runtime listener rail"
    );
    assert!(
        !TLS_DRIVER.contains("connect_timeout"),
        "no blocking connect_timeout — connect rides the completion-backed rail"
    );
    // rustls must be sans-I/O: the runtime owns a Connection, not a socket-
    // coupled StreamOwned.
    assert!(
        !TLS_DRIVER.contains("StreamOwned"),
        "rustls must run in sans-I/O mode (Connection), not StreamOwned over a socket"
    );
}
