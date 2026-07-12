use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use base64::Engine;
use http::Method;
use rustls::pki_types::ServerName;
use sha1::{Digest, Sha1};
use tina::CallContext;
use tina::prelude::*;
use tina_http::{
    HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, HttpsListener, HttpsListenerMsg,
    HttpsReady, HttpsServerConfig, TlsServerIdentity, TlsTrustRoots, WebSocketClientConnection,
    WebSocketClientEvent, WebSocketClientMsg, WebSocketClientReply, WebSocketCloseCode,
    WebSocketError, WebSocketLimits, WebSocketMessage, WebSocketSessionMsg,
    WebSocketSessionOutcome, WebSocketTarget, websocket_upgrade,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ProtocolFact, RuntimeEventKind, ThreadedRuntime,
    ThreadedRuntimeConfig, WebSocketCloseReason,
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
        io: Infallible,
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
        io: Infallible,
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
        // The protocol owner now delivers one session-rich event per wire
        // frame; wire text/binary/close arrive as `Session*`, not the legacy
        // `Text`/`Binary`/`Close`.
        match msg {
            WebSocketSessionMsg::SessionText { text, .. } if text == "server-ping" => {
                WebSocketSessionOutcome::Many(vec![WebSocketMessage::Ping(b"alive".to_vec())])
            }
            WebSocketSessionMsg::SessionText { text, .. } if text == "large-out" => {
                WebSocketSessionOutcome::Text("x".repeat(64))
            }
            WebSocketSessionMsg::SessionText { text, .. } if text == "burst-out" => {
                WebSocketSessionOutcome::Many(vec![
                    WebSocketMessage::Text("one".to_owned()),
                    WebSocketMessage::Text("two".to_owned()),
                    WebSocketMessage::Text("three".to_owned()),
                ])
            }
            WebSocketSessionMsg::SessionText { text, .. } if text == "server-close" => {
                WebSocketSessionOutcome::Close(Some(WebSocketCloseCode(1000)), b"bye".to_vec())
            }
            WebSocketSessionMsg::SessionText { text, .. } => WebSocketSessionOutcome::Text(text),
            WebSocketSessionMsg::SessionBinary { bytes, .. } => {
                WebSocketSessionOutcome::Binary(bytes)
            }
            WebSocketSessionMsg::Pong(bytes) => {
                WebSocketSessionOutcome::Many(vec![WebSocketMessage::Text(format!(
                    "pong:{}",
                    bytes.len()
                ))])
            }
            WebSocketSessionMsg::SessionClose { code, reason, .. } => {
                WebSocketSessionOutcome::Close(code, reason)
            }
            WebSocketSessionMsg::SessionPressure { .. }
            | WebSocketSessionMsg::SessionClosed { .. }
            | WebSocketSessionMsg::Pressure(_)
            | WebSocketSessionMsg::Closed(_)
            | WebSocketSessionMsg::Text(_)
            | WebSocketSessionMsg::Binary(_)
            | WebSocketSessionMsg::Close(_, _)
            | WebSocketSessionMsg::Open
            | WebSocketSessionMsg::SessionAccepted { .. }
            | WebSocketSessionMsg::SessionOpen { .. }
            | WebSocketSessionMsg::SessionReport(_)
            | WebSocketSessionMsg::SendOutcome(_)
            | WebSocketSessionMsg::AppControl(_)
            | WebSocketSessionMsg::Shutdown { .. }
            | WebSocketSessionMsg::Ping(_) => WebSocketSessionOutcome::None,
        }
    }
}

struct Harness {
    addr: SocketAddr,
    runtime: Option<ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>>,
    listener: tina_http::HttpListenerAddress,
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
        let bound = runtime
            .observe_next_bound()
            .expect("register bind observer");
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

fn protocol_facts(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
) -> Vec<ProtocolFact> {
    runtime
        .complete_trace()
        .expect("complete trace")
        .into_iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::FactObserved {
                fact: tina_runtime::RuntimeFact::Protocol(protocol),
            } => Some(protocol),
            RuntimeEventKind::FactObserved { .. } => None,
            _ => None,
        })
        .collect()
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
    unmasked_frame_with_first_byte(0x80 | opcode, payload)
}

