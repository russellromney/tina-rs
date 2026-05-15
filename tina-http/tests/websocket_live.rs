use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use http::Method;
use rustls::pki_types::ServerName;
use tina::CallContext;
use tina::prelude::*;
use tina_http::{
    HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, HttpsListener, HttpsListenerMsg,
    HttpsReady, HttpsServerConfig, TlsServerIdentity, WebSocketCloseCode, WebSocketError,
    WebSocketLimits, WebSocketMessage, WebSocketSessionMsg, WebSocketSessionOutcome,
    websocket_upgrade,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
};

const TEST_IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(187)
    }
}

#[derive(Debug)]
struct Gateway {
    ws_app: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
    limits: WebSocketLimits,
}

impl Isolate for Gateway {
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
        reply(self.response_for(request))
    }

    fn handle_call(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.response_for(request))
    }
}

impl Gateway {
    fn response_for(&self, request: HttpRequest) -> HttpResponse {
        if request.method == Method::GET && request.path == "/ws" {
            match websocket_upgrade(&request, self.limits) {
                Ok(upgrade)
                    if upgrade
                        .offered_subprotocols()
                        .iter()
                        .any(|protocol| protocol == "chat.v1") =>
                {
                    match upgrade.accept_subprotocol(self.ws_app, self.limits, "chat.v1") {
                        Ok(accept) => HttpResponse::websocket(accept),
                        Err(_) => HttpResponse::bad_request(),
                    }
                }
                Ok(upgrade) => HttpResponse::websocket(upgrade.accept(self.ws_app, self.limits)),
                Err(_) => HttpResponse::bad_request(),
            }
        } else {
            HttpResponse::not_found()
        }
    }
}

#[derive(Debug, Default)]
struct WsEcho;

impl Isolate for WsEcho {
    tina::isolate_types! {
        message: WebSocketSessionMsg,
        reply: WebSocketSessionOutcome,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: WebSocketSessionMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(Self::outcome_for(msg))
    }

    fn handle_call(
        &mut self,
        msg: WebSocketSessionMsg,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        call.reply(Self::outcome_for(msg))
    }
}

impl WsEcho {
    fn outcome_for(msg: WebSocketSessionMsg) -> WebSocketSessionOutcome {
        match msg {
            WebSocketSessionMsg::Text(text) if text == "server-ping" => {
                WebSocketSessionOutcome::Many(vec![WebSocketMessage::Ping(b"alive".to_vec())])
            }
            WebSocketSessionMsg::Text(text) if text == "large-out" => {
                WebSocketSessionOutcome::Text("x".repeat(64))
            }
            WebSocketSessionMsg::Text(text) if text == "burst-out" => {
                WebSocketSessionOutcome::Many(vec![
                    WebSocketMessage::Text("one".to_owned()),
                    WebSocketMessage::Text("two".to_owned()),
                    WebSocketMessage::Text("three".to_owned()),
                ])
            }
            WebSocketSessionMsg::Text(text) if text == "server-close" => {
                WebSocketSessionOutcome::Close(Some(WebSocketCloseCode(1000)), b"bye".to_vec())
            }
            WebSocketSessionMsg::Text(text) => WebSocketSessionOutcome::Text(text),
            WebSocketSessionMsg::Binary(bytes) => WebSocketSessionOutcome::Binary(bytes),
            WebSocketSessionMsg::Pong(bytes) => {
                WebSocketSessionOutcome::Many(vec![WebSocketMessage::Text(format!(
                    "pong:{}",
                    bytes.len()
                ))])
            }
            WebSocketSessionMsg::Close(code, reason) => {
                WebSocketSessionOutcome::Close(code, reason)
            }
            WebSocketSessionMsg::Pressure(_) | WebSocketSessionMsg::Closed(_) => {
                WebSocketSessionOutcome::None
            }
            WebSocketSessionMsg::Open
            | WebSocketSessionMsg::SessionAccepted { .. }
            | WebSocketSessionMsg::SessionOpen { .. }
            | WebSocketSessionMsg::SessionText { .. }
            | WebSocketSessionMsg::SessionBinary { .. }
            | WebSocketSessionMsg::SessionClose { .. }
            | WebSocketSessionMsg::SessionPressure { .. }
            | WebSocketSessionMsg::SessionClosed { .. }
            | WebSocketSessionMsg::SessionReport(_)
            | WebSocketSessionMsg::SendOutcome(_)
            | WebSocketSessionMsg::Shutdown { .. }
            | WebSocketSessionMsg::Ping(_) => WebSocketSessionOutcome::None,
        }
    }
}

