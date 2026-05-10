//! HTTPS keepalive smoke test: drive `build_keepalive_pool` against a
//! real rustls server that supports keepalive (loops reading
//! requests on one TLS stream), prove sequential requests share one
//! TLS handshake.

#![allow(dead_code)]

mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tina::pool::{AcquireOutcome, PoolConfig, PoolLease, ReleaseDisposition};
use tina::prelude::*;
use tina_http::{
    HttpClientConfig, HttpRequest, HttpTarget, KeepaliveConnAddr, KeepaliveConnectionMsg,
    KeepaliveOutcome, TlsTrustRoots, build_keepalive_pool,
};
use tina_runtime::pool::{WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

use common::TestShard;

// =====================================================================
// Test cert + keepalive HTTPS server
// =====================================================================

struct CertBundle {
    cert_der: Vec<u8>,
    server_config: rustls::ServerConfig,
}

fn build_cert_bundle() -> CertBundle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen self-sign");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();
    let server_cert = rustls::pki_types::CertificateDer::from(cert_der.clone());
    let server_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
    );
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![server_cert], server_key)
        .expect("server config");
    CertBundle {
        cert_der,
        server_config,
    }
}

struct TlsKeepaliveServer {
    addr: SocketAddr,
    accepts: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TlsKeepaliveServer {
    fn start(server_config: rustls::ServerConfig) -> Self {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind tls keepalive");
        let addr = listener.local_addr().expect("local addr");
        listener.set_nonblocking(true).expect("nonblocking");
        let accepts = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let server_config = Arc::new(server_config);

        let accepts_t = accepts.clone();
        let requests_t = requests.clone();
        let stop_t = stop.clone();
        let handle = thread::spawn(move || {
            while !stop_t.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((tcp, _)) => {
                        accepts_t.fetch_add(1, Ordering::Relaxed);
                        let cfg = server_config.clone();
                        let req_ctr = requests_t.clone();
                        let stop_inner = stop_t.clone();
                        thread::spawn(move || {
                            let _ = tcp.set_nonblocking(false);
                            let _ = tcp.set_read_timeout(Some(Duration::from_secs(5)));
                            let _ = tcp.set_write_timeout(Some(Duration::from_secs(5)));
                            let connection = match rustls::ServerConnection::new(cfg) {
                                Ok(c) => c,
                                Err(_) => return,
                            };
                            let mut stream = rustls::StreamOwned::new(connection, tcp);
                            let mut buf: Vec<u8> = Vec::with_capacity(4096);
                            loop {
                                if stop_inner.load(Ordering::Acquire) {
                                    return;
                                }
                                // Read until we have a complete head.
                                let head_end = loop {
                                    if let Some(p) = find_double_crlf(&buf) {
                                        break p;
                                    }
                                    let mut chunk = [0u8; 1024];
                                    let n = match stream.read(&mut chunk) {
                                        Ok(0) => return,
                                        Ok(n) => n,
                                        Err(_) => return,
                                    };
                                    buf.extend_from_slice(&chunk[..n]);
                                };
                                let head_str = std::str::from_utf8(&buf[..head_end]).unwrap_or("");
                                let cl = parse_content_length(head_str);
                                let body_end = head_end + cl;
                                while buf.len() < body_end {
                                    let mut chunk = [0u8; 1024];
                                    let n = match stream.read(&mut chunk) {
                                        Ok(0) => return,
                                        Ok(n) => n,
                                        Err(_) => return,
                                    };
                                    buf.extend_from_slice(&chunk[..n]);
                                }
                                req_ctr.fetch_add(1, Ordering::Relaxed);
                                let body = b"ok";
                                let resp = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                    body.len()
                                );
                                if stream.write_all(resp.as_bytes()).is_err() {
                                    return;
                                }
                                if stream.write_all(body).is_err() {
                                    return;
                                }
                                let _ = stream.flush();
                                buf.drain(..body_end);
                            }
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            accepts,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn accepts(&self) -> usize {
        self.accepts.load(Ordering::Relaxed)
    }
    fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }
}

impl Drop for TlsKeepaliveServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn parse_content_length(head: &str) -> usize {
    for line in head.split("\r\n") {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                return n;
            }
        }
    }
    0
}

// =====================================================================
// Tiny driver isolate — acquire/request/release loop
// =====================================================================

type PoolAddr = Address<WorkerPoolMsg<KeepaliveConnAddr>, WorkerPoolReply<KeepaliveConnAddr>>;

#[derive(Debug, Clone)]
enum DriverEvent {
    Acquired,
    RequestOk,
    RequestErr(String),
    Released,
}

enum DriverMsg {
    BeginAcquire,
    BeginRequest(HttpRequest),
    BeginRelease,
    AcquireReturned(CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>),
    RequestReturned(CallOutcome<KeepaliveOutcome>),
    ReleaseReturned(CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>),
}

impl std::fmt::Debug for DriverMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeginAcquire => f.write_str("BeginAcquire"),
            Self::BeginRequest(_) => f.write_str("BeginRequest"),
            Self::BeginRelease => f.write_str("BeginRelease"),
            Self::AcquireReturned(_) => f.write_str("AcquireReturned"),
            Self::RequestReturned(_) => f.write_str("RequestReturned"),
            Self::ReleaseReturned(_) => f.write_str("ReleaseReturned"),
        }
    }
}