fn unmasked_fragment(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let first = if fin { 0x80 | opcode } else { opcode };
    unmasked_frame_with_first_byte(first, payload)
}

fn unmasked_frame_with_first_byte(first: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![first];
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

fn websocket_accept_for_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

fn sec_websocket_key_from_request(request: &str) -> String {
    request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim().to_owned())
        })
        .expect("sec websocket key")
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
fn native_websocket_client_talks_to_native_server_and_reports_session() {
    let harness = Harness::start(WebSocketLimits::default());
    let runtime = harness.runtime.as_ref().expect("runtime");
    let client = runtime
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");

    match runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Connect {
                target: WebSocketTarget::ws(harness.addr, "localhost", "/ws"),
                subprotocols: Vec::new(),
            },
            Duration::from_secs(5),
        )
        .expect("connect call")
    {
        CallOutcome::Replied(WebSocketClientReply::Connected(Ok(connected))) => {
            assert_eq!(connected.selected_subprotocol, None);
        }
        other => panic!("expected connected, got {other:?}"),
    }

    match runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Send(WebSocketMessage::Text("hello-native-client".to_owned())),
            Duration::from_secs(5),
        )
        .expect("send call")
    {
        CallOutcome::Replied(WebSocketClientReply::Sent(Ok(()))) => {}
        other => panic!("expected sent, got {other:?}"),
    }

    match runtime
        .call_blocking(client, WebSocketClientMsg::Receive, Duration::from_secs(5))
        .expect("receive call")
    {
        CallOutcome::Replied(WebSocketClientReply::Event(Ok(WebSocketClientEvent::Text(text)))) => {
            assert_eq!(text, "hello-native-client");
        }
        other => panic!("expected echoed text, got {other:?}"),
    }

    match runtime
        .call_blocking(client, WebSocketClientMsg::Report, Duration::from_secs(5))
        .expect("report call")
    {
        CallOutcome::Replied(WebSocketClientReply::Report(report)) => {
            assert_eq!(report.state, tina_http::WebSocketClientState::Open);
            assert_eq!(report.sent_messages, 1);
            assert_eq!(report.received_messages, 1);
            assert_eq!(report.queued_outbound_bytes, 0);
        }
        other => panic!("expected report, got {other:?}"),
    }
}