struct Harness {
    addr: SocketAddr,
    runtime: Option<ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>>,
    listener: Address<HttpListenerMsg>,
    _serial_guard: MutexGuard<'static, ()>,
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

fn rustls_client(
    addr: SocketAddr,
    root_cert_der: Vec<u8>,
) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(root_cert_der))
        .expect("add root");
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = ServerName::try_from("localhost").expect("server name");
    let connection =
        rustls::ClientConnection::new(Arc::new(config), server_name).expect("client connection");
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).expect("tcp connect");
    tcp.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    tcp.set_write_timeout(Some(Duration::from_secs(5)))
        .expect("write timeout");
    let mut stream = rustls::StreamOwned::new(connection, tcp);
    while stream.conn.is_handshaking() {
        stream
            .conn
            .complete_io(&mut stream.sock)
            .expect("client handshake");
    }
    stream
}

impl Harness {
    fn start(limits: WebSocketLimits) -> Self {
        let serial_guard = websocket_live_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let runtime = ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig {
                command_capacity: 64,
                idle_wait: Duration::from_millis(1),
                ..Default::default()
            },
        );
        let ws_app = runtime
            .register_with_capacity::<WsEcho, Infallible>(WsEcho, 16)
            .expect("register ws app");
        let gateway = runtime
            .register_with_capacity::<Gateway, Infallible>(Gateway { ws_app, limits }, 16)
            .expect("register gateway");
        let listener_isolate = HttpListener::<TestShard>::new(
            "127.0.0.1:0".parse().unwrap(),
            gateway,
            tina_http::HttpLimits::default(),
            Duration::from_secs(2),
            16,
        );
        let listener = runtime
            .register_with_capacity::<HttpListener<TestShard>, Infallible>(listener_isolate, 8)
            .expect("register listener");
        let bound = runtime.observe_next_bound();
        runtime
            .try_send(listener, HttpListenerMsg::Start)
            .expect("start listener");
        let addr = bound.wait(Duration::from_secs(2)).expect("bound addr");
        Self {
            addr,
            runtime: Some(runtime),
            listener,
            _serial_guard: serial_guard,
        }
    }
}

fn websocket_live_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            let _ = runtime.try_send(self.listener, HttpListenerMsg::Stop);
            let _ = runtime.shutdown();
        }
    }
}

fn connect_ws(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect_timeout(&addr, TEST_IO_TIMEOUT).expect("connect");
    stream
        .set_read_timeout(Some(TEST_IO_TIMEOUT))
        .expect("read timeout");
    write_upgrade(&mut stream);
    read_upgrade_response(&mut stream);
    stream
}

fn masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    masked_frame_with_first_byte(0x80 | opcode, payload)
}

fn masked_frame_with_first_byte(first: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [1u8, 2, 3, 4];
    let mut out = vec![first];
    if payload.len() < 126 {
        out.push(0x80 | payload.len() as u8);
    } else {
        out.push(0x80 | 126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        out.push(*b ^ mask[i % 4]);
    }
    out
}

fn masked_fragment(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let first = if fin { 0x80 | opcode } else { opcode };
    masked_frame_with_first_byte(first, payload)
}

fn unmasked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
    if payload.len() < 126 {
        out.push(payload.len() as u8);
    } else {
        out.push(126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

fn read_upgrade_head(stream: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    let mut b = [0u8; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut b).unwrap();
        bytes.push(b[0]);
    }
    String::from_utf8(bytes).unwrap()
}

fn read_server_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    read_server_frame_from(stream)
}

fn read_server_frame_from(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).expect("read frame head");
    assert_eq!(head[1] & 0x80, 0, "server frames must be unmasked");
    let opcode = head[0] & 0x0f;
    let mut len = usize::from(head[1] & 0x7f);
    if len == 126 {
        let mut wide = [0u8; 2];
        stream.read_exact(&mut wide).expect("read len16");
        len = usize::from(u16::from_be_bytes(wide));
    }
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).expect("read payload");
    (opcode, payload)
}

