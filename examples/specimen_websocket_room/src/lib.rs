use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use http::Method;
use tina::CallContext;
use tina::prelude::*;
use tina_http::{
    HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, WebSocketError, WebSocketLimits,
    WebSocketSessionMsg, WebSocketSessionOutcome, websocket_upgrade,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

#[derive(Debug, Default)]
pub struct RoomReport {
    pub text_echoed: bool,
    pub slow_peer_reported: bool,
}

#[derive(Debug, Default)]
struct DemoShard;

impl Shard for DemoShard {
    fn id(&self) -> ShardId {
        ShardId::new(987)
    }
}

#[derive(Debug)]
struct Gateway {
    room: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
    limits: WebSocketLimits,
}

impl Isolate for Gateway {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: DemoShard,
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, DemoShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.response_for(request))
    }

    fn handle_call(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.response_for(request))
    }
}

impl Gateway {
    fn response_for(&self, request: HttpRequest) -> HttpResponse {
        if request.method == Method::GET && request.path == "/room" {
            match websocket_upgrade(&request, self.limits) {
                Ok(upgrade) => HttpResponse::websocket(upgrade.accept(self.room, self.limits)),
                Err(_) => HttpResponse::bad_request(),
            }
        } else {
            HttpResponse::not_found()
        }
    }
}

#[derive(Debug)]
struct Room {
    joined: usize,
    fanout_limit: usize,
}

impl Isolate for Room {
    tina::isolate_types! {
        message: WebSocketSessionMsg,
        reply: WebSocketSessionOutcome,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: DemoShard,
    }

    fn handle(
        &mut self,
        msg: WebSocketSessionMsg,
        _ctx: &mut Context<'_, DemoShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.outcome_for(msg))
    }

    fn handle_call(
        &mut self,
        msg: WebSocketSessionMsg,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        call.reply(self.outcome_for(msg))
    }
}

impl Room {
    fn outcome_for(&mut self, msg: WebSocketSessionMsg) -> WebSocketSessionOutcome {
        match msg {
            WebSocketSessionMsg::Open => {
                self.joined += 1;
                WebSocketSessionOutcome::Text(format!("join:{}", self.joined))
            }
            WebSocketSessionMsg::Text(text) => {
                WebSocketSessionOutcome::Text(format!("room:{text}"))
            }
            WebSocketSessionMsg::Binary(bytes) => WebSocketSessionOutcome::Binary(bytes),
            WebSocketSessionMsg::Close(code, reason) => {
                WebSocketSessionOutcome::Close(code, reason)
            }
            WebSocketSessionMsg::Pressure(_) | WebSocketSessionMsg::Closed(_) => {
                WebSocketSessionOutcome::None
            }
            WebSocketSessionMsg::Ping(_) | WebSocketSessionMsg::Pong(_) => {
                WebSocketSessionOutcome::None
            }
        }
    }

    fn broadcast_pressure(&self, targets: usize) -> Result<(), WebSocketError> {
        if targets > self.fanout_limit {
            Err(WebSocketError::OutboundQueueFull)
        } else {
            Ok(())
        }
    }
}

pub fn run() -> RoomReport {
    let limits = WebSocketLimits {
        broadcast_fanout_max_targets: 1,
        ..Default::default()
    };
    let runtime = ThreadedRuntime::with_config(
        DemoShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let room = runtime
        .register_with_capacity::<Room, Infallible>(
            Room {
                joined: 0,
                fanout_limit: limits.broadcast_fanout_max_targets,
            },
            16,
        )
        .expect("register room");
    let gateway = runtime
        .register_with_capacity::<Gateway, Infallible>(Gateway { room, limits }, 16)
        .expect("register gateway");
    let listener_isolate = HttpListener::<DemoShard>::new(
        "127.0.0.1:0".parse().unwrap(),
        gateway,
        tina_http::HttpLimits::default(),
        Duration::from_secs(2),
        16,
    );
    let listener = runtime
        .register_with_capacity::<HttpListener<DemoShard>, Infallible>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .expect("start listener");
    let addr = bound.wait(Duration::from_secs(2)).expect("bound address");

    let text_echoed = drive_client(addr);
    let slow_peer_reported = Room {
        joined: 0,
        fanout_limit: limits.broadcast_fanout_max_targets,
    }
    .broadcast_pressure(2)
    .is_err();

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    RoomReport {
        text_echoed,
        slow_peer_reported,
    }
}

fn drive_client(addr: SocketAddr) -> bool {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    stream
        .write_all(
            b"GET /room HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        )
        .unwrap();
    read_head(&mut stream);
    let _join = read_frame(&mut stream);
    stream.write_all(&masked_frame(0x1, b"hello")).unwrap();
    read_frame(&mut stream) == (0x1, b"room:hello".to_vec())
}

fn read_head(stream: &mut TcpStream) {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut b).unwrap();
        head.push(b[0]);
    }
    assert!(String::from_utf8(head).unwrap().starts_with("HTTP/1.1 101"));
}

fn masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [1u8, 2, 3, 4];
    let mut out = vec![0x80 | opcode, 0x80 | payload.len() as u8];
    out.extend_from_slice(&mask);
    for (i, byte) in payload.iter().enumerate() {
        out.push(*byte ^ mask[i % 4]);
    }
    out
}

fn read_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).unwrap();
    let opcode = head[0] & 0x0f;
    let len = usize::from(head[1] & 0x7f);
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).unwrap();
    (opcode, payload)
}

#[cfg(test)]
mod tests {
    #[test]
    fn specimen_websocket_room_smoke() {
        let report = crate::run();
        assert!(report.text_echoed);
        assert!(report.slow_peer_reported);
    }
}
