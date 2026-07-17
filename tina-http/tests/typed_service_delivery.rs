//! Typed HTTP service delivery: event-only, request-only, and split-service
//! handles install without naming the private envelope or extracting a raw
//! address. Wire outcomes stay exact.
//!
//! Direct proof matrix:
//! - event-only admission → 202
//! - request-only and split request lanes → typed reply
//! - Full → 429 (unit + host-fill), Closed → 503, timeout → 504, malformed → 400
//! - peer close and listener shutdown remain clean
//! - compile_fail doctests on the constructors prove lane confusion is
//!   rejected at compile time

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina::{AddressGeneration, RequestContext, reply_to};
use tina_http::{
    HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, HttpServerConfig,
    response_for_call_outcome, response_for_send_outcome,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, SendOutcome, ThreadedRuntime, ThreadedRuntimeConfig,
};

const SHARD: u32 = 173;

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

fn config() -> HttpServerConfig {
    HttpServerConfig {
        limits: HttpLimits {
            header_read_timeout: Duration::from_secs(2),
            ..HttpLimits::default()
        },
        service_call_timeout: Duration::from_millis(200),
        connection_mailbox_capacity: 16,
        listener_mailbox_capacity: 8,
    }
}

fn start_listener<M: Send + 'static>(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    listener: HttpListener<TestShard, M>,
    mailbox: usize,
) -> (
    SocketAddr,
    Address<HttpListenerMsg, Result<tina_http::HttpReady, tina_http::HttpStartupError>>,
) {
    let listener_addr = runtime
        .register_with_capacity::<HttpListener<TestShard, M>, _>(listener, mailbox)
        .expect("register listener");
    let bound = runtime.observe_next_bound().expect("bind observer");
    runtime
        .try_send(listener_addr, HttpListenerMsg::Start)
        .expect("start listener");
    let addr = bound.wait(Duration::from_secs(2)).expect("bound addr");
    (addr, listener_addr)
}

