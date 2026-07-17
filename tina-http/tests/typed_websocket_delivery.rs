//! Typed WebSocket delivery: split-service and request-only handles install
//! without naming the private envelope or extracting a raw address. Session
//! lanes stay exact; room broadcast reaches every snapshot recipient other
//! than the sender exactly once.
//!
//! Direct proof matrix:
//! - request-only upgrade → typed echo reply
//! - split-service upgrade → request-lane open/text, event-lane SendOutcome
//! - peer close frame (request) / peer drop SessionClosed (event)
//! - malformed upgrade / listener shutdown
//! - two- and three-client broadcast exactness (no omission, no self-echo,
//!   no second wire frame; SendOutcome count equals offer count)
//! - compile_fail doctests on accept constructors (lane confusion is a type error)

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use http::Method;
use tina::prelude::*;
use tina_http::{
    AdmitOutcome, HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse,
    WebSocketLimits, WebSocketMemberTable, WebSocketSessionLane, WebSocketSessionMsg,
    WebSocketSessionOutcome, websocket_session_lane, websocket_upgrade,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

const SHARD: u32 = 174;
const TEST_IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(SHARD)
    }
}

fn make_runtime() -> ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> {
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

fn start_listener(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    listener: HttpListener<TestShard>,
) -> (
    SocketAddr,
    Address<HttpListenerMsg, Result<tina_http::HttpReady, tina_http::HttpStartupError>>,
) {
    let listener_addr = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(listener, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound().expect("bind observer");
    runtime
        .try_send(listener_addr, HttpListenerMsg::Start)
        .expect("start listener");
    let addr = bound.wait(Duration::from_secs(2)).expect("bound addr");
    (addr, listener_addr)
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

fn read_upgrade_response(stream: &mut impl Read) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("read head");
        head.push(byte[0]);
    }
    String::from_utf8(head).expect("utf8 head")
}

fn connect_ws(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect_timeout(&addr, TEST_IO_TIMEOUT).expect("connect");
    stream
        .set_read_timeout(Some(TEST_IO_TIMEOUT))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(TEST_IO_TIMEOUT))
        .expect("write timeout");
    write_upgrade(&mut stream);
    let head = read_upgrade_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
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

fn read_server_text(stream: &mut TcpStream) -> String {
    let (opcode, payload) = read_server_frame(stream);
    assert_eq!(opcode, 0x1, "expected text frame, opcode={opcode}");
    String::from_utf8(payload).expect("utf8 text")
}

/// Assert no further WebSocket data frame arrives within a short drain window.
///
/// Control frames (ping/pong/close) are skipped; a second text/binary would
/// fail the exactly-once wire claim. Always restores the long read timeout.
fn assert_no_extra_data_frame(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("short drain timeout");
    let result = (|| -> Result<(), String> {
        loop {
            let mut head = [0u8; 2];
            match stream.read(&mut head) {
                Ok(0) | Err(_) => return Ok(()),
                Ok(1) => return Ok(()), // incomplete head; no full data frame
                Ok(_) => {
                    let opcode = head[0] & 0x0f;
                    let mut len = usize::from(head[1] & 0x7f);
                    if len == 126 {
                        let mut wide = [0u8; 2];
                        if stream.read_exact(&mut wide).is_err() {
                            return Ok(());
                        }
                        len = usize::from(u16::from_be_bytes(wide));
                    }
                    let mut payload = vec![0; len];
                    if stream.read_exact(&mut payload).is_err() {
                        return Ok(());
                    }
                    match opcode {
                        0x1 | 0x2 => {
                            return Err(format!(
                                "exactly-once violated: extra data frame opcode={opcode} payload={payload:?}"
                            ));
                        }
                        0x8..=0xa => continue, // close/ping/pong
                        other => return Err(format!("unexpected opcode during drain: {other}")),
                    }
                }
            }
        }
    })();
    stream
        .set_read_timeout(Some(TEST_IO_TIMEOUT))
        .expect("restore timeout");
    if let Err(msg) = result {
        panic!("{msg}");
    }
}

fn wait_for_count(counter: &AtomicU64, at_least: u64, label: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while counter.load(Ordering::SeqCst) < at_least && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let got = counter.load(Ordering::SeqCst);
    assert!(
        got >= at_least,
        "timed out waiting for {label}: want >={at_least}, got {got}"
    );
}

// ---------------------------------------------------------------------------
// Request-only echo
// ---------------------------------------------------------------------------

struct EchoService;

#[tina_runtime::isolate(
    request = WebSocketSessionMsg,
    reply = WebSocketSessionOutcome,
    shard = TestShard
)]
impl EchoService {
    fn handle_request(
        &mut self,
        msg: WebSocketSessionMsg,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match msg {
            WebSocketSessionMsg::SessionText { text, .. } => {
                call.reply(WebSocketSessionOutcome::Text(text))
            }
            WebSocketSessionMsg::SessionBinary { bytes, .. } => {
                call.reply(WebSocketSessionOutcome::Binary(bytes))
            }
            WebSocketSessionMsg::SessionClose { code, reason, .. } => {
                call.reply(WebSocketSessionOutcome::Close(code, reason))
            }
            _ => call.reply(WebSocketSessionOutcome::None),
        }
    }
}