struct Driver {
    pool: PoolAddr,
    notify: mpsc::Sender<DriverEvent>,
    lease: Option<PoolLease<KeepaliveConnAddr>>,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DriverMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::BeginAcquire => {
                call(self.pool, WorkerPoolMsg::Acquire, Duration::from_secs(5))
                    .reply(DriverMsg::AcquireReturned)
            }
            DriverMsg::AcquireReturned(outcome) => match outcome {
                CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => {
                    self.lease = Some(lease);
                    let _ = self.notify.send(DriverEvent::Acquired);
                    noop()
                }
                _ => panic!("acquire failed"),
            },
            DriverMsg::BeginRequest(request) => {
                let lease = self.lease.as_ref().expect("lease held");
                let addr = *lease.handle();
                call(
                    addr,
                    KeepaliveConnectionMsg::request(request, Duration::from_secs(5)),
                    Duration::from_secs(5),
                )
                .reply(DriverMsg::RequestReturned)
            }
            DriverMsg::RequestReturned(outcome) => {
                match outcome {
                    CallOutcome::Replied(KeepaliveOutcome::Request { result: Ok(_), .. }) => {
                        let _ = self.notify.send(DriverEvent::RequestOk);
                    }
                    CallOutcome::Replied(KeepaliveOutcome::Request { result: Err(e), .. }) => {
                        let _ = self.notify.send(DriverEvent::RequestErr(format!("{e:?}")));
                    }
                    other => {
                        let _ = self
                            .notify
                            .send(DriverEvent::RequestErr(format!("call: {other:?}")));
                    }
                }
                noop()
            }
            DriverMsg::BeginRelease => {
                let lease = self.lease.take().expect("lease held");
                tina_runtime::pool::release_effect(
                    lease,
                    self.pool,
                    ReleaseDisposition::Reuse,
                    Duration::from_secs(2),
                    DriverMsg::ReleaseReturned,
                )
            }
            DriverMsg::ReleaseReturned(_) => {
                let _ = self.notify.send(DriverEvent::Released);
                noop()
            }
        }
    }
}

fn wait_for(rx: &mpsc::Receiver<DriverEvent>) -> DriverEvent {
    rx.recv_timeout(Duration::from_secs(10))
        .expect("driver event")
}

#[test]
fn https_keepalive_pool_reuses_one_tls_handshake_for_three_requests() {
    let bundle = build_cert_bundle();
    let server = TlsKeepaliveServer::start(bundle.server_config);

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let trust = TlsTrustRoots::from_der(vec![bundle.cert_der]);
    let target = HttpTarget::https(server.addr, "localhost", trust);

    let handles = build_keepalive_pool(
        &runtime,
        target,
        HttpClientConfig::dev(),
        PoolConfig::new(1, 4),
        16,
        16,
    )
    .expect("build pool");

    let (tx, rx) = mpsc::channel();
    let driver_addr = runtime
        .register_with_capacity::<Driver, Infallible>(
            Driver {
                pool: handles.pool,
                notify: tx,
                lease: None,
            },
            16,
        )
        .expect("register driver");

    // HTTPS target's UseServerName policy injects Host:localhost
    // automatically; supplying it ourselves would conflict.
    let request_template = || HttpRequest::get("/").build();

    for _ in 0..3 {
        let _ = runtime.try_send(driver_addr, DriverMsg::BeginAcquire);
        match wait_for(&rx) {
            DriverEvent::Acquired => {}
            other => panic!("expected Acquired, got {other:?}"),
        }
        let _ = runtime.try_send(driver_addr, DriverMsg::BeginRequest(request_template()));
        match wait_for(&rx) {
            DriverEvent::RequestOk => {}
            other => panic!("expected RequestOk, got {other:?}"),
        }
        let _ = runtime.try_send(driver_addr, DriverMsg::BeginRelease);
        match wait_for(&rx) {
            DriverEvent::Released => {}
            other => panic!("expected Released, got {other:?}"),
        }
    }

    assert_eq!(
        server.accepts(),
        1,
        "three sequential HTTPS requests must share one TLS handshake"
    );
    assert_eq!(server.requests(), 3);

    // Stop the connection isolate before runtime shutdown so the TLS
    // session ends cleanly.
    let _ = runtime.try_send(handles.connections[0], KeepaliveConnectionMsg::Stop);

    let _ = runtime.shutdown();
}