#[test]
fn native_websocket_client_wss_talks_to_native_tls_server() {
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
    let addr = match runtime
        .call_blocking(listener, HttpsListenerMsg::Start, Duration::from_secs(5))
        .expect("start call runs")
    {
        CallOutcome::Replied(Ok(HttpsReady { local_addr })) => local_addr,
        other => panic!("expected TLS listener ready, got {other:?}"),
    };

    let client = runtime
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");
    match runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Connect {
                target: WebSocketTarget::wss(
                    addr,
                    "localhost",
                    "/ws",
                    "localhost",
                    TlsTrustRoots::from_der(vec![cert_der]),
                ),
                subprotocols: Vec::new(),
            },
            Duration::from_secs(5),
        )
        .expect("connect call")
    {
        CallOutcome::Replied(WebSocketClientReply::Connected(Ok(_))) => {}
        other => panic!("expected connected, got {other:?}"),
    }
    assert!(matches!(
        runtime
            .call_blocking(
                client,
                WebSocketClientMsg::Send(WebSocketMessage::Text("wss-native".to_owned())),
                Duration::from_secs(5),
            )
            .expect("send call"),
        CallOutcome::Replied(WebSocketClientReply::Sent(Ok(())))
    ));
    match runtime
        .call_blocking(client, WebSocketClientMsg::Receive, Duration::from_secs(5))
        .expect("receive call")
    {
        CallOutcome::Replied(WebSocketClientReply::Event(Ok(WebSocketClientEvent::Text(text)))) => {
            assert_eq!(text, "wss-native");
        }
        other => panic!("expected wss echo, got {other:?}"),
    }

    let _ = runtime.try_send(listener, HttpsListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn native_websocket_client_handles_ping_and_close() {
    let harness = Harness::start(WebSocketLimits::default());
    let runtime = harness.runtime.as_ref().expect("runtime");
    let client = runtime
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");
    let connected = runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Connect {
                target: WebSocketTarget::ws(harness.addr, "localhost", "/ws"),
                subprotocols: Vec::new(),
            },
            Duration::from_secs(5),
        )
        .expect("connect call");
    assert!(matches!(
        connected,
        CallOutcome::Replied(WebSocketClientReply::Connected(Ok(_)))
    ));

    let sent = runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Send(WebSocketMessage::Text("server-ping".to_owned())),
            Duration::from_secs(5),
        )
        .expect("send call");
    assert!(matches!(
        sent,
        CallOutcome::Replied(WebSocketClientReply::Sent(Ok(())))
    ));

    match runtime
        .call_blocking(client, WebSocketClientMsg::Receive, Duration::from_secs(5))
        .expect("receive ping")
    {
        CallOutcome::Replied(WebSocketClientReply::Event(Ok(WebSocketClientEvent::Ping(
            bytes,
        )))) => {
            assert_eq!(bytes, b"alive");
        }
        other => panic!("expected ping event, got {other:?}"),
    }
    match runtime
        .call_blocking(client, WebSocketClientMsg::Receive, Duration::from_secs(5))
        .expect("receive pong echo")
    {
        CallOutcome::Replied(WebSocketClientReply::Event(Ok(WebSocketClientEvent::Text(text)))) => {
            assert_eq!(text, "pong:5");
        }
        other => panic!("expected pong echo, got {other:?}"),
    }

    let sent = runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Send(WebSocketMessage::Text("server-close".to_owned())),
            Duration::from_secs(5),
        )
        .expect("send close trigger");
    assert!(matches!(
        sent,
        CallOutcome::Replied(WebSocketClientReply::Sent(Ok(())))
    ));
    match runtime
        .call_blocking(client, WebSocketClientMsg::Receive, Duration::from_secs(5))
        .expect("receive close")
    {
        CallOutcome::Replied(WebSocketClientReply::Event(Ok(WebSocketClientEvent::Close {
            code,
            reason,
        }))) => {
            assert_eq!(code, Some(WebSocketCloseCode(1000)));
            assert_eq!(reason, b"bye");
        }
        other => panic!("expected close event, got {other:?}"),
    }
    std::thread::sleep(Duration::from_millis(50));
    match runtime
        .call_blocking(client, WebSocketClientMsg::Report, Duration::from_secs(5))
        .expect("report after close")
    {
        CallOutcome::Replied(WebSocketClientReply::Report(report)) => {
            assert_eq!(report.state, tina_http::WebSocketClientState::Closed);
            assert!(report.close_sent);
            assert!(report.close_received);
        }
        other => panic!("expected report after close, got {other:?}"),
    }
}

