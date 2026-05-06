//! Phase 047 Rock 7: bridge drain + shutdown tests.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntimeConfig};
use tina_tokio_bridge::{BridgeHost, BridgeMessage, BridgeRequest, BridgeShutdownError};

#[derive(Debug, Default)]
struct EchoIsolate;

#[derive(Debug)]
enum EchoMsg {
    Request(BridgeRequest<u32, u32>),
}

impl From<BridgeRequest<u32, u32>> for EchoMsg {
    fn from(value: BridgeRequest<u32, u32>) -> Self {
        Self::Request(value)
    }
}

impl BridgeMessage for EchoMsg {
    fn bridge_cancelled(&self) -> bool {
        match self {
            Self::Request(request) => request.bridge_cancelled(),
        }
    }
}

#[tina_runtime::isolate(message = EchoMsg)]
impl EchoIsolate {
    fn handle(&mut self, msg: EchoMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            EchoMsg::Request(request) => {
                let (value, responder) = request.into_parts();
                let _ = responder.respond(value);
                noop()
            }
        }
    }
}

fn make_host() -> BridgeHost<SingleShard, DefaultThreadedMailboxFactory> {
    BridgeHost::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

#[test]
fn drain_and_shutdown_succeeds_when_no_handles_outstanding() {
    let mut host = make_host();
    let report = host
        .drain_and_shutdown(Duration::from_millis(100))
        .expect("shutdown completes");
    assert_eq!(report.outstanding_handles_at_shutdown, 0);
    assert!(report.drained_within_timeout);
}

#[test]
fn drain_and_shutdown_waits_for_outstanding_handles_to_drop() {
    let mut host = make_host();
    let handle = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            8,
            Duration::from_millis(100),
        )
        .expect("register bridge");
    assert_eq!(host.pending_handles(), 1);

    // Drop the handle on a worker thread so the host's drain loop sees the
    // strong count drop within the drain window.
    let drop_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        drop(handle);
    });

    let report = host
        .drain_and_shutdown(Duration::from_secs(2))
        .expect("drain completes within budget");
    drop_thread.join().expect("drop thread");
    assert!(report.drained_within_timeout);
    assert_eq!(report.outstanding_handles_at_shutdown, 0);
}

#[test]
fn drain_and_shutdown_reports_partial_drain_on_timeout() {
    let mut host = make_host();
    let leak = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            4,
            Duration::from_millis(100),
        )
        .expect("register bridge");

    // Hold the handle across drain. The drain hits its deadline.
    let report = host
        .drain_and_shutdown(Duration::from_millis(50))
        .expect("drain returns even if handles remain");
    assert!(!report.drained_within_timeout);
    assert!(report.outstanding_handles_at_shutdown >= 1);
    assert!(report.trace.is_empty());
    drop(leak);
    let retry = host
        .drain_and_shutdown(Duration::from_secs(2))
        .expect("retry drain after leaked handle is dropped");
    assert!(retry.drained_within_timeout);
    assert_eq!(retry.outstanding_handles_at_shutdown, 0);
}

#[test]
fn try_shutdown_with_outstanding_handle_returns_still_shared() {
    let mut host = make_host();
    let _leak = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            4,
            Duration::from_millis(100),
        )
        .expect("register bridge");
    match host.try_shutdown() {
        Err(BridgeShutdownError::StillShared) => {}
        other => panic!("expected StillShared, got {other:?}"),
    }
    // Host is still usable: pending_handles still names the leak.
    assert!(host.pending_handles() >= 1);
}
