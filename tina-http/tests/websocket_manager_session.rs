//! Live session-lifecycle test for the WebSocket client manager.
//!
//! A real WS echo server needs the random-key handshake, which is heavy to
//! stand up in a test. Instead a stub connection isolate speaks the
//! `WebSocketClientMsg`/`WebSocketClientReply` protocol directly: the manager
//! resolves a literal address through real DNS and dials the stub, which
//! short-circuits the socket. This exercises the manager's success path end
//! to end — connect → send → receive → peer-close → bounded reconnect at a
//! new generation — plus pressure folding and shutdown, on the real runtime.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::mpsc;
use std::time::Duration;

use tina::prelude::*;
use tina::{Address, CallContext};
use tina_http::{
    AddressFamilyPolicy, ConnectPolicy, WebSocketClientConnected, WebSocketClientEvent,
    WebSocketClientManager, WebSocketClientMsg, WebSocketClientReply, WebSocketClientReport,
    WebSocketCloseCode, WebSocketConnectOutcome, WebSocketEndpoint, WebSocketManagerConfig,
    WebSocketManagerMsg, WebSocketManagerReply, WebSocketSessionError,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

const TEST_SHARD_ID: u32 = 217;

#[derive(Debug, Default)]
struct TestShard;
impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(TEST_SHARD_ID)
    }
}

// ----- stub connection isolate: speaks the WS client protocol, no socket ---

struct StubConn {
    events: VecDeque<WebSocketClientEvent>,
    report: WebSocketClientReport,
}

impl Isolate for StubConn {
    tina::isolate_types! {
        message: WebSocketClientMsg,
        reply: WebSocketClientReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        _msg: WebSocketClientMsg,
        _ctx: &mut Context<'_, TestShard, WebSocketClientReply>,
    ) -> Effect<Self> {
        // Stop and stray fire-and-forget messages: nothing to do in the stub.
        noop()
    }

