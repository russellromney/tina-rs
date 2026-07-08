//! End-to-end coverage of the RPC call-dispatch path through the runtime.
//!
//! These tests drive real `call()` traffic: host → registry → service →
//! reply, plus a full TCP roundtrip through the [`Connection`] isolate. They
//! are the coverage whose absence let call dispatch regress. Every in-crate
//! unit test drives isolates via `.handle(...)` directly, which never
//! exercises `handle_call` — where the bug lived. On the pre-fix code
//! (`Registry` / `SingleService` implementing only `handle`), the runtime
//! delivers `call()` traffic to the default `handle_call`, which rejects with
//! `UnsupportedMessage`; every assertion below would fail with
//! `CallOutcome::Rejected` / a wire `Error(Internal)`.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_rpc::{
    Connection, ConnectionConfig, ConnectionInit, ConnectionMsg, Frame, FrameError, FrameKind,
    FrameLimits, LENGTH_PREFIX_SIZE, Registry, RegistryMsg, RouterReply, RouterRequest,
    ServiceCall, ServiceHandler, ServiceReply, SingleService, decode_body, encode,
    parse_length_prefix,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ListenerId, TcpAcceptReply, TcpBindReply,
    TcpListenerCloseReply, ThreadedRuntime, tcp_accept, tcp_bind, tcp_close_listener,
};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Method-dispatching handler. `ping` echoes; `boom` reports Internal;
/// anything else is UnknownMethod. Enough to exercise the whole mapping.
struct EchoHandler;

impl ServiceHandler<SingleShard> for EchoHandler {
    fn dispatch(&mut self, call: ServiceCall) -> ServiceReply {
        match call.method.as_str() {
            "ping" => ServiceReply::Ok(call.payload),
            "boom" => ServiceReply::Internal,
            _ => ServiceReply::UnknownMethod,
        }
    }
}

type Rt = ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>;

fn registry_runtime() -> (
    Rt,
    Address<RegistryMsg, RouterReply>,
    Address<ServiceCall, ServiceReply>,
) {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let service = runtime
        .register_with_capacity::<_, Infallible>(SingleService::new(EchoHandler), 16)
        .expect("register service");
    let registry_state = Registry::<SingleShard>::builder()
        .service("echo", service)
        .build();
    let registry = runtime
        .register_with_capacity::<_, Infallible>(registry_state, 16)
        .expect("register registry");
    (runtime, registry, service)
}

fn route(service: &str, method: &str, payload: &[u8]) -> RegistryMsg {
    RegistryMsg::Route(RouterRequest {
        request_id: 1,
        service: service.into(),
        method: method.into(),
        payload: payload.to_vec(),
    })
}

#[test]
fn single_service_call_returns_reply_not_rejected() {
    let (runtime, _registry, service) = registry_runtime();
    let outcome = runtime
        .call_blocking(
            service,
            ServiceCall {
                method: "ping".into(),
                payload: b"hi".to_vec(),
            },
            TIMEOUT,
        )
        .expect("host call to service");
    assert_eq!(
        outcome,
        CallOutcome::Replied(ServiceReply::Ok(b"hi".to_vec())),
        "SingleService must answer calls through handle_call"
    );
    let _ = runtime.shutdown();
}

#[test]
fn registry_call_returns_reply_not_internal() {
    let (runtime, registry, _service) = registry_runtime();
    let outcome = runtime
        .call_blocking(registry, route("echo", "ping", b"hi"), TIMEOUT)
        .expect("host call to registry");
    assert_eq!(
        outcome,
        CallOutcome::Replied(RouterReply::Ok(b"hi".to_vec())),
        "a well-formed call to a registered method must return Ok, not Internal/Rejected"
    );
    let _ = runtime.shutdown();
}