#[test]
fn native_websocket_client_user_close_updates_report_and_rejects_late_send() {
    let harness = Harness::start(WebSocketLimits::default());
    let runtime = harness.runtime.as_ref().expect("runtime");
    let client = runtime
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");
    assert!(matches!(
        runtime
            .call_blocking(
                client,
                WebSocketClientMsg::Connect {
                    target: WebSocketTarget::ws(harness.addr, "localhost", "/ws"),
                    subprotocols: Vec::new(),
                },
                Duration::from_secs(5),
            )
            .expect("connect call"),
        CallOutcome::Replied(WebSocketClientReply::Connected(Ok(_)))
    ));
    assert!(matches!(
        runtime
            .call_blocking(
                client,
                WebSocketClientMsg::Send(WebSocketMessage::Close(
                    Some(WebSocketCloseCode(1000)),
                    b"done".to_vec(),
                )),
                Duration::from_secs(5),
            )
            .expect("send close"),
        CallOutcome::Replied(WebSocketClientReply::Sent(Ok(())))
    ));
    match runtime
        .call_blocking(client, WebSocketClientMsg::Report, Duration::from_secs(5))
        .expect("report")
    {
        CallOutcome::Replied(WebSocketClientReply::Report(report)) => {
            assert!(
                matches!(
                    report.state,
                    tina_http::WebSocketClientState::Closing
                        | tina_http::WebSocketClientState::Closed
                ),
                "expected closing or closed after local close, got {:?}",
                report.state
            );
            assert!(report.close_sent);
            assert_eq!(report.sent_messages, 1);
        }
        other => panic!("expected report, got {other:?}"),
    }
    std::thread::sleep(Duration::from_millis(50));
    match runtime
        .call_blocking(client, WebSocketClientMsg::Report, Duration::from_secs(5))
        .expect("final report")
    {
        CallOutcome::Replied(WebSocketClientReply::Report(report)) => {
            assert_eq!(report.state, tina_http::WebSocketClientState::Closed);
            assert!(report.close_sent);
        }
        other => panic!("expected final report, got {other:?}"),
    }
    match runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Send(WebSocketMessage::Text("late".to_owned())),
            Duration::from_secs(5),
        )
        .expect("late send")
    {
        CallOutcome::Replied(WebSocketClientReply::Sent(Err(
            tina_http::WebSocketClientError::Closed,
        ))) => {}
        other => panic!("expected closed late send rejection, got {other:?}"),
    }
}

#[test]
fn native_websocket_client_invalid_target_rejects_before_connecting() {
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
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");
    match runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Connect {
                target: WebSocketTarget::ws(
                    "127.0.0.1:9".parse().unwrap(),
                    "localhost\r\nX-Bad: yes",
                    "/ws",
                ),
                subprotocols: Vec::new(),
            },
            Duration::from_secs(5),
        )
        .expect("connect call")
    {
        CallOutcome::Replied(WebSocketClientReply::Connected(Err(
            tina_http::WebSocketClientError::InvalidTarget,
        ))) => {}
        other => panic!("expected invalid target, got {other:?}"),
    }
    match runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Connect {
                target: WebSocketTarget::ws("127.0.0.1:9".parse().unwrap(), "localhost", "/ws"),
                subprotocols: vec!["bad protocol".to_owned()],
            },
            Duration::from_secs(5),
        )
        .expect("connect call")
    {
        CallOutcome::Replied(WebSocketClientReply::Connected(Err(
            tina_http::WebSocketClientError::InvalidTarget,
        ))) => {}
        other => panic!("expected invalid subprotocol, got {other:?}"),
    }
    let _ = runtime.shutdown();
}

#[test]
fn native_websocket_client_receive_before_connect_is_typed_not_connected() {
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
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");
    match runtime
        .call_blocking(client, WebSocketClientMsg::Receive, Duration::from_secs(5))
        .expect("receive before connect")
    {
        CallOutcome::Replied(WebSocketClientReply::Event(Err(
            tina_http::WebSocketClientError::NotConnected,
        ))) => {}
        other => panic!("expected typed not-connected receive, got {other:?}"),
    }
    let _ = runtime.shutdown();
}

#[test]
fn native_websocket_client_connect_failure_returns_typed_error_not_closed_call() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve addr");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
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
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");
    match runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Connect {
                target: WebSocketTarget::ws(addr, "localhost", "/ws"),
                subprotocols: Vec::new(),
            },
            Duration::from_secs(5),
        )
        .expect("connect call")
    {
        CallOutcome::Replied(WebSocketClientReply::Connected(Err(
            tina_http::WebSocketClientError::Connect,
        ))) => {}
        other => panic!("expected typed connect error, got {other:?}"),
    }
    match runtime
        .call_blocking(client, WebSocketClientMsg::Report, Duration::from_secs(5))
        .expect("report")
    {
        CallOutcome::Replied(WebSocketClientReply::Report(report)) => {
            assert_eq!(report.state, tina_http::WebSocketClientState::Closed);
        }
        other => panic!("expected report after failed connect, got {other:?}"),
    }
    let _ = runtime.shutdown();
}