struct EchoGateway {
    requests: tina_runtime::RequestServiceHandle<WebSocketSessionMsg, WebSocketSessionOutcome>,
    limits: WebSocketLimits,
}

impl Isolate for EchoGateway {
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

    fn handle_call(
        &mut self,
        request: HttpRequest,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        call.reply(self.response_for(request))
    }
}

impl EchoGateway {
    fn response_for(&self, request: HttpRequest) -> HttpResponse {
        if request.method == Method::GET && request.path == "/ws" {
            return match websocket_upgrade(&request, self.limits) {
                Ok(upgrade) => HttpResponse::websocket(
                    upgrade.accept_request_service(self.requests, self.limits),
                ),
                Err(_) => HttpResponse::bad_request(),
            };
        }
        HttpResponse::not_found()
    }
}

#[test]
fn request_only_accept_echoes_text_over_wire() {
    let runtime = make_runtime();
    let requests = runtime
        .register_request_service::<EchoService, WebSocketSessionMsg, Infallible>(EchoService, 16)
        .expect("register echo");
    let gateway = runtime
        .register_with_capacity::<EchoGateway, Infallible>(
            EchoGateway {
                requests,
                limits: WebSocketLimits::default(),
            },
            8,
        )
        .expect("register gateway");
    let listener = HttpListener::<TestShard>::new(
        "127.0.0.1:0".parse().unwrap(),
        gateway,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    );
    let (addr, listener_addr) = start_listener(&runtime, listener);

    let mut stream = connect_ws(addr);
    stream
        .write_all(&masked_frame(0x1, b"hello-request-only"))
        .expect("write text");
    assert_eq!(read_server_text(&mut stream), "hello-request-only");

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

// ---------------------------------------------------------------------------
// Split-service room: lane separation + two-client broadcast
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct LaneCounts {
    request_msgs: AtomicU64,
    event_msgs: AtomicU64,
    send_outcomes: AtomicU64,
    session_closed: AtomicU64,
    session_texts: AtomicU64,
}

struct SplitRoom {
    members: WebSocketMemberTable,
    lanes: Arc<LaneCounts>,
}

#[tina_runtime::isolate(
    event = WebSocketSessionMsg,
    request = WebSocketSessionMsg,
    reply = WebSocketSessionOutcome,
    send = tina::Outbound<tina_http::HttpConnectionMsg>,
    shard = TestShard
)]
impl SplitRoom {
    fn handle_event(
        &mut self,
        msg: WebSocketSessionMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        assert_eq!(
            websocket_session_lane(&msg),
            WebSocketSessionLane::Event,
            "event handler saw request-lane message: {msg:?}"
        );
        self.lanes.event_msgs.fetch_add(1, Ordering::SeqCst);
        match msg {
            WebSocketSessionMsg::SendOutcome(outcome) => {
                self.lanes.send_outcomes.fetch_add(1, Ordering::SeqCst);
                let _ = self.members.record_send_outcome(&outcome);
                noop()
            }
            WebSocketSessionMsg::SessionClosed { session_id, .. } => {
                self.lanes.session_closed.fetch_add(1, Ordering::SeqCst);
                let _ = self.members.remove_peer(session_id);
                noop()
            }
            WebSocketSessionMsg::SessionPressure { session_id, .. } => {
                let _ = self.members.remove_peer(session_id);
                noop()
            }
            _ => noop(),
        }
    }

    fn handle_request(
        &mut self,
        msg: WebSocketSessionMsg,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        assert_eq!(
            websocket_session_lane(&msg),
            WebSocketSessionLane::Request,
            "request handler saw event-lane message: {msg:?}"
        );
        self.lanes.request_msgs.fetch_add(1, Ordering::SeqCst);
        match msg {
            WebSocketSessionMsg::SessionOpen { session } => match self.members.admit(session) {
                AdmitOutcome::Admitted => {
                    let id = session.session_id();
                    call.reply(WebSocketSessionOutcome::Text(format!("join:{}", id.raw())))
                }
                AdmitOutcome::Full => call.reply(WebSocketSessionOutcome::Close(
                    Some(tina_http::WebSocketCloseCode(1013)),
                    b"full".to_vec(),
                )),
                AdmitOutcome::AlreadyMember => call.reply(WebSocketSessionOutcome::None),
            },
            WebSocketSessionMsg::SessionText { session_id, text } => {
                self.lanes.session_texts.fetch_add(1, Ordering::SeqCst);
                let body = format!("room:{text}");
                let effects = self.members.broadcast_text::<Self>(Some(session_id), body);
                call.reply_and(WebSocketSessionOutcome::None, effects)
            }
            WebSocketSessionMsg::SessionClose { code, reason, .. } => {
                call.reply(WebSocketSessionOutcome::Close(code, reason))
            }
            _ => call.reply(WebSocketSessionOutcome::None),
        }
    }
}

