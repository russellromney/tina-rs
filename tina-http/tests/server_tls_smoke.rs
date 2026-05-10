//! Native HTTPS server smoke. Real rustls client GETs `/counter`;
//! bad-key path returns typed `HttpsStartupError::Bind`.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use http::{Method, StatusCode};
use rustls::pki_types::ServerName;
use tina::prelude::*;
use tina_http::{
    HttpRequest, HttpRequestBody, HttpResponse, HttpsListener, HttpsListenerMsg, HttpsReady,
    HttpsServerConfig, HttpsStartupError, TlsServerIdentity,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(108)
    }
}

#[derive(Debug, Default)]
struct Counter {
    value: u64,
}

impl Isolate for Counter {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        let response = match (request.method.clone(), request.path.as_str()) {
            (Method::GET, "/counter") => HttpResponse::text(self.value.to_string()),
            (Method::POST, "/counter") => {
                self.value += 1;
                HttpResponse::text(self.value.to_string())
            }
            (Method::POST, "/echo") => {
                let body = match request.body {
                    HttpRequestBody::Buffered(b) => b,
                    HttpRequestBody::Stream(_) => Vec::new(),
                };
                HttpResponse::with_body(StatusCode::OK, body)
            }
            _ => HttpResponse::with_status(StatusCode::NOT_FOUND),
        };
        reply(response)
    }
}

/// One-shot `call(listener, Start, t)`; reply lands on an mpsc channel.
#[derive(Debug, Clone)]
enum DriverMsg {
    Begin {
        listener: Address<HttpsListenerMsg, Result<HttpsReady, HttpsStartupError>>,
        timeout: Duration,
    },
    Returned(CallOutcome<Result<HttpsReady, HttpsStartupError>>),
}

struct Driver {
    sender: mpsc::Sender<DriverOutcome>,
}

enum DriverOutcome {
    Replied(Result<HttpsReady, HttpsStartupError>),
    NonReply(&'static str),
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
            DriverMsg::Begin { listener, timeout } => {
                call(listener, HttpsListenerMsg::Start, timeout).reply(DriverMsg::Returned)
            }
            DriverMsg::Returned(outcome) => {
                let outcome = match outcome {
                    CallOutcome::Replied(inner) => DriverOutcome::Replied(inner),
                    CallOutcome::Full => DriverOutcome::NonReply("full"),
                    CallOutcome::Closed => DriverOutcome::NonReply("closed"),
                    CallOutcome::Timeout => DriverOutcome::NonReply("timeout"),
                };
                let _ = self.sender.send(outcome);
                stop()
            }
        }
    }
}

struct GeneratedIdentity {
    identity: TlsServerIdentity,
    cert_der: Vec<u8>,
}

fn generate_identity() -> GeneratedIdentity {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen self-sign");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();
    let identity = TlsServerIdentity::from_der(vec![cert_der.clone()], key_der);
    GeneratedIdentity { identity, cert_der }
}

fn run_https_request(addr: SocketAddr, root_cert_der: Vec<u8>, request: &[u8]) -> Vec<u8> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(root_cert_der))
        .expect("add self-signed root");
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = ServerName::try_from("localhost").expect("server name");
    let connection =
        rustls::ClientConnection::new(Arc::new(config), server_name).expect("client connection");
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).expect("tcp connect");
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    tcp.set_write_timeout(Some(Duration::from_secs(10)))
        .expect("write timeout");
    let mut stream = rustls::StreamOwned::new(connection, tcp);
    while stream.conn.is_handshaking() {
        stream
            .conn
            .complete_io(&mut stream.sock)
            .expect("client handshake");
    }
    stream.write_all(request).expect("write request");
    stream.flush().expect("flush request");
    let mut response = Vec::new();
    // `Connection: close` framing — rustls may surface the close as
    // an unclean read error even after all bytes arrived. Trust the
    // bytes we got.
    let _ = stream.read_to_end(&mut response);
    response
}

fn build_runtime() -> ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

#[test]
fn https_listener_serves_get_through_real_rustls_client() {
    let GeneratedIdentity { identity, cert_der } = generate_identity();

    let runtime = build_runtime();
    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");

    let listener_isolate = HttpsListener::<TestShard>::new(
        "127.0.0.1:0".parse().unwrap(),
        counter,
        HttpsServerConfig::dev(identity),
    );
    let listener = runtime
        .register_with_capacity::<HttpsListener<TestShard>, _>(listener_isolate, 8)
        .expect("register https listener");

    let (tx, rx) = mpsc::channel();
    let driver = runtime
        .register_with_capacity::<Driver, Infallible>(Driver { sender: tx }, 8)
        .expect("register driver");
    runtime
        .try_send(
            driver,
            DriverMsg::Begin {
                listener,
                timeout: Duration::from_secs(5),
            },
        )
        .expect("send Begin");

    let outcome = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("startup reply lands");
    let ready = match outcome {
        DriverOutcome::Replied(Ok(ready)) => ready,
        DriverOutcome::Replied(Err(error)) => {
            panic!("expected HttpsReady, got typed startup error: {error:?}")
        }
        DriverOutcome::NonReply(reason) => panic!("startup call did not reply: {reason}"),
    };

    let request = b"GET /counter HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = run_https_request(ready.local_addr, cert_der, request);
    let text = std::str::from_utf8(&response).expect("utf8 response");
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "expected 200 OK, got: {text:?}"
    );
    assert!(
        text.ends_with('0'),
        "expected counter body '0' at end of response: {text:?}"
    );

    let _ = runtime.try_send(listener, HttpsListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn https_listener_typed_failure_on_invalid_key_does_not_leak_listener() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Real cert + garbage private key → rustls `with_single_cert` fails.
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen self-sign");
    let cert_der = certified.cert.der().to_vec();
    let identity = TlsServerIdentity::from_der(vec![cert_der], b"not-a-real-private-key".to_vec());

    let runtime = build_runtime();
    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");
    let listener_isolate = HttpsListener::<TestShard>::new(
        "127.0.0.1:0".parse().unwrap(),
        counter,
        HttpsServerConfig::dev(identity),
    );
    let listener = runtime
        .register_with_capacity::<HttpsListener<TestShard>, _>(listener_isolate, 8)
        .expect("register https listener");

    let (tx, rx) = mpsc::channel();
    let driver = runtime
        .register_with_capacity::<Driver, Infallible>(Driver { sender: tx }, 8)
        .expect("register driver");
    runtime
        .try_send(
            driver,
            DriverMsg::Begin {
                listener,
                timeout: Duration::from_secs(5),
            },
        )
        .expect("send Begin");

    let outcome = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("startup reply lands");
    match outcome {
        DriverOutcome::Replied(Err(HttpsStartupError::Bind { source })) => {
            // Rustls `with_single_cert` failure → `CallError::TlsCertificate`.
            assert_eq!(
                source,
                tina_runtime::CallError::TlsCertificate,
                "expected TlsCertificate from invalid key, got {source:?}"
            );
        }
        DriverOutcome::Replied(Ok(ready)) => panic!(
            "expected typed bind failure on invalid key, got HttpsReady at {addr}",
            addr = ready.local_addr,
        ),
        DriverOutcome::NonReply(reason) => panic!("startup call did not reply: {reason}"),
    }

    // Shutdown must complete cleanly — runtime would surface a held
    // TlsListenerId in the shutdown report otherwise.
    let _ = runtime.shutdown();
}
