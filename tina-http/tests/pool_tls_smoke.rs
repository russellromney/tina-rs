//! Pool over HTTPS. `HttpConnectionPool` is target-agnostic — same
//! capacity-1 admission gate, just an HTTPS target on the wire.
//! Covers happy-path admission + `PoolFull` when the slot is busy.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use http::{HeaderMap, Method, StatusCode, Version};
use tina::prelude::*;
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientError, HttpConnectionPool, HttpPoolMsg, HttpRequest,
    HttpRequestBody, HttpResponse, HttpResponseBody, HttpTarget, OutboundCall, PoolConfig,
    TlsTrustRoots,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(303)
    }
}

fn build_cert_bundle() -> (Vec<u8>, rustls::ServerConfig) {
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
    (cert_der, server_config)
}

fn spawn_one_shot_https_server(server_config: rustls::ServerConfig) -> SocketAddr {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let (tcp, _) = match listener.accept() {
            Ok(p) => p,
            Err(_) => return,
        };
        tcp.set_read_timeout(Some(Duration::from_secs(5))).ok();
        tcp.set_write_timeout(Some(Duration::from_secs(5))).ok();
        let connection = match rustls::ServerConnection::new(Arc::new(server_config)) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut stream = rustls::StreamOwned::new(connection, tcp);
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let body: &[u8] = b"pool-ok";
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
        stream.conn.send_close_notify();
        let _ = stream.flush();
    });
    addr
}

#[derive(Debug, Clone)]
enum DriverMsg {
    Begin {
        pool: Address<HttpPoolMsg, Result<HttpResponse, HttpClientError>>,
        outbound: OutboundCall,
        timeout: Duration,
    },
    Returned(CallOutcome<Result<HttpResponse, HttpClientError>>),
}

struct Driver {
    sender: mpsc::Sender<Result<HttpResponse, HttpClientError>>,
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
            DriverMsg::Begin {
                pool,
                outbound,
                timeout,
            } => call(pool, HttpPoolMsg::Submit(outbound), timeout).reply(DriverMsg::Returned),
            DriverMsg::Returned(outcome) => {
                let result = match outcome {
                    CallOutcome::Replied(inner) => inner,
                    CallOutcome::Full => Err(HttpClientError::Busy),
                    CallOutcome::Closed => Err(HttpClientError::Closed),
                    CallOutcome::Timeout => Err(HttpClientError::Timeout),
                };
                let _ = self.sender.send(result);
                stop()
            }
        }
    }
}

/// HTTPS server that handshakes, then holds the stream silent so the
/// in-flight Submit ties up the pool slot until its deadline.
fn spawn_black_hole_https_server(server_config: rustls::ServerConfig) -> SocketAddr {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let (tcp, _) = match listener.accept() {
            Ok(p) => p,
            Err(_) => return,
        };
        tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let connection = match rustls::ServerConnection::new(Arc::new(server_config)) {
            Ok(c) => c,
            Err(_) => return,
        };
        // Drive handshake so client's `tls_connect` returns, then
        // sleep silent so the in-flight Submit holds the slot.
        let mut stream = rustls::StreamOwned::new(connection, tcp);
        while stream.conn.is_handshaking() {
            if stream.conn.complete_io(&mut stream.sock).is_err() {
                return;
            }
        }
        thread::sleep(Duration::from_secs(5));
        drop(stream);
    });
    addr
}

#[test]
fn pool_refuses_with_full_when_https_slot_busy() {
    // Submit_A holds the slot via a black-hole server; Submit_B must
    // get `PoolFull` while in-flight. Same shape as the TCP test.
    let (cert_der, server_config) = build_cert_bundle();
    let slow_addr = spawn_black_hole_https_server(server_config);

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let client = runtime
        .register_with_capacity::<HttpClient<TestShard>, Infallible>(
            HttpClient::<TestShard>::new(HttpClientConfig::dev()),
            16,
        )
        .expect("register client");
    let pool = runtime
        .register_with_capacity::<HttpConnectionPool<TestShard>, _>(
            HttpConnectionPool::<TestShard>::new(PoolConfig::dev(), client),
            16,
        )
        .expect("register pool");

    let target = HttpTarget::https(
        slow_addr,
        "localhost",
        TlsTrustRoots::from_der(vec![cert_der]),
    );
    let request = HttpRequest {
        method: Method::GET,
        path: "/".to_string(),
        version: Version::HTTP_11,
        headers: HeaderMap::new(),
        body: HttpRequestBody::default(),
    };

    let (tx_a, _rx_a) = mpsc::channel();
    let driver_a = runtime
        .register_with_capacity::<Driver, Infallible>(Driver { sender: tx_a }, 8)
        .expect("register driver A");
    runtime
        .try_send(
            driver_a,
            DriverMsg::Begin {
                pool,
                outbound: OutboundCall {
                    target: target.clone(),
                    request: request.clone(),
                },
                timeout: Duration::from_secs(5),
            },
        )
        .expect("send Begin A");

    // Let Submit_A reach the pool and set in_flight.
    thread::sleep(Duration::from_millis(150));

    let (tx_b, rx_b) = mpsc::channel();
    let driver_b = runtime
        .register_with_capacity::<Driver, Infallible>(Driver { sender: tx_b }, 8)
        .expect("register driver B");
    runtime
        .try_send(
            driver_b,
            DriverMsg::Begin {
                pool,
                outbound: OutboundCall { target, request },
                timeout: Duration::from_secs(2),
            },
        )
        .expect("send Begin B");

    let result_b = rx_b
        .recv_timeout(Duration::from_secs(3))
        .expect("Submit_B reply");
    assert!(
        matches!(result_b, Err(HttpClientError::PoolFull)),
        "expected PoolFull while HTTPS slot is busy, got {result_b:?}"
    );

    let _ = runtime.shutdown();
}

#[test]
fn pool_serial_admission_works_with_https_target() {
    let (cert_der, server_config) = build_cert_bundle();
    let addr = spawn_one_shot_https_server(server_config);

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let client = runtime
        .register_with_capacity::<HttpClient<TestShard>, Infallible>(
            HttpClient::<TestShard>::new(HttpClientConfig::dev()),
            16,
        )
        .expect("register client");
    let pool = runtime
        .register_with_capacity::<HttpConnectionPool<TestShard>, _>(
            HttpConnectionPool::<TestShard>::new(PoolConfig::dev(), client),
            16,
        )
        .expect("register pool");

    let (tx, rx) = mpsc::channel();
    let driver = runtime
        .register_with_capacity::<Driver, Infallible>(Driver { sender: tx }, 8)
        .expect("register driver");
    let target = HttpTarget::https(addr, "localhost", TlsTrustRoots::from_der(vec![cert_der]));
    runtime
        .try_send(
            driver,
            DriverMsg::Begin {
                pool,
                outbound: OutboundCall {
                    target,
                    request: HttpRequest {
                        method: Method::GET,
                        path: "/".to_string(),
                        version: Version::HTTP_11,
                        headers: HeaderMap::new(),
                        body: HttpRequestBody::default(),
                    },
                },
                timeout: Duration::from_secs(5),
            },
        )
        .expect("send Begin");

    let result = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("driver delivers result")
        .expect("pool fetches over https");
    assert_eq!(result.status, StatusCode::OK);
    let body = match &result.body {
        HttpResponseBody::Buffered(b) => b.clone(),
        HttpResponseBody::Stream(_) | HttpResponseBody::ChunkedStream(_) => Vec::new(),
    };
    assert_eq!(body, b"pool-ok");

    let _ = runtime.shutdown();
}