#[test]
fn native_websocket_client_bad_peer_masked_server_frame_is_protocol_error() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind bad peer");
    let addr = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let request = read_upgrade_head(&mut stream);
        let key = sec_websocket_key_from_request(&request);
        let accept = websocket_accept_for_key(&key);
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .expect("write upgrade");
        stream
            .write_all(&masked_frame(0x1, b"server-must-not-mask"))
            .expect("write masked server frame");
    });

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
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");
    assert!(matches!(
        runtime
            .call_blocking(
                client,
                WebSocketClientMsg::Connect {
                    target: WebSocketTarget::ws(addr, "localhost", "/ws"),
                    subprotocols: Vec::new(),
                },
                Duration::from_secs(5),
            )
            .expect("connect call"),
        CallOutcome::Replied(WebSocketClientReply::Connected(Ok(_)))
    ));
    match runtime
        .call_blocking(client, WebSocketClientMsg::Receive, Duration::from_secs(5))
        .expect("receive protocol close")
    {
        CallOutcome::Replied(WebSocketClientReply::Event(Ok(WebSocketClientEvent::Close {
            code,
            reason,
        }))) => {
            assert_eq!(code, Some(WebSocketCloseCode(1002)));
            assert!(
                String::from_utf8(reason)
                    .expect("reason utf8")
                    .contains("ServerFrameMasked")
            );
        }
        other => panic!("expected protocol close event, got {other:?}"),
    }
    match runtime
        .call_blocking(client, WebSocketClientMsg::Report, Duration::from_secs(5))
        .expect("report")
    {
        CallOutcome::Replied(WebSocketClientReply::Report(report)) => {
            assert_eq!(report.protocol_errors, 1);
            assert_eq!(report.state, tina_http::WebSocketClientState::Closed);
        }
        other => panic!("expected report, got {other:?}"),
    }
    server.join().expect("bad peer thread");
    let _ = runtime.shutdown();
}

#[test]
fn native_websocket_client_reassembles_fragmented_text_with_interleaved_ping() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fragmenting peer");
    let addr = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let request = read_upgrade_head(&mut stream);
        let key = sec_websocket_key_from_request(&request);
        let accept = websocket_accept_for_key(&key);
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .expect("write upgrade");
        stream
            .write_all(&unmasked_fragment(false, 0x1, b"hel"))
            .expect("write first text fragment");
        stream
            .write_all(&unmasked_frame(0x9, b"mid"))
            .expect("write interleaved ping");
        stream
            .write_all(&unmasked_fragment(false, 0x0, b"lo"))
            .expect("write continuation");
        stream
            .write_all(&unmasked_fragment(true, 0x0, b"!"))
            .expect("write final continuation");
        std::thread::sleep(Duration::from_millis(100));
    });

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
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");
    assert!(matches!(
        runtime
            .call_blocking(
                client,
                WebSocketClientMsg::Connect {
                    target: WebSocketTarget::ws(addr, "localhost", "/ws"),
                    subprotocols: Vec::new(),
                },
                Duration::from_secs(5),
            )
            .expect("connect call"),
        CallOutcome::Replied(WebSocketClientReply::Connected(Ok(_)))
    ));
    match runtime
        .call_blocking(client, WebSocketClientMsg::Receive, Duration::from_secs(5))
        .expect("receive ping")
    {
        CallOutcome::Replied(WebSocketClientReply::Event(Ok(WebSocketClientEvent::Ping(
            bytes,
        )))) => {
            assert_eq!(bytes, b"mid");
        }
        other => panic!("expected interleaved ping event, got {other:?}"),
    }
    match runtime
        .call_blocking(client, WebSocketClientMsg::Receive, Duration::from_secs(5))
        .expect("receive text")
    {
        CallOutcome::Replied(WebSocketClientReply::Event(Ok(WebSocketClientEvent::Text(text)))) => {
            assert_eq!(text, "hello!");
        }
        other => panic!("expected one reassembled text event, got {other:?}"),
    }
    server.join().expect("fragmenting peer thread");
    let _ = runtime.shutdown();
}

