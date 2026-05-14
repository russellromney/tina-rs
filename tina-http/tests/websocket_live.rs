use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use http::Method;
use tina::CallContext;
use tina::prelude::*;
use tina_http::{
    HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, WebSocketCloseCode, WebSocketError,
    WebSocketLimits, WebSocketMessage, WebSocketSessionMsg, WebSocketSessionOutcome,
    websocket_upgrade,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

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
            WebSocketSessionMsg::Open | WebSocketSessionMsg::Ping(_) => {
                WebSocketSessionOutcome::None
            }
        }
    }
}

struct Harness {
    addr: SocketAddr,
    runtime: Option<ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>>,
    listener: Address<HttpListenerMsg>,
}

impl Harness {
    fn start(limits: WebSocketLimits) -> Self {
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
        }
    }
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
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
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
    stream
}

fn masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [1u8, 2, 3, 4];
    let mut out = vec![0x80 | opcode];
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

fn read_server_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
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
fn websocket_unsupported_extension_rejects() {
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