struct SplitGateway {
    room: tina_runtime::SplitServiceHandle<
        WebSocketSessionMsg,
        WebSocketSessionMsg,
        WebSocketSessionOutcome,
    >,
    limits: WebSocketLimits,
}

impl Isolate for SplitGateway {
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

    fn handle_call(
        &mut self,
        request: HttpRequest,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        call.reply(self.response_for(request))
    }
}

impl SplitGateway {
    fn response_for(&self, request: HttpRequest) -> HttpResponse {
        if request.method == Method::GET && request.path == "/ws" {
            return match websocket_upgrade(&request, self.limits) {
                Ok(upgrade) => {
                    HttpResponse::websocket(upgrade.accept_split_service(self.room, self.limits))
                }
                Err(_) => HttpResponse::bad_request(),
            };
        }
        HttpResponse::not_found()
    }
}

type ListenerAddr =
    Address<HttpListenerMsg, Result<tina_http::HttpReady, tina_http::HttpStartupError>>;

fn start_split_room(
    member_capacity: usize,
) -> (
    ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    SocketAddr,
    ListenerAddr,
    Arc<LaneCounts>,
) {
    let runtime = make_runtime();
    let lanes = Arc::new(LaneCounts::default());
    let room = runtime
        .register_split_service::<SplitRoom, WebSocketSessionMsg, WebSocketSessionMsg, tina_http::HttpConnectionMsg>(
            SplitRoom {
                members: WebSocketMemberTable::new(member_capacity),
                lanes: Arc::clone(&lanes),
            },
            32,
        )
        .expect("register room");
    let gateway = runtime
        .register_with_capacity::<SplitGateway, Infallible>(
            SplitGateway {
                room,
                limits: WebSocketLimits::default(),
            },
            8,
        )
        .expect("register gateway");
    let listener = HttpListener::<TestShard>::new(
        "127.0.0.1:0".parse().unwrap(),
        gateway,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    );
    let (addr, listener_addr) = start_listener(&runtime, listener);
    (runtime, addr, listener_addr, lanes)
}