fn write_upgrade(stream: &mut impl Write) {
    stream
        .write_all(
            b"GET /ws HTTP/1.1\r\n\
              Host: x\r\n\
              Upgrade: websocket\r\n\
              Connection: Upgrade\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
              Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .expect("write upgrade");
}

fn read_upgrade_response(stream: &mut impl Read) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("read head");
        head.push(byte[0]);
    }
    let text = String::from_utf8(head).expect("utf8 head");
    assert!(text.starts_with("HTTP/1.1 101"), "{text}");
    assert!(
        text.to_ascii_lowercase()
            .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo=\r\n"),
        "{text}"
    );
}

#[test]
fn websocket_valid_upgrade_computes_accept_response() {
    let harness = Harness::start(WebSocketLimits::default());
    let _stream = connect_ws(harness.addr);
}

#[test]
fn websocket_bad_upgrade_headers_reject() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
}

#[test]
fn websocket_extension_offer_is_ignored() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n",
        )
        .unwrap();
    let head = read_upgrade_head(&mut stream);
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(
        !head
            .to_ascii_lowercase()
            .contains("sec-websocket-extensions"),
        "{head}"
    );
}

#[test]
fn websocket_subprotocol_selection_is_returned() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: chat.v0, chat.v1\r\n\r\n",
        )
        .unwrap();
    let head = read_upgrade_head(&mut stream);
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("sec-websocket-protocol: chat.v1"),
        "{head}"
    );
}

#[test]
fn websocket_invalid_subprotocol_offer_rejects() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: chat.v1, chat.v1\r\n\r\n",
        )
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
}

#[test]
fn websocket_text_and_binary_echo_work() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = connect_ws(harness.addr);
    stream.write_all(&masked_frame(0x1, b"hello")).unwrap();
    assert_eq!(read_server_frame(&mut stream), (0x1, b"hello".to_vec()));
    stream.write_all(&masked_frame(0x2, b"\x01\x02")).unwrap();
    assert_eq!(read_server_frame(&mut stream), (0x2, vec![1, 2]));
}

#[test]
fn websocket_fragmented_text_reassembles_with_interleaved_ping() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = connect_ws(harness.addr);
    stream
        .write_all(&masked_fragment(false, 0x1, b"hel"))
        .unwrap();
    stream.write_all(&masked_frame(0x9, b"mid")).unwrap();
    assert_eq!(read_server_frame(&mut stream), (0xA, b"mid".to_vec()));
    stream
        .write_all(&masked_fragment(true, 0x0, b"lo"))
        .unwrap();
    assert_eq!(read_server_frame(&mut stream), (0x1, b"hello".to_vec()));
}

#[test]
fn websocket_fragmented_binary_reassembles() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = connect_ws(harness.addr);
    stream
        .write_all(&masked_fragment(false, 0x2, &[1, 2]))
        .unwrap();
    stream
        .write_all(&masked_fragment(false, 0x0, &[3]))
        .unwrap();
    stream
        .write_all(&masked_fragment(true, 0x0, &[4, 5]))
        .unwrap();
    assert_eq!(read_server_frame(&mut stream), (0x2, vec![1, 2, 3, 4, 5]));
}

#[test]
fn websocket_tls_text_and_ping_work() {
    let GeneratedIdentity { identity, cert_der } = generate_identity();
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let limits = WebSocketLimits::default();
    let ws_app = runtime
        .register_with_capacity::<WsEcho, Infallible>(WsEcho, 16)
        .expect("register ws app");
    let gateway = runtime
        .register_with_capacity::<Gateway, Infallible>(Gateway { ws_app, limits }, 16)
        .expect("register gateway");
    let listener_isolate = HttpsListener::<TestShard>::new(
        "127.0.0.1:0".parse().unwrap(),
        gateway,
        HttpsServerConfig::dev(identity),
    );
    let listener = runtime
        .register_with_capacity::<HttpsListener<TestShard>, _>(listener_isolate, 8)
        .expect("register https listener");
    let ready = match runtime
        .call_blocking(listener, HttpsListenerMsg::Start, Duration::from_secs(5))
        .expect("start call runs")
    {
        CallOutcome::Replied(Ok(HttpsReady { local_addr })) => local_addr,
        other => panic!("expected TLS listener ready, got {other:?}"),
    };

    let mut stream = rustls_client(ready, cert_der);
    write_upgrade(&mut stream);
    stream.flush().expect("flush upgrade");
    read_upgrade_response(&mut stream);
    stream.write_all(&masked_frame(0x1, b"tls")).unwrap();
    stream.flush().unwrap();
    assert_eq!(read_server_frame_from(&mut stream), (0x1, b"tls".to_vec()));
    stream.write_all(&masked_frame(0x9, b"abc")).unwrap();
    stream.flush().unwrap();
    assert_eq!(read_server_frame_from(&mut stream), (0xA, b"abc".to_vec()));

    let _ = runtime.try_send(listener, HttpsListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn websocket_two_buffered_frames_do_not_wait_for_more_bytes() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = connect_ws(harness.addr);
    let mut frames = masked_frame(0x1, b"one");
    frames.extend_from_slice(&masked_frame(0x1, b"two"));
    stream.write_all(&frames).unwrap();
    assert_eq!(read_server_frame(&mut stream), (0x1, b"one".to_vec()));
    assert_eq!(read_server_frame(&mut stream), (0x1, b"two".to_vec()));
}

#[test]
fn websocket_unmasked_client_frame_rejects() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = connect_ws(harness.addr);
    stream.write_all(&unmasked_frame(0x1, b"bad")).unwrap();
    let (opcode, _) = read_server_frame(&mut stream);
    assert_eq!(opcode, 0x8);
}