#[test]
fn registry_unknown_method_maps_to_unknown_method() {
    let (runtime, registry, _service) = registry_runtime();
    let outcome = runtime
        .call_blocking(registry, route("echo", "nope", b""), TIMEOUT)
        .expect("host call to registry");
    assert_eq!(outcome, CallOutcome::Replied(RouterReply::UnknownMethod));
    let _ = runtime.shutdown();
}

#[test]
fn registry_unknown_service_maps_to_unknown_service() {
    let (runtime, registry, _service) = registry_runtime();
    let outcome = runtime
        .call_blocking(registry, route("absent", "ping", b""), TIMEOUT)
        .expect("host call to registry");
    assert_eq!(outcome, CallOutcome::Replied(RouterReply::UnknownService));
    let _ = runtime.shutdown();
}

#[test]
fn registry_internal_error_propagates() {
    let (runtime, registry, _service) = registry_runtime();
    let outcome = runtime
        .call_blocking(registry, route("echo", "boom", b""), TIMEOUT)
        .expect("host call to registry");
    assert_eq!(outcome, CallOutcome::Replied(RouterReply::Internal));
    let _ = runtime.shutdown();
}

/// A registered service that captures the caller's authority and never
/// answers. The registry's downstream `call` to it therefore times out —
/// the only way to drive a downstream failure through the registry's live
/// `defer(...) -> reply_to` path (the synchronous unknown-method/service
/// arms never reach it).
#[derive(Default)]
struct BlackHole {
    held: Vec<RequestContext<ServiceReply>>,
}

#[tina_runtime::isolate(message = ServiceCall, reply = ServiceReply)]
impl BlackHole {
    fn handle(
        &mut self,
        _msg: ServiceCall,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _msg: ServiceCall, call: CallContext<'_, Self>) -> Effect<Self> {
        // Hold the authority so the caller never gets an answer and its call
        // reaches the deadline.
        self.held.push(call.into_request_context());
        noop()
    }
}

#[test]
fn registry_downstream_timeout_maps_to_internal_through_deferred_reply() {
    // Exercises the deferred branch the synchronous error arms skip: registry
    // handle_call -> defer(call(service)) -> [downstream timeout] -> continuation
    // -> reply_to(caller, Internal). A regression that broke this plumbing while
    // leaving the pure mapping table correct would slip past the unit tests but
    // fail here.
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let blackhole = runtime
        .register_with_capacity::<_, Infallible>(BlackHole::default(), 16)
        .expect("register black-hole service");
    let registry_state = Registry::<SingleShard>::builder()
        .timeout(Duration::from_millis(150))
        .service("void", blackhole)
        .build();
    let registry = runtime
        .register_with_capacity::<_, Infallible>(registry_state, 16)
        .expect("register registry");

    let outcome = runtime
        .call_blocking(registry, route("void", "ping", b"x"), TIMEOUT)
        .expect("registry answers the caller even when the service never does");
    assert_eq!(
        outcome,
        CallOutcome::Replied(RouterReply::Internal),
        "a downstream service that never replies must time out and map to \
         Internal through the deferred reply path — not hang the caller"
    );
    let _ = runtime.shutdown();
}

// ---------------------------------------------------------------------------
// Full TCP roundtrip: client → Connection → Registry → SingleService → reply.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ListenerMsg {
    Start,
    Bound(TcpBindReply),
    Accepted(TcpAcceptReply),
    Closed(TcpListenerCloseReply),
}

struct Listener {
    bind_addr: SocketAddr,
    router: Address<RegistryMsg, RouterReply>,
    listener_id: Option<ListenerId>,
}

