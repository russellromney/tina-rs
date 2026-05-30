//! Integration tests for the reconnecting WebSocket client manager.
//!
//! The closed-port reconnect storm needs no server: it dials a port that
//! nothing is listening on, so each TCP connect is refused. It proves the
//! whole live path — real DNS of a literal address, a real refused connect,
//! the bounded connect race, and the bounded reconnect budget — ends in
//! `NoHealthyEndpoint` without leaking sessions or attempts.

#![allow(dead_code)]

use std::convert::Infallible;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use tina::Address;
use tina::prelude::*;
use tina_http::{
    AddressFamilyPolicy, ConnectPolicy, WebSocketConnectOutcome, WebSocketEndpoint,
    WebSocketManagerConfig, WebSocketManagerMsg, WebSocketManagerReply, WebSocketManagerReport,
    build_websocket_client_manager,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

const TEST_SHARD_ID: u32 = 211;

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(TEST_SHARD_ID)
    }
}

#[derive(Debug)]
enum DriverEvent {
    Connect(WebSocketConnectOutcome),
    Report(WebSocketManagerReport),
    Unexpected(String),
}

#[derive(Debug)]
enum DriverMsg {
    Connect,
    Report,
    ConnectDone(CallOutcome<WebSocketManagerReply>),
    ReportDone(CallOutcome<WebSocketManagerReply>),
}

struct Driver {
    manager: Address<WebSocketManagerMsg, WebSocketManagerReply>,
    notify: mpsc::Sender<DriverEvent>,
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
            DriverMsg::Connect => call(
                self.manager,
                WebSocketManagerMsg::Connect,
                Duration::from_secs(10),
            )
            .then(DriverMsg::ConnectDone),
            DriverMsg::Report => call(
                self.manager,
                WebSocketManagerMsg::Report,
                Duration::from_secs(10),
            )
            .then(DriverMsg::ReportDone),
            DriverMsg::ConnectDone(outcome) => {
                let event = match outcome {
                    CallOutcome::Replied(WebSocketManagerReply::Connect(o)) => {
                        DriverEvent::Connect(o)
                    }
                    other => DriverEvent::Unexpected(format!("{other:?}")),
                };
                let _ = self.notify.send(event);
                noop()
            }
            DriverMsg::ReportDone(outcome) => {
                let event = match outcome {
                    CallOutcome::Replied(WebSocketManagerReply::Report(r)) => {
                        DriverEvent::Report(r)
                    }
                    other => DriverEvent::Unexpected(format!("{other:?}")),
                };
                let _ = self.notify.send(event);
                noop()
            }
        }
    }
}

fn wait_event(rx: &mpsc::Receiver<DriverEvent>) -> DriverEvent {
    rx.recv_timeout(Duration::from_secs(15))
        .expect("driver event before deadline")
}

/// A port that nothing is listening on: bind to an ephemeral port, then drop
/// the listener so connects are refused.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

#[test]
fn closed_port_reconnect_storm_is_bounded_and_leaks_nothing() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let port = closed_port();
    let endpoint = WebSocketEndpoint::ws("127.0.0.1", port, "/ws");

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
    config.validate().expect("config validates");

    let handles = build_websocket_client_manager(&runtime, endpoint, config, 32, 16)
        .expect("register manager");

    let (tx, rx) = mpsc::channel();
    let driver = Driver {
        manager: handles.manager,
        notify: tx,
    };
    let driver_addr = runtime
        .register_with_capacity::<Driver, Infallible>(driver, 32)
        .expect("register driver");

    // Fresh connect + two reconnects all fail against the refused port.
    for attempt in 0..3 {
        let _ = runtime.try_send(driver_addr, DriverMsg::Connect);
        match wait_event(&rx) {
            DriverEvent::Connect(WebSocketConnectOutcome::ConnectFailed(report))
            | DriverEvent::Connect(WebSocketConnectOutcome::TimedOut(report)) => {
                assert!(report.winner.is_none(), "no winner against a closed port");
                assert_eq!(report.attempted.len(), 1, "one bounded attempt per connect");
                assert!(report.late_completions == 0, "no late completion leaks");
            }
            other => panic!("attempt {attempt}: expected ConnectFailed, got {other:?}"),
        }
    }

    // The reconnect budget is now spent: the next connect refuses to dial.
    let _ = runtime.try_send(driver_addr, DriverMsg::Connect);
    match wait_event(&rx) {
        DriverEvent::Connect(WebSocketConnectOutcome::NoHealthyEndpoint(_)) => {}
        other => panic!("expected NoHealthyEndpoint after budget exhausted, got {other:?}"),
    }

    // Nothing leaked: no session, the reconnect budget is fully spent, and
    // the connect-failure count matches the attempts that actually dialed.
    let _ = runtime.try_send(driver_addr, DriverMsg::Report);
    match wait_event(&rx) {
        DriverEvent::Report(report) => {
            assert!(!report.has_session, "no session should be open");
            assert_eq!(report.sessions_open, 0);
            assert_eq!(report.reconnects_used, report.max_reconnects);
            assert_eq!(report.no_healthy_count, 1);
            assert_eq!(report.connect_failed_count, 3);
        }
        other => panic!("expected Report, got {other:?}"),
    }

    let _ = runtime.shutdown();
}
