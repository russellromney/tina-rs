//! HTTP/HTTPS body parity proof: a streamed response served over
//! HTTPS records the same body-pressure metric shape as the plain
//! HTTP version. Body code paths are unified above the transport;
//! these tests confirm metrics and wire framing behave identically
//! over the two transports for both `Content-Length` and
//! `Transfer-Encoding: chunked`.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use rustls::pki_types::ServerName;
use tina::prelude::*;
use tina_http::{
    BodyMetrics, HttpConnectionMsg, HttpRequest, HttpRequestBody, HttpResponse, HttpsListener,
    HttpsListenerMsg, HttpsReady, HttpsServerConfig, HttpsStartupError, IterBodySource,
    RequestChunkReply, ResponseChunkMsg, ResponseChunkReply, ResponseStream, TlsServerIdentity,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
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

/// Chunked-over-HTTPS parity. Same `IterBodySource` + chunked
/// framing path that the plain-HTTP tests cover; the only thing
/// that should change is the transport.
struct ChunkedService {
    source: Address<ResponseChunkMsg, ResponseChunkReply>,
}

impl Isolate for ChunkedService {
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
        let response = if request.path == "/big-chunked" {
            HttpResponse::stream_chunked(StatusCode::OK, self.source)
        } else {
            HttpResponse::not_found()
        };
        reply(response)
    }
}

fn read_through_real_rustls_path(addr: SocketAddr, root_cert_der: Vec<u8>, path: &str) -> Vec<u8> {
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
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("write request");
    stream.flush().expect("flush request");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response
}

fn decode_chunked_body(mut bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let crlf = bytes
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| "chunked: missing size CRLF".to_string())?;
        let size_str = std::str::from_utf8(&bytes[..crlf])
            .map_err(|_| "chunked: size line is not ASCII".to_string())?;
        let size_str = size_str.split(';').next().unwrap_or("");
        let size = usize::from_str_radix(size_str.trim(), 16)
            .map_err(|e| format!("chunked: bad size {size_str:?}: {e}"))?;
        bytes = &bytes[crlf + 2..];
        if size == 0 {
            return Ok(out);
        }
        if bytes.len() < size + 2 {
            return Err(format!("chunked: truncated chunk size={size}"));
        }
        out.extend_from_slice(&bytes[..size]);
        if &bytes[size..size + 2] != b"\r\n" {
            return Err("chunked: missing CRLF after data".to_string());
        }
        bytes = &bytes[size + 2..];
    }
}

#[test]
fn chunked_response_over_https_matches_http_shape() {
    let (identity, root_cert) = generate_identity();
    let metrics = BodyMetrics::new();

    let chunks_data: Vec<Vec<u8>> = vec![
        b"hello, ".to_vec(),
        b"chunked over ".to_vec(),
        b"tls".to_vec(),
    ];
    let total_decoded: Vec<u8> = chunks_data.iter().flatten().copied().collect();
    let largest_chunk = chunks_data.iter().map(Vec::len).max().unwrap();

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let source = runtime
        .register_with_capacity::<IterBodySource<TestShard>, Infallible>(
            IterBodySource::new(chunks_data.into_iter()),
            16,
        )
        .expect("register chunked source");
    let service = runtime
        .register_with_capacity::<ChunkedService, Infallible>(ChunkedService { source }, 16)
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

    let response = read_through_real_rustls_path(ready.local_addr, root_cert, "/big-chunked");
    let separator = b"\r\n\r\n";
    let head_end = response
        .windows(separator.len())
        .position(|w| w == separator)
        .expect("response has CRLFCRLF");
    let head = std::str::from_utf8(&response[..head_end]).expect("head ASCII");
    assert!(
        head.contains("Transfer-Encoding: chunked\r\n"),
        "chunked-over-HTTPS must declare chunked framing; got {head:?}"
    );
    assert!(
        !head.to_lowercase().contains("content-length"),
        "chunked must not also emit Content-Length; got {head:?}"
    );
    let body_wire = &response[head_end + separator.len()..];
    let decoded = decode_chunked_body(body_wire).expect("chunked body decodes over TLS");
    assert_eq!(
        decoded, total_decoded,
        "decoded chunked body over HTTPS matches concatenated source chunks"
    );

    let _ = runtime.try_send(listener, HttpsListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(snap.drained(), "metrics drain on shutdown; got {snap:?}");
    assert_eq!(
        snap.response_body_high_water, largest_chunk,
        "chunked-over-HTTPS peak body equals the largest single chunk \
         (parity with the plain-HTTP chunked test)"
    );
    assert_eq!(
        snap.body_io_error_count, 0,
        "happy path: chunked-over-HTTPS records no IO errors"
    );
}

/// HTTPS parity for chunked *request* acceptance. A raw rustls client
/// sends a chunked POST; the server decodes it via the same streaming
/// pull model used for plain HTTP. The response echoes the total decoded
/// bytes so the test can verify end-to-end correctness.
#[derive(Debug, Clone)]
enum ChunkedRequestMsg {
    Inbound(HttpRequest),
    ChunkArrived(CallOutcome<RequestChunkReply>),
}

impl From<HttpRequest> for ChunkedRequestMsg {
    fn from(req: HttpRequest) -> Self {
        Self::Inbound(req)
    }
}

struct ChunkedRequestConsumer {
    pending_source: Option<Address<HttpConnectionMsg, RequestChunkReply>>,
    accumulated: Vec<u8>,
}

impl Isolate for ChunkedRequestConsumer {
    tina::isolate_types! {
        message: ChunkedRequestMsg,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<ChunkedRequestMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: ChunkedRequestMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ChunkedRequestMsg::Inbound(req) => match req.body {
                HttpRequestBody::Buffered(bytes) => reply(HttpResponse::with_text(
                    StatusCode::OK,
                    format!("buffered:{}", bytes.len()),
                )),
                HttpRequestBody::Stream(stream) => {
                    self.accumulated.clear();
                    self.pending_source = Some(stream.source);
                    call(
                        stream.source,
                        HttpConnectionMsg::body_next(),
                        Duration::from_secs(2),
                    )
                    .reply(ChunkedRequestMsg::ChunkArrived)
                }
            },
            ChunkedRequestMsg::ChunkArrived(outcome) => match outcome {
                CallOutcome::Replied(RequestChunkReply::Chunk(bytes)) => {
                    self.accumulated.extend_from_slice(&bytes);
                    let source = self.pending_source.expect("source set");
                    call(
                        source,
                        HttpConnectionMsg::body_next(),
                        Duration::from_secs(2),
                    )
                    .reply(ChunkedRequestMsg::ChunkArrived)
                }
                CallOutcome::Replied(RequestChunkReply::Eof) => {
                    let total = self.accumulated.len();
                    self.pending_source = None;
                    self.accumulated.clear();
                    reply(HttpResponse::with_text(
                        StatusCode::OK,
                        format!("stream:{total}"),
                    ))
                }
                CallOutcome::Replied(RequestChunkReply::Error(_))
                | CallOutcome::Full
                | CallOutcome::Closed
                | CallOutcome::Rejected(_)
                | CallOutcome::Timeout => {
                    self.pending_source = None;
                    reply(HttpResponse::internal_error())
                }
            },
        }
    }
}