#[tina_runtime::isolate(
    message = ListenerMsg,
    spawn = ChildDefinition<Connection<SingleShard>>,
)]
impl Listener {
    fn handle(
        &mut self,
        msg: ListenerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ListenerMsg::Start => tcp_bind(self.bind_addr).then(ListenerMsg::Bound),
            ListenerMsg::Bound(Ok((listener, _local))) => {
                self.listener_id = Some(listener);
                tcp_accept(listener).then(ListenerMsg::Accepted)
            }
            ListenerMsg::Accepted(Ok((stream, _peer))) => {
                let listener = self.listener_id.expect("listener set after bind");
                // tiny_pressure => max_in_flight = 1, so over-cap requests come
                // back as wire Error(Full) and the first must Reply.
                let connection = Connection::<SingleShard>::new(
                    ConnectionInit::new(stream, self.router)
                        .with_config(ConnectionConfig::tiny_pressure()),
                );
                batch(vec![
                    spawn(
                        ChildDefinition::new(connection, 64)
                            .with_initial_message(ConnectionMsg::Begin),
                    ),
                    tcp_close_listener(listener).then(ListenerMsg::Closed),
                ])
            }
            ListenerMsg::Closed(Ok(())) => stop(),
            ListenerMsg::Bound(Err(_))
            | ListenerMsg::Accepted(Err(_))
            | ListenerMsg::Closed(Err(_)) => stop(),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    ok: usize,
    full: usize,
    other: usize,
}

fn drive_client(addr: SocketAddr, burst: usize) -> Tally {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let limits = FrameLimits::default();
    let mut request_bytes = Vec::new();
    for id in 1..=burst as u64 {
        // EchoHandler echoes raw payload bytes; no encoding needed.
        let frame = Frame::request(id, "echo", "ping", b"hi".to_vec());
        request_bytes.extend(encode(&frame, &limits).expect("encode request"));
    }
    stream.write_all(&request_bytes).expect("write burst");

    let mut tally = Tally::default();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while tally.ok + tally.full + tally.other < burst {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => {
                tally.other += burst - (tally.ok + tally.full + tally.other);
                return tally;
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        loop {
            if buf.len() < LENGTH_PREFIX_SIZE {
                break;
            }
            let mut prefix = [0u8; LENGTH_PREFIX_SIZE];
            prefix.copy_from_slice(&buf[..LENGTH_PREFIX_SIZE]);
            let body_len = match parse_length_prefix(prefix, &limits) {
                Ok(len) => len,
                Err(_) => {
                    tally.other += 1;
                    return tally;
                }
            };
            let total = LENGTH_PREFIX_SIZE + body_len;
            if buf.len() < total {
                break;
            }
            let body = buf[LENGTH_PREFIX_SIZE..total].to_vec();
            buf.drain(..total);
            match decode_body(&body) {
                Ok(frame) => match (frame.kind, frame.error) {
                    (FrameKind::Reply, _) => tally.ok += 1,
                    (FrameKind::Error, Some(FrameError::Full)) => tally.full += 1,
                    _ => tally.other += 1,
                },
                Err(_) => tally.other += 1,
            }
        }
    }
    tally
}

#[test]
fn full_tcp_roundtrip_first_request_replies() {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let service = runtime
        .register_with_capacity::<_, Infallible>(SingleService::new(EchoHandler), 16)
        .expect("register service");
    let registry_state = Registry::<SingleShard>::builder()
        .service("echo", service)
        .build();
    let registry = runtime
        .register_with_capacity::<_, Infallible>(registry_state, 16)
        .expect("register registry");

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = runtime
        .register_with_capacity::<_, Infallible>(
            Listener {
                bind_addr,
                router: registry,
                listener_id: None,
            },
            8,
        )
        .expect("register listener");

    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, ListenerMsg::Start)
        .expect("start listener");
    let addr = bound.wait(Duration::from_secs(3)).expect("listener bind");

    let tally = thread::spawn(move || drive_client(addr, 4))
        .join()
        .expect("client thread");
    let _ = runtime.shutdown();

    // max_in_flight = 1: the first request grabs the slot and replies; the
    // remaining three arrive over-cap and come back as wire Error(Full).
    assert_eq!(
        tally,
        Tally {
            ok: 1,
            full: 3,
            other: 0
        },
        "the first request must round-trip a Reply, not Error(Internal)"
    );
}