#[test]
fn native_websocket_client_rejects_oversized_fragmented_message() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind oversized peer");
    let addr = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let request = read_upgrade_head(&mut stream);
        let key = sec_websocket_key_from_request(&request);
        let accept = websocket_accept_for_key(&key);
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .expect("write upgrade");
        stream
            .write_all(&unmasked_fragment(false, 0x1, b"hel"))
            .expect("write first fragment");
        stream
            .write_all(&unmasked_fragment(true, 0x0, b"lo"))
            .expect("write oversized continuation");
        std::thread::sleep(Duration::from_millis(100));
    });

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let limits = WebSocketLimits {
        max_message_bytes: 4,
        ..WebSocketLimits::default()
    };
    let client = runtime
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(limits),
            16,
        )
        .expect("register websocket client");
    assert!(matches!(
        runtime
            .call_blocking(
                client,
                WebSocketClientMsg::Connect {
                    target: WebSocketTarget::ws(addr, "localhost", "/ws"),
                    subprotocols: Vec::new(),
                },
                Duration::from_secs(5),
            )
            .expect("connect call"),
        CallOutcome::Replied(WebSocketClientReply::Connected(Ok(_)))
    ));
    match runtime
        .call_blocking(client, WebSocketClientMsg::Receive, Duration::from_secs(5))
        .expect("receive close")
    {
        CallOutcome::Replied(WebSocketClientReply::Event(Ok(WebSocketClientEvent::Close {
            code,
            reason,
        }))) => {
            assert_eq!(code, Some(WebSocketCloseCode(1002)));
            assert!(
                String::from_utf8(reason)
                    .expect("reason utf8")
                    .contains("MessageTooLarge")
            );
        }
        other => panic!("expected protocol close event, got {other:?}"),
    }
    server.join().expect("oversized peer thread");
    let _ = runtime.shutdown();
}

#[test]
fn native_websocket_client_bad_handshake_requires_upgrade_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind bad peer");
    let addr = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let request = read_upgrade_head(&mut stream);
        let key = sec_websocket_key_from_request(&request);
        let accept = websocket_accept_for_key(&key);
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .expect("write bad upgrade");
    });

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
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");
    match runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Connect {
                target: WebSocketTarget::ws(addr, "localhost", "/ws"),
                subprotocols: Vec::new(),
            },
            Duration::from_secs(5),
        )
        .expect("connect call")
    {
        CallOutcome::Replied(WebSocketClientReply::Connected(Err(
            tina_http::WebSocketClientError::BadHandshake,
        ))) => {}
        other => panic!("expected bad handshake, got {other:?}"),
    }
    server.join().expect("bad peer thread");
    let _ = runtime.shutdown();
}

#[test]
fn native_websocket_client_rejects_unoffered_server_subprotocol() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind bad peer");
    let addr = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let request = read_upgrade_head(&mut stream);
        let key = sec_websocket_key_from_request(&request);
        let accept = websocket_accept_for_key(&key);
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             Sec-WebSocket-Protocol: other.v1\r\n\r\n"
        )
        .expect("write bad subprotocol upgrade");
    });

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
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");
    match runtime
        .call_blocking(
            client,
            WebSocketClientMsg::Connect {
                target: WebSocketTarget::ws(addr, "localhost", "/ws"),
                subprotocols: vec!["chat.v1".to_owned()],
            },
            Duration::from_secs(5),
        )
        .expect("connect call")
    {
        CallOutcome::Replied(WebSocketClientReply::Connected(Err(
            tina_http::WebSocketClientError::UnsupportedSubprotocol,
        ))) => {}
        other => panic!("expected unsupported subprotocol, got {other:?}"),
    }
    server.join().expect("bad peer thread");
    let _ = runtime.shutdown();
}

