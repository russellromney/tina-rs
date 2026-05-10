//! HTTP/HTTPS body parity proof: a streamed response served over
//! HTTPS records the same body-pressure metric shape as the plain
//! HTTP version. Body code paths are unified above the transport;
//! this test confirms metrics behave identically over the two
//! transports.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use rustls::pki_types::ServerName;
use tina::prelude::*;
use tina_http::{
    BodyMetrics, HttpRequest, HttpResponse, HttpsListener, HttpsListenerMsg, HttpsReady,
    HttpsServerConfig, HttpsStartupError, ResponseChunkMsg, ResponseChunkReply, ResponseStream,
    TlsServerIdentity,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(110)
    }
}

struct ChunkProducer {
    chunk_size: usize,
    total_chunks: usize,
    yielded: usize,
}

impl Isolate for ChunkProducer {
    tina::isolate_types! {
        message: ResponseChunkMsg,
        reply: ResponseChunkReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        _msg: ResponseChunkMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        if self.yielded >= self.total_chunks {
            return reply(ResponseChunkReply::Eof);
        }
        let i = self.yielded;
        self.yielded += 1;
        reply(ResponseChunkReply::Chunk(vec![
            (i % 251) as u8;
            self.chunk_size
        ]))
    }
}

struct StreamingService {
    producer: Address<ResponseChunkMsg, ResponseChunkReply>,
    content_length: usize,
}

impl Isolate for StreamingService {
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
        let response = if request.path == "/big" {
            HttpResponse::with_stream(
                StatusCode::OK,
                ResponseStream {
                    content_length: self.content_length,
                    source: self.producer,
                },
            )
        } else {
            HttpResponse::not_found()
        };
        reply(response)
    }
}

fn generate_identity() -> (TlsServerIdentity, Vec<u8>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen self-sign");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();
    (
        TlsServerIdentity::from_der(vec![cert_der.clone()], key_der),
        cert_der,
    )
}

fn read_through_real_rustls(addr: SocketAddr, root_cert_der: Vec<u8>) -> Vec<u8> {
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
    stream
        .write_all(b"GET /big HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write request");
    stream.flush().expect("flush request");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response
}

#[test]
fn streaming_response_over_https_records_same_metric_shape_as_http() {
    // Same workload as `body_metrics::streaming_response_charges_per_chunk_and_releases_after_write`,
    // but over HTTPS via `HttpsListener`. Asserts the metric snapshot
    // has nonzero high-water and drains to zero on shutdown — i.e.
    // body code paths above the transport produce the same shape.
    let (identity, root_cert) = generate_identity();
    let metrics = BodyMetrics::new();

    let chunk_size = 1024;
    let total_chunks = 8;
    let total_bytes = chunk_size * total_chunks;

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let producer = runtime
        .register_with_capacity::<ChunkProducer, Infallible>(
            ChunkProducer {
                chunk_size,
                total_chunks,
                yielded: 0,
            },
            16,
        )
        .expect("register producer");
    let service = runtime
        .register_with_capacity::<StreamingService, Infallible>(
            StreamingService {
                producer,
                content_length: total_bytes,
            },
            16,
        )
        .expect("register service");

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener_isolate =
        HttpsListener::<TestShard>::new(bind, service, HttpsServerConfig::dev(identity))
            .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpsListener<TestShard>, _>(listener_isolate, 8)
        .expect("register listener");

    type StartOutcome = CallOutcome<Result<HttpsReady, HttpsStartupError>>;
    let outcome: StartOutcome = runtime
        .call_blocking(listener, HttpsListenerMsg::Start, Duration::from_secs(5))
        .expect("startup call runs");
    let ready = match outcome {
        CallOutcome::Replied(Ok(ready)) => ready,
        other => panic!("expected ready, got {other:?}"),
    };

    let response = read_through_real_rustls(ready.local_addr, root_cert);
    let separator = b"\r\n\r\n";
    let head_end = response
        .windows(separator.len())
        .position(|w| w == separator)
        .expect("response has CRLFCRLF");
    let body = &response[head_end + separator.len()..];
    assert_eq!(body.len(), total_bytes, "wire body size matches declared");

    let _ = runtime.try_send(listener, HttpsListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(
        snap.response_body_high_water > 0,
        "metrics must charge response chunks over HTTPS"
    );
    assert!(
        snap.response_body_high_water <= total_bytes,
        "high water cannot exceed declared length"
    );
    assert!(
        snap.drained(),
        "all response charges must release after writes complete; got {snap:?}"
    );
    assert_eq!(
        snap.body_io_error_count, 0,
        "happy path: clean delivery records zero IO errors"
    );
}