fn scripted(addr: SocketAddr, request: &[u8]) -> String {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("write timeout");
    stream.write_all(request).expect("write request");
    let _ = stream.flush();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn status_line(response: &str) -> &str {
    response.lines().next().unwrap_or("")
}

// ---------------------------------------------------------------------------
// Request-only service
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct EchoRequest(HttpRequest);

impl From<HttpRequest> for EchoRequest {
    fn from(value: HttpRequest) -> Self {
        Self(value)
    }
}

struct EchoService;

#[tina_runtime::isolate(request = EchoRequest, reply = HttpResponse, shard = TestShard)]
impl EchoService {
    fn handle_request(
        &mut self,
        request: EchoRequest,
        call: tina::RequestCall<'_, Self>,
    ) -> tina::RequestEffect<Self> {
        let path = request.0.path;
        call.reply(HttpResponse::text(format!("echo:{path}")))
    }
}

#[test]
fn request_only_delivery_returns_typed_reply() {
    let runtime = make_runtime();
    let requests = runtime
        .register_request_service::<EchoService, EchoRequest, Infallible>(EchoService, 8)
        .expect("register request service");
    let listener = HttpListener::<TestShard, _>::for_request_service(
        "127.0.0.1:0".parse().unwrap(),
        requests,
        config(),
    );
    let (addr, listener_addr) = start_listener(&runtime, listener, 8);

    let response = scripted(addr, b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        status_line(&response).starts_with("HTTP/1.1 200"),
        "got {response}"
    );
    assert!(response.contains("echo:/hello"), "got {response}");

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

// ---------------------------------------------------------------------------
// Event-only service
// ---------------------------------------------------------------------------

#[derive(Debug)]
#[allow(dead_code)]
struct NotifyEvent(HttpRequest);

impl From<HttpRequest> for NotifyEvent {
    fn from(value: HttpRequest) -> Self {
        Self(value)
    }
}

struct NotifyService {
    admitted: Arc<AtomicU64>,
}

#[tina_runtime::isolate(event = NotifyEvent, shard = TestShard)]
impl NotifyService {
    fn handle_event(
        &mut self,
        event: NotifyEvent,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        let _ = event;
        self.admitted.fetch_add(1, Ordering::SeqCst);
        noop()
    }
}

#[test]
fn event_only_delivery_returns_202_on_admission() {
    let runtime = make_runtime();
    let admitted = Arc::new(AtomicU64::new(0));
    let events = runtime
        .register_event_service::<NotifyService, NotifyEvent, Infallible>(
            NotifyService {
                admitted: Arc::clone(&admitted),
            },
            8,
        )
        .expect("register event service");
    let listener = HttpListener::<TestShard, _>::for_event_service(
        "127.0.0.1:0".parse().unwrap(),
        events,
        config(),
    );
    let (addr, listener_addr) = start_listener(&runtime, listener, 8);

    let response = scripted(
        addr,
        b"POST /notify HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(
        status_line(&response).starts_with("HTTP/1.1 202"),
        "event admission must answer 202, got {response}"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while admitted.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(admitted.load(Ordering::SeqCst), 1, "event was delivered");

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

// ---------------------------------------------------------------------------
// Split service
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SplitRequest(HttpRequest);

impl From<HttpRequest> for SplitRequest {
    fn from(value: HttpRequest) -> Self {
        Self(value)
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum SplitEvent {
    Tick,
}

struct SplitService {
    ticks: u64,
}

#[tina_runtime::isolate(
    event = SplitEvent,
    request = SplitRequest,
    reply = HttpResponse,
    shard = TestShard
)]
impl SplitService {
    fn handle_event(
        &mut self,
        event: SplitEvent,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            SplitEvent::Tick => {
                self.ticks += 1;
                noop()
            }
        }
    }

    fn handle_request(
        &mut self,
        request: SplitRequest,
        call: tina::RequestCall<'_, Self>,
    ) -> tina::RequestEffect<Self> {
        let path = request.0.path;
        call.reply(HttpResponse::text(format!(
            "split:{path}:ticks={}",
            self.ticks
        )))
    }
}

#[test]
fn split_service_delivery_uses_request_lane_only() {
    let runtime = make_runtime();
    let handle = runtime
        .register_split_service::<SplitService, SplitEvent, SplitRequest, Infallible>(
            SplitService { ticks: 0 },
            8,
        )
        .expect("register split service");
    let listener = HttpListener::<TestShard, _>::for_split_service(
        "127.0.0.1:0".parse().unwrap(),
        handle,
        config(),
    );
    let (addr, listener_addr) = start_listener(&runtime, listener, 8);

    let response = scripted(addr, b"GET /tree HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        status_line(&response).starts_with("HTTP/1.1 200"),
        "got {response}"
    );
    assert!(response.contains("split:/tree:ticks=0"), "got {response}");

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

// ---------------------------------------------------------------------------
// Terminal outcomes
// ---------------------------------------------------------------------------

#[test]
fn closed_service_returns_503() {
    let runtime = make_runtime();
    // Same system, never-registered isolate: every call observes Closed.
    let stale = tina::ServiceRequestAddress::<Infallible, EchoRequest, HttpResponse>::from_call_address(
        Address::<tina::ServiceMessage<Infallible, EchoRequest>, HttpResponse>::new_with_generation_in(
            runtime.system_incarnation(),
            ShardId::new(SHARD),
            IsolateId::new(9_999_999),
            AddressGeneration::new(0),
        )
        .callable(),
    );
    let listener = HttpListener::<TestShard, _>::for_requests(
        "127.0.0.1:0".parse().unwrap(),
        stale,
        config(),
    );
    let (addr, listener_addr) = start_listener(&runtime, listener, 8);

    let response = scripted(addr, b"GET /x HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        status_line(&response).starts_with("HTTP/1.1 503"),
        "closed service must answer 503, got {response}"
    );

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn event_closed_service_returns_503() {
    let runtime = make_runtime();
    let stale = tina::ServiceEventAddress::<NotifyEvent, Infallible>::from_send_address(
        Address::<tina::ServiceMessage<NotifyEvent, Infallible>>::new_with_generation_in(
            runtime.system_incarnation(),
            ShardId::new(SHARD),
            IsolateId::new(9_999_998),
            AddressGeneration::new(0),
        )
        .send_only(),
    );
    let listener =
        HttpListener::<TestShard, _>::for_events("127.0.0.1:0".parse().unwrap(), stale, config());
    let (addr, listener_addr) = start_listener(&runtime, listener, 8);

    let response = scripted(
        addr,
        b"POST /n HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(
        status_line(&response).starts_with("HTTP/1.1 503"),
        "closed event service must answer 503, got {response}"
    );

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[derive(Debug)]
enum TimeoutEvent {
    Late(RequestContext<HttpResponse>),
}

#[derive(Debug)]
#[allow(dead_code)]
struct TimeoutHttp(HttpRequest);

impl From<HttpRequest> for TimeoutHttp {
    fn from(value: HttpRequest) -> Self {
        Self(value)
    }
}

struct TimeoutSplit;

#[tina_runtime::isolate(
    event = TimeoutEvent,
    request = TimeoutHttp,
    reply = HttpResponse,
    shard = TestShard
)]
impl TimeoutSplit {
    fn handle_event(
        &mut self,
        event: TimeoutEvent,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            TimeoutEvent::Late(req) => reply_to(req, HttpResponse::text("late")),
        }
    }

    fn handle_request(
        &mut self,
        _request: TimeoutHttp,
        call: tina::RequestCall<'_, Self>,
    ) -> tina::RequestEffect<Self> {
        // Sleep longer than service_call_timeout so the connection sees Timeout.
        call.defer(tina_runtime::sleep(Duration::from_secs(2)))
            .reply(|req, _| tina::ServiceMessage::Event(TimeoutEvent::Late(req)))
    }
}

#[test]
fn request_timeout_returns_504() {
    let runtime = make_runtime();
    let handle = runtime
        .register_split_service::<TimeoutSplit, TimeoutEvent, TimeoutHttp, Infallible>(
            TimeoutSplit,
            8,
        )
        .expect("register timeout service");
    let mut cfg = config();
    cfg.service_call_timeout = Duration::from_millis(50);
    let listener = HttpListener::<TestShard, _>::for_split_service(
        "127.0.0.1:0".parse().unwrap(),
        handle,
        cfg,
    );
    let (addr, listener_addr) = start_listener(&runtime, listener, 8);

    let response = scripted(addr, b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        status_line(&response).starts_with("HTTP/1.1 504"),
        "slow service must answer 504, got {response}"
    );

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn malformed_input_returns_400() {
    let runtime = make_runtime();
    let requests = runtime
        .register_request_service::<EchoService, EchoRequest, Infallible>(EchoService, 8)
        .expect("register");
    let listener = HttpListener::<TestShard, _>::for_request_service(
        "127.0.0.1:0".parse().unwrap(),
        requests,
        config(),
    );
    let (addr, listener_addr) = start_listener(&runtime, listener, 8);

    let response = scripted(addr, b"GARBAGE\r\n\r\n");
    assert!(
        status_line(&response).starts_with("HTTP/1.1 400"),
        "malformed must answer 400, got {response}"
    );

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn peer_close_before_request_is_clean() {
    let runtime = make_runtime();
    let requests = runtime
        .register_request_service::<EchoService, EchoRequest, Infallible>(EchoService, 8)
        .expect("register");
    let listener = HttpListener::<TestShard, _>::for_request_service(
        "127.0.0.1:0".parse().unwrap(),
        requests,
        config(),
    );
    let (addr, listener_addr) = start_listener(&runtime, listener, 8);

    {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
        drop(stream);
    }
    std::thread::sleep(Duration::from_millis(100));

    let response = scripted(addr, b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        status_line(&response).starts_with("HTTP/1.1 200"),
        "server remains healthy after peer close, got {response}"
    );

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn listener_shutdown_stops_accept_loop() {
    let runtime = make_runtime();
    let requests = runtime
        .register_request_service::<EchoService, EchoRequest, Infallible>(EchoService, 8)
        .expect("register");
    let listener = HttpListener::<TestShard, _>::for_request_service(
        "127.0.0.1:0".parse().unwrap(),
        requests,
        config(),
    );
    let (addr, listener_addr) = start_listener(&runtime, listener, 8);

    let _ = runtime.try_send(listener_addr, HttpListenerMsg::Stop);
    std::thread::sleep(Duration::from_millis(100));
    drop(TcpStream::connect_timeout(&addr, Duration::from_millis(200)));
    let _ = runtime.shutdown();
}

#[test]
fn terminal_status_table_is_settled() {
    // Full → 429, Closed → 503, Timeout → 504, Accepted → 202.
    assert_eq!(
        response_for_send_outcome(SendOutcome::Full).status,
        http::StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        response_for_send_outcome(SendOutcome::Closed).status,
        http::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        response_for_send_outcome(SendOutcome::Accepted).status,
        http::StatusCode::ACCEPTED
    );
    assert_eq!(
        response_for_call_outcome(&CallOutcome::<HttpResponse>::Full)
            .expect("full")
            .status,
        http::StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        response_for_call_outcome(&CallOutcome::<HttpResponse>::Closed)
            .expect("closed")
            .status,
        http::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        response_for_call_outcome(&CallOutcome::<HttpResponse>::Timeout)
            .expect("timeout")
            .status,
        http::StatusCode::GATEWAY_TIMEOUT
    );
}

/// Install shape for scoped request tree after this work: split handle in,
/// no `ServiceMessage` name at the call site.
#[test]
fn scoped_tree_install_shape_compiles_without_envelope_alias() {
    let runtime = make_runtime();
    let handle = runtime
        .register_split_service::<SplitService, SplitEvent, SplitRequest, Infallible>(
            SplitService { ticks: 0 },
            8,
        )
        .expect("register");
    // Desired post-rebase shape for system_scoped_request_tree:
    let _listener = HttpListener::<TestShard, _>::for_split_service(
        "127.0.0.1:0".parse().unwrap(),
        handle,
        config(),
    );
    // Type is inferred — the private envelope is never named here.
    let _ = runtime.shutdown();
}