#[test]
fn native_websocket_client_bad_close_payload_is_protocol_error() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind bad peer");
    let addr = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let request = read_upgrade_head(&mut stream);
        let key = sec_websocket_key_from_request(&request);
        let accept = websocket_accept_for_key(&key);
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .expect("write upgrade");
        stream
            .write_all(&unmasked_frame(0x8, &[0x03]))
            .expect("write bad close");
    });

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
        .register_with_capacity::<WebSocketClientConnection<TestShard>, Infallible>(
            WebSocketClientConnection::new(WebSocketLimits::default()),
            16,
        )
        .expect("register websocket client");
    assert!(matches!(
        runtime
            .call_blocking(
                client,
                WebSocketClientMsg::Connect {
                    target: WebSocketTarget::ws(addr, "localhost", "/ws"),
                    subprotocols: Vec::new(),
                },
                Duration::from_secs(5),
            )
            .expect("connect call"),
        CallOutcome::Replied(WebSocketClientReply::Connected(Ok(_)))
    ));
    match runtime
        .call_blocking(client, WebSocketClientMsg::Receive, Duration::from_secs(5))
        .expect("receive protocol close")
    {
        CallOutcome::Replied(WebSocketClientReply::Event(Ok(WebSocketClientEvent::Close {
            code,
            reason,
        }))) => {
            assert_eq!(code, Some(WebSocketCloseCode(1002)));
            assert!(
                String::from_utf8(reason)
                    .expect("reason utf8")
                    .contains("InvalidClosePayload")
            );
        }
        other => panic!("expected protocol close event, got {other:?}"),
    }
    server.join().expect("bad peer thread");
    let _ = runtime.shutdown();
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
fn websocket_fragmented_text_invalid_utf8_rejects_before_app_delivery() {
    let harness = Harness::start(WebSocketLimits::default());
    let mut stream = connect_ws(harness.addr);
    stream
        .write_all(&masked_fragment(false, 0x1, &[0xc3]))
        .unwrap();
    stream
        .write_all(&masked_fragment(true, 0x0, &[0x28]))
        .unwrap();
    assert_eq!(
        read_server_frame(&mut stream).0,
        0x8,
        "invalid UTF-8 across text fragments must close, not echo"
    );
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
    {
        let harness = Harness::start(WebSocketLimits::default());
        let mut stream = connect_ws(harness.addr);
        stream
            .write_all(&masked_fragment(true, 0x0, b"bad"))
            .unwrap();
        assert_eq!(read_server_frame(&mut stream).0, 0x8);
    }

    {
        let harness = Harness::start(WebSocketLimits::default());
        let mut stream = connect_ws(harness.addr);
        stream
            .write_all(&masked_fragment(false, 0x1, b"one"))
            .unwrap();
        stream
            .write_all(&masked_fragment(false, 0x2, b"two"))
            .unwrap();
        assert_eq!(read_server_frame(&mut stream).0, 0x8);
    }

    {
        let harness = Harness::start(WebSocketLimits::default());
        let mut stream = connect_ws(harness.addr);
        stream
            .write_all(&masked_fragment(false, 0x1, b"hel"))
            .unwrap();
        stream.write_all(&masked_frame(0x1, b"new")).unwrap();
        assert_eq!(
            read_server_frame(&mut stream).0,
            0x8,
            "new data frame during an open fragmented message must protocol-close"
        );
    }
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

    let facts = protocol_facts(harness.runtime.as_ref().expect("runtime"));
    assert!(
        facts.iter().any(|fact| matches!(
            fact,
            ProtocolFact::WebSocketSessionClosed {
                reason: WebSocketCloseReason::Normal,
                code: Some(1000),
                ..
            }
        )),
        "expected normal WebSocket close fact, got {facts:?}",
    );
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

    let facts = protocol_facts(harness.runtime.as_ref().expect("runtime"));
    assert!(
        facts.iter().any(|fact| matches!(
            fact,
            ProtocolFact::WebSocketSessionClosed {
                reason: WebSocketCloseReason::GoingAway,
                code: None,
                ..
            }
        )),
        "expected peer-FIN WebSocket close fact, got {facts:?}",
    );
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