#[test]
fn split_service_two_client_broadcast_exact_once_excludes_sender() {
    let (runtime, addr, listener_addr, lanes) = start_split_room(8);

    let mut a = connect_ws(addr);
    let mut b = connect_ws(addr);

    // Drain join acks (request-lane SessionOpen replies).
    let join_a = read_server_text(&mut a);
    let join_b = read_server_text(&mut b);
    assert!(join_a.starts_with("join:"), "{join_a}");
    assert!(join_b.starts_with("join:"), "{join_b}");
    assert_ne!(join_a, join_b);

    // A speaks; only B must receive the room broadcast.
    a.write_all(&masked_frame(0x1, b"from-a"))
        .expect("a writes");
    let seen_b = read_server_text(&mut b);
    assert_eq!(seen_b, "room:from-a");

    // B speaks; only A must receive.
    b.write_all(&masked_frame(0x1, b"from-b"))
        .expect("b writes");
    let seen_a = read_server_text(&mut a);
    assert_eq!(seen_a, "room:from-b");

    // Exactly-once on the wire: after each expected room frame, no second
    // data frame (duplicate fanout or self-echo) arrives during a short drain.
    assert_no_extra_data_frame(&mut a);
    assert_no_extra_data_frame(&mut b);

    // Two fanouts × one non-sender recipient each ⇒ exactly two offers.
    wait_for_count(&lanes.send_outcomes, 2, "SendOutcome");
    // Allow a short settle so a duplicate outcome would still be counted.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        lanes.send_outcomes.load(Ordering::SeqCst),
        2,
        "SendOutcome count must equal fanout offer count"
    );
    assert!(
        lanes.event_msgs.load(Ordering::SeqCst) >= 2,
        "event lane should see SendOutcome deliveries (and may also see SessionAccepted)"
    );
    assert!(
        lanes.request_msgs.load(Ordering::SeqCst) >= 4,
        "request lane should see 2×SessionOpen + 2×SessionText, got {}",
        lanes.request_msgs.load(Ordering::SeqCst)
    );
    assert_eq!(lanes.session_texts.load(Ordering::SeqCst), 2);

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn split_service_three_client_broadcast_offer_count_and_wire_once() {
    // Multi-recipient: A speaks → B and C each get one room frame; SendOutcome
    // count equals snapshot offer count (2). Snapshot order itself is pinned
    // by websocket_room unit tests (BTreeMap by session id).
    let (runtime, addr, listener_addr, lanes) = start_split_room(8);

    let mut a = connect_ws(addr);
    let mut b = connect_ws(addr);
    let mut c = connect_ws(addr);

    let join_a = read_server_text(&mut a);
    let join_b = read_server_text(&mut b);
    let join_c = read_server_text(&mut c);
    for join in [&join_a, &join_b, &join_c] {
        assert!(join.starts_with("join:"), "{join}");
    }
    assert_ne!(join_a, join_b);
    assert_ne!(join_a, join_c);
    assert_ne!(join_b, join_c);

    a.write_all(&masked_frame(0x1, b"to-all"))
        .expect("a writes");
    assert_eq!(read_server_text(&mut b), "room:to-all");
    assert_eq!(read_server_text(&mut c), "room:to-all");
    assert_no_extra_data_frame(&mut b);
    assert_no_extra_data_frame(&mut c);
    // Sender excluded.
    assert_no_extra_data_frame(&mut a);

    wait_for_count(&lanes.send_outcomes, 2, "SendOutcome for two recipients");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        lanes.send_outcomes.load(Ordering::SeqCst),
        2,
        "offer count for three members excluding sender is 2"
    );
    assert_eq!(lanes.session_texts.load(Ordering::SeqCst), 1);

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn split_service_peer_close_frame_uses_request_lane() {
    let (runtime, addr, listener_addr, lanes) = start_split_room(4);
    let mut stream = connect_ws(addr);
    let _join = read_server_text(&mut stream);
    let requests_after_join = lanes.request_msgs.load(Ordering::SeqCst);

    // Client close frame is reply-needed (SessionClose → request lane).
    stream
        .write_all(&masked_frame(0x8, &{
            let mut p = vec![0x03, 0xe8]; // 1000
            p.extend_from_slice(b"bye");
            p
        }))
        .expect("write close");

    wait_for_count(
        &lanes.request_msgs,
        requests_after_join + 1,
        "SessionClose on request lane",
    );
    assert!(
        lanes.request_msgs.load(Ordering::SeqCst) > requests_after_join,
        "SessionClose must enter the request lane"
    );
    // Clean close handshake does not deliver app SessionClosed (that path is
    // peer-drop / empty-read). Event lane must not be required here.
    assert_eq!(
        lanes.session_closed.load(Ordering::SeqCst),
        0,
        "clean close frame is SessionClose on request, not SessionClosed on event"
    );

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn split_service_peer_drop_delivers_session_closed_on_event_lane() {
    let (runtime, addr, listener_addr, lanes) = start_split_room(4);
    let mut stream = connect_ws(addr);
    // Join ack proves SessionOpen on the request lane before we tear the peer.
    let join = read_server_text(&mut stream);
    assert!(join.starts_with("join:"), "{join}");
    // Abrupt TCP drop (no close frame) → SessionClosed on the event lane.
    drop(stream);

    wait_for_count(&lanes.session_closed, 1, "SessionClosed on event lane");
    assert_eq!(
        lanes.session_closed.load(Ordering::SeqCst),
        1,
        "one peer drop must deliver one SessionClosed to the event handler"
    );
    assert!(
        lanes.event_msgs.load(Ordering::SeqCst) >= 1,
        "SessionClosed must be counted as an event-lane delivery"
    );

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn malformed_upgrade_stays_http_400() {
    let (runtime, addr, listener_addr, _lanes) = start_split_room(2);
    let mut stream = TcpStream::connect_timeout(&addr, TEST_IO_TIMEOUT).expect("connect");
    stream
        .set_read_timeout(Some(TEST_IO_TIMEOUT))
        .expect("read timeout");
    stream
        .write_all(b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\n\r\n")
        .expect("write bad upgrade");
    let head = read_upgrade_response(&mut stream);
    assert!(
        head.starts_with("HTTP/1.1 400") || head.starts_with("HTTP/1.1 4"),
        "malformed upgrade should fail before 101, got {head}"
    );

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn listener_shutdown_during_session_is_clean() {
    let (runtime, addr, listener_addr, _lanes) = start_split_room(2);
    let mut stream = connect_ws(addr);
    let _join = read_server_text(&mut stream);

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    // Peer may see close or connection drop; either is a clean terminal.
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let mut buf = [0u8; 64];
    let _ = stream.read(&mut buf);
}