    fn handle_call(&mut self, msg: WebSocketClientMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WebSocketClientMsg::Connect { .. } => call_ctx.reply(WebSocketClientReply::Connected(
                Ok(WebSocketClientConnected {
                    selected_subprotocol: None,
                }),
            )),
            WebSocketClientMsg::Send(_) => call_ctx.reply(WebSocketClientReply::Sent(Ok(()))),
            WebSocketClientMsg::Receive => {
                let event = self.events.pop_front().unwrap_or(WebSocketClientEvent::Close {
                    code: Some(WebSocketCloseCode(1000)),
                    reason: Vec::new(),
                });
                call_ctx.reply(WebSocketClientReply::Event(Ok(event)))
            }
            WebSocketClientMsg::Report => {
                call_ctx.reply(WebSocketClientReply::Report(self.report.clone()))
            }
            _ => call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

// ----- driver that calls the manager and forwards replies to the host ------

#[derive(Debug)]
enum DriverMsg {
    Op(WebSocketManagerMsg),
    Done(CallOutcome<WebSocketManagerReply>),
}

struct Driver {
    manager: Address<WebSocketManagerMsg, WebSocketManagerReply>,
    notify: mpsc::Sender<WebSocketManagerReply>,
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

    fn handle(&mut self, msg: DriverMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        match msg {
            DriverMsg::Op(op) => {
                call(self.manager, op, Duration::from_secs(10)).then(DriverMsg::Done)
            }
            DriverMsg::Done(CallOutcome::Replied(reply)) => {
                let _ = self.notify.send(reply);
                noop()
            }
            DriverMsg::Done(other) => {
                panic!("manager call did not reply: {other:?}");
            }
        }
    }
}

fn wait(rx: &mpsc::Receiver<WebSocketManagerReply>) -> WebSocketManagerReply {
    rx.recv_timeout(Duration::from_secs(15))
        .expect("manager reply before deadline")
}

#[test]
fn session_lifecycle_connect_send_receive_then_bounded_reconnect() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    // The stub scripts one text event then a peer close.
    let mut report = WebSocketClientReport::default();
    report.queued_outbound_bytes = 7;
    let stub = StubConn {
        events: VecDeque::from(vec![
            WebSocketClientEvent::Text("hi".to_string()),
            WebSocketClientEvent::Close {
                code: Some(WebSocketCloseCode(1000)),
                reason: Vec::new(),
            },
        ]),
        report,
    };
    let stub_addr = runtime
        .register_with_capacity::<StubConn, Infallible>(stub, 16)
        .expect("register stub");

    let mut policy = ConnectPolicy::balanced();
    policy.address_family = AddressFamilyPolicy::PreserveOrder;
    policy.max_resolved_addresses = 1;
    policy.max_total_attempts = 1;
    policy.happy_eyeballs.max_concurrent_attempts = 1;
    policy.happy_eyeballs.delay = Duration::ZERO;
    policy.dns_timeout = Duration::from_secs(2);
    policy.connect_timeout = Duration::from_secs(2);

    let mut config = WebSocketManagerConfig::new(policy);
    config.max_reconnects = 2;
    config.validate().unwrap();

    let endpoint = WebSocketEndpoint::ws("127.0.0.1", 8080, "/ws");
    let manager = WebSocketClientManager::<TestShard>::new(endpoint, config, vec![stub_addr]);
    let manager_addr = runtime
        .register_with_capacity::<WebSocketClientManager<TestShard>, WebSocketClientMsg>(
            manager, 32,
        )
        .expect("register manager");

    let (tx, rx) = mpsc::channel();
    let driver = Driver {
        manager: manager_addr,
        notify: tx,
    };
    let driver_addr = runtime
        .register_with_capacity::<Driver, Infallible>(driver, 32)
        .expect("register driver");

    let op = |op| {
        let _ = runtime.try_send(driver_addr, DriverMsg::Op(op));
        wait(&rx)
    };

    // Connect: a session opens at generation 1.
    let gen1 = match op(WebSocketManagerMsg::Connect) {
        WebSocketManagerReply::Connect(WebSocketConnectOutcome::Connected(report)) => {
            assert_eq!(report.winner, Some("127.0.0.1:8080".parse().unwrap()));
            report.generation
        }
        other => panic!("expected Connected, got {other:?}"),
    };

    // Send routes to the session.
    assert_eq!(
        op(WebSocketManagerMsg::Send(tina_http::WebSocketMessage::Text(
            "ping".to_string()
        ))),
        WebSocketManagerReply::Sent(Ok(()))
    );

    // Receive pulls the scripted text event.
    match op(WebSocketManagerMsg::Receive) {
        WebSocketManagerReply::Event(Ok(WebSocketClientEvent::Text(t))) => assert_eq!(t, "hi"),
        other => panic!("expected Text event, got {other:?}"),
    }

    // Report folds the session's queued-byte pressure.
    match op(WebSocketManagerMsg::Report) {
        WebSocketManagerReply::Report(r) => {
            assert!(r.has_session);
            assert_eq!(
                r.current_pressure.map(|p| p.queued_outbound_bytes),
                Some(7)
            );
        }
        other => panic!("expected Report, got {other:?}"),
    }

    // Receive pulls the peer close: the session is retired.
    match op(WebSocketManagerMsg::Receive) {
        WebSocketManagerReply::Event(Ok(WebSocketClientEvent::Close { .. })) => {}
        other => panic!("expected Close event, got {other:?}"),
    }
    match op(WebSocketManagerMsg::Report) {
        WebSocketManagerReply::Report(r) => assert!(!r.has_session, "session retired on close"),
        other => panic!("expected Report, got {other:?}"),
    }

    // A send with no session is NotConnected, not a panic.
    assert_eq!(
        op(WebSocketManagerMsg::Send(tina_http::WebSocketMessage::Text(
            "x".to_string()
        ))),
        WebSocketManagerReply::Sent(Err(WebSocketSessionError::NotConnected))
    );

    // Reconnect: a new session opens at a fresh generation.
    let gen2 = match op(WebSocketManagerMsg::Connect) {
        WebSocketManagerReply::Connect(WebSocketConnectOutcome::Connected(report)) => {
            report.generation
        }
        other => panic!("expected reconnect Connected, got {other:?}"),
    };
    assert!(gen2 > gen1, "reconnect bumps the endpoint generation");

    // The reconnect spent one of the budget; the report proves it.
    match op(WebSocketManagerMsg::Report) {
        WebSocketManagerReply::Report(r) => {
            assert!(r.has_session);
            assert_eq!(r.sessions_opened, 2);
            // A healthy reconnect resets the live reconnect budget.
            assert_eq!(r.reconnects_used, 0);
            assert_eq!(r.reconnects_total, 1);
        }
        other => panic!("expected Report, got {other:?}"),
    }

    // Shutdown drains the open session and reports it.
    match op(WebSocketManagerMsg::Shutdown) {
        WebSocketManagerReply::Shutdown(report) => {
            assert_eq!(report.stopped, 1, "the open session was stopped");
            assert!(!report.state.has_session);
        }
        other => panic!("expected Shutdown, got {other:?}"),
    }

    let _ = runtime.shutdown();
}