#[test]
fn websocket_control_frame_rules_are_enforced() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut fragmented_ping = connect_ws(harness.addr);
    let mut bad = masked_frame(0x9, b"x");
    bad[0] &= !0x80;
    fragmented_ping.write_all(&bad).unwrap();
    assert_eq!(read_server_frame(&mut fragmented_ping).0, 0x8);

    let mut oversized_ping = connect_ws(harness.addr);
    oversized_ping
        .write_all(&masked_frame(0x9, &[b'x'; 126]))
        .unwrap();
    assert_eq!(read_server_frame(&mut oversized_ping).0, 0x8);
}

#[test]
fn websocket_malformed_fragmentation_rejects() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut continuation_without_start = connect_ws(harness.addr);
    continuation_without_start
        .write_all(&masked_fragment(true, 0x0, b"bad"))
        .unwrap();
    assert_eq!(read_server_frame(&mut continuation_without_start).0, 0x8);

    let mut nested_data_fragment = connect_ws(harness.addr);
    nested_data_fragment
        .write_all(&masked_fragment(false, 0x1, b"one"))
        .unwrap();
    nested_data_fragment
        .write_all(&masked_fragment(false, 0x2, b"two"))
        .unwrap();
    assert_eq!(read_server_frame(&mut nested_data_fragment).0, 0x8);
}

#[test]
fn websocket_fragmented_message_cap_is_enforced() {
    let limits = WebSocketLimits {
        max_frame_bytes: 64,
        max_message_bytes: 4,
        ..Default::default()
    };
    let harness = Harness::start(limits);
    let mut stream = connect_ws(harness.addr);
    stream
        .write_all(&masked_fragment(false, 0x1, b"ab"))
        .unwrap();
    stream
        .write_all(&masked_fragment(true, 0x0, b"cde"))
        .unwrap();
    assert_eq!(read_server_frame(&mut stream).0, 0x8);
}

#[test]
fn websocket_rsv_bits_reject_without_negotiated_extensions() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = connect_ws(harness.addr);
    stream
        .write_all(&masked_frame_with_first_byte(0x80 | 0x40 | 0x1, b"bad"))
        .unwrap();
    assert_eq!(read_server_frame(&mut stream).0, 0x8);
}

#[test]
fn websocket_ping_produces_pong_and_pong_is_visible() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = connect_ws(harness.addr);
    stream.write_all(&masked_frame(0x9, b"abc")).unwrap();
    assert_eq!(read_server_frame(&mut stream), (0xA, b"abc".to_vec()));
    stream.write_all(&masked_frame(0xA, b"ok")).unwrap();
    assert_eq!(read_server_frame(&mut stream), (0x1, b"pong:2".to_vec()));
}

#[test]
fn websocket_peer_close_gets_close_reply() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = connect_ws(harness.addr);
    let mut payload = WebSocketCloseCode(1000).0.to_be_bytes().to_vec();
    payload.extend_from_slice(b"bye");
    stream.write_all(&masked_frame(0x8, &payload)).unwrap();
    let (opcode, body) = read_server_frame(&mut stream);
    assert_eq!(opcode, 0x8);
    assert_eq!(body, payload);
}