fn send_chunked_request_over_rustls(
    addr: SocketAddr,
    root_cert_der: Vec<u8>,
    path: &str,
    chunked_body: &[u8],
) -> Vec<u8> {
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
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).expect("write head");
    stream.write_all(chunked_body).expect("write body");
    stream.flush().expect("flush");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response
}

#[test]
fn chunked_request_over_https_matches_http_shape() {
    let (identity, cert_der) = generate_identity();
    let metrics = BodyMetrics::new();

    let body: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let mut chunked_body = Vec::new();
    for chunk in body.chunks(1024) {
        chunked_body.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        chunked_body.extend_from_slice(chunk);
        chunked_body.extend_from_slice(b"\r\n");
    }
    chunked_body.extend_from_slice(b"0\r\n\r\n");

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let consumer = runtime
        .register_with_capacity::<ChunkedRequestConsumer, Infallible>(
            ChunkedRequestConsumer {
                pending_source: None,
                accumulated: Vec::new(),
            },
            32,
        )
        .expect("register consumer");

    let mut server_config = HttpsServerConfig::dev(identity);
    server_config.http.limits.inbound_stream_chunk_size = Some(512);
    let listener_isolate = HttpsListener::<TestShard, ChunkedRequestMsg>::new(
        "127.0.0.1:0".parse().unwrap(),
        consumer,
        server_config,
    )
    .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpsListener<TestShard, ChunkedRequestMsg>, _>(
            listener_isolate,
            8,
        )
        .expect("register listener");

    type StartOutcome = CallOutcome<Result<HttpsReady, HttpsStartupError>>;
    let outcome: StartOutcome = runtime
        .call_blocking(listener, HttpsListenerMsg::Start, Duration::from_secs(5))
        .expect("startup call runs");
    let ready = match outcome {
        CallOutcome::Replied(Ok(ready)) => ready,
        other => panic!("expected ready, got {other:?}"),
    };

    let response =
        send_chunked_request_over_rustls(ready.local_addr, cert_der, "/upload", &chunked_body);
    let text = std::str::from_utf8(&response).expect("utf8 response");
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "expected 200 OK, got: {text:?}"
    );
    assert!(
        text.ends_with(&format!("stream:{}", body.len())),
        "decoded body length must match original; got: {text:?}"
    );

    let _ = runtime.try_send(listener, HttpsListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(snap.drained(), "metrics drain on shutdown; got {snap:?}");
    assert!(
        snap.request_body_high_water > 0,
        "chunked request over HTTPS must charge body bytes"
    );
    assert_eq!(
        snap.body_io_error_count, 0,
        "happy path: no IO errors on chunked request over HTTPS"
    );
}