#[test]
fn websocket_bad_close_payload_rejects() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = connect_ws(harness.addr);
    stream.write_all(&masked_frame(0x8, &[0x03])).unwrap();
    assert_eq!(read_server_frame(&mut stream).0, 0x8);
    drop(stream);

    let mut stream = connect_ws(harness.addr);
    let mut payload = 1000u16.to_be_bytes().to_vec();
    payload.push(0xff);
    stream.write_all(&masked_frame(0x8, &payload)).unwrap();
    assert_eq!(read_server_frame(&mut stream).0, 0x8);
}

#[test]
fn websocket_app_ping_timeout_closes() {
    let limits = WebSocketLimits {
        ping_pong_timeout: Duration::from_millis(20),
        close_handshake_timeout: Duration::from_millis(20),
        ..Default::default()
    };
    let harness = Harness::start(limits);
    let mut stream = connect_ws(harness.addr);
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    stream
        .write_all(&masked_frame(0x1, b"server-ping"))
        .unwrap();
    assert_eq!(read_server_frame(&mut stream), (0x9, b"alive".to_vec()));
    assert_eq!(read_server_frame(&mut stream).0, 0x8);
}

#[test]
fn websocket_close_handshake_timeout_closes_resource() {
    let limits = WebSocketLimits {
        close_handshake_timeout: Duration::from_millis(20),
        ..Default::default()
    };
    let harness = Harness::start(limits);
    let mut stream = connect_ws(harness.addr);
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    stream
        .write_all(&masked_frame(0x1, b"server-close"))
        .unwrap();
    assert_eq!(read_server_frame(&mut stream).0, 0x8);
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
    assert!(rest.is_empty());
}

#[test]
fn websocket_peer_fin_closes_resource() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = connect_ws(harness.addr);
    stream.shutdown(Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
}

#[test]
fn websocket_immediate_outbound_bytes_cap_is_visible_pressure() {
    let limits = WebSocketLimits {
        max_queued_outbound_bytes: 8,
        ..Default::default()
    };
    let harness = Harness::start(limits);
    let mut stream = connect_ws(harness.addr);
    stream.write_all(&masked_frame(0x1, b"large-out")).unwrap();
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
    assert!(rest.is_empty(), "large outbound frame must not be written");
}

#[test]
fn websocket_app_many_respects_frame_cap_before_writing() {
    let limits = WebSocketLimits {
        outbound_frame_queue_capacity: 1,
        max_queued_outbound_bytes: 1024,
        ..Default::default()
    };
    let harness = Harness::start(limits);
    let mut stream = connect_ws(harness.addr);
    stream.write_all(&masked_frame(0x1, b"burst-out")).unwrap();
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
    assert!(
        rest.is_empty(),
        "over-cap app burst must not partially write frames before closing"
    );
}

#[test]
fn websocket_read_buffer_high_water_caps_each_read() {
    let limits = WebSocketLimits {
        read_buffer_high_water: 2,
        max_frame_bytes: 64,
        max_message_bytes: 64,
        ..Default::default()
    };
    let harness = Harness::start(limits);
    let mut stream = connect_ws(harness.addr);
    stream.write_all(&masked_frame(0x1, b"hello")).unwrap();
    assert_eq!(read_server_frame(&mut stream).0, 0x8);
}

#[test]
fn websocket_oversized_frame_rejects_and_closes() {
    let limits = WebSocketLimits {
        max_frame_bytes: 2,
        max_message_bytes: 2,
        ..Default::default()
    };
    let harness = Harness::start(limits);
    let mut stream = connect_ws(harness.addr);
    stream.write_all(&masked_frame(0x1, b"boom")).unwrap();
    assert_eq!(read_server_frame(&mut stream).0, 0x8);
}

#[test]
fn websocket_outbound_queue_caps_are_visible() {
    let mut queue = tina_http::WebSocketOutboundQueue::new(1, 3);
    queue.push(vec![1, 2, 3]).unwrap();
    assert_eq!(queue.push(vec![4]), Err(WebSocketError::OutboundQueueFull));
    assert_eq!(queue.pop(), Some(vec![1, 2, 3]));
    assert_eq!(
        queue.push(vec![1, 2, 3, 4]),
        Err(WebSocketError::OutboundBytesFull)
    );
}
