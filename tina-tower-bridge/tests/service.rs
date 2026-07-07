//! End-to-end tests for `tina-tower-bridge`.

use std::convert::Infallible;
use std::task::Poll;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntimeConfig};
use tina_tokio_bridge::{BridgeError, BridgeHost, BridgeMessage, BridgeRequest, BridgeResponder};
use tina_tower_bridge::{Service, TinaTowerService};

// --- shared echo + slow isolates ----------------------------------------

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
    fn handle(
        &mut self,
        msg: EchoMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            EchoMsg::Request(req) => {
                let (value, responder) = req.into_parts();
                let _ = responder.respond(value * 2);
                noop()
            }
        }
    }
}

#[derive(Debug)]
struct SlowIsolate {
    delay: Duration,
}

#[derive(Debug)]
enum SlowMsg {
    Request(BridgeRequest<u32, u32>),
    Done(u32, BridgeResponder<u32>),
}

impl From<BridgeRequest<u32, u32>> for SlowMsg {
    fn from(value: BridgeRequest<u32, u32>) -> Self {
        Self::Request(value)
    }
}

impl BridgeMessage for SlowMsg {
    fn bridge_cancelled(&self) -> bool {
        match self {
            Self::Request(request) => request.bridge_cancelled(),
            Self::Done(_, _) => false,
        }
    }
}

impl Isolate for SlowIsolate {
    tina::isolate_types! {
        message: SlowMsg,
        reply: u32,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: tina_runtime::RuntimeCall<SlowMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: SlowMsg,
        _ctx: &mut tina::Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SlowMsg::Request(req) => {
                let (value, responder) = req.into_parts();
                let delay = self.delay;
                tina_runtime::sleep(delay).then(move |_| SlowMsg::Done(value, responder))
            }
            SlowMsg::Done(value, responder) => {
                let _ = responder.respond(value * 2);
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
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

// --- happy path ---------------------------------------------------------

#[tokio::test]
async fn service_call_returns_handler_reply() {
    let host = make_host();
    let bridge = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            8,
            Duration::from_secs(1),
        )
        .expect("register");
    let mut svc = TinaTowerService::new(bridge);
    let response = svc.call(7).await.expect("svc call");
    assert_eq!(response, 14);
}

// --- readiness contract -----------------------------------------------

#[tokio::test]
async fn poll_ready_returns_ready_ok_when_accepting() {
    let host = make_host();
    let bridge = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            4,
            Duration::from_secs(1),
        )
        .expect("register");
    let mut svc = TinaTowerService::new(bridge);
    std::future::poll_fn(|cx| svc.poll_ready(cx))
        .await
        .expect("expected Ready(Ok)");
}

#[test]
fn bridge_error_implements_display_and_error() {
    let err = BridgeError::Full;
    let display = format!("{err}");
    assert!(display.contains("full"), "Display: {display}");
    let _: &dyn std::error::Error = &err;
    let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
    let _ = boxed;
}

#[tokio::test]
async fn is_closed_reflects_close_state() {
    let host = make_host();
    let bridge = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            4,
            Duration::from_secs(1),
        )
        .expect("register");
    let svc = TinaTowerService::new(bridge);
    assert!(!svc.is_closed());
    svc.close();
    assert!(svc.is_closed());
}

#[tokio::test]
async fn poll_ready_returns_ready_err_after_close() {
    let host = make_host();
    let bridge = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            4,
            Duration::from_secs(1),
        )
        .expect("register");
    let mut svc = TinaTowerService::new(bridge);
    svc.close();
    let result = std::future::poll_fn(|cx| svc.poll_ready(cx)).await;
    assert!(matches!(result, Err(BridgeError::Closed)));
}

#[tokio::test]
async fn poll_ready_does_not_reflect_in_flight_count() {
    // poll_ready answers "is the bridge open" only. Even with one
    // call in flight on a slow handler, poll_ready must still return
    // Ready(Ok). Admission backpressure surfaces on the call future,
    // never on poll_ready.
    let host = make_host();
    let bridge = host
        .register_bridge::<SlowIsolate, u32, u32, Infallible>(
            SlowIsolate {
                delay: Duration::from_millis(100),
            },
            4,
            Duration::from_secs(2),
        )
        .expect("register");
    let svc = TinaTowerService::new(bridge);

    let mut svc_a = svc.clone();
    let in_flight = tokio::spawn(async move { svc_a.call(1).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut svc_b = svc.clone();
    let polled = std::future::poll_fn(|cx| Poll::Ready(svc_b.poll_ready(cx))).await;
    assert!(
        matches!(polled, Poll::Ready(Ok(()))),
        "poll_ready must never be Pending: got {polled:?}"
    );

    let _ = in_flight.await.expect("await first");
}

// --- pressure ----------------------------------------------------------

#[tokio::test]
async fn full_surfaces_on_call_future_not_poll_ready() {
    // Mailbox capacity 0 forces every send to fail Full at admission
    // time. poll_ready still returns Ready(Ok); the Full surfaces on
    // the call future.
    let host = make_host();
    let bridge = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            0,
            Duration::from_secs(1),
        )
        .expect("register");
    let mut svc = TinaTowerService::new(bridge);

    std::future::poll_fn(|cx| svc.poll_ready(cx))
        .await
        .expect("ready");
    match svc.call(7).await {
        Err(BridgeError::Full) => {}
        other => panic!("expected Full on zero-capacity mailbox, got {other:?}"),
    }
}

#[tokio::test]
async fn closed_surfaces_on_call_future() {
    let host = make_host();
    let bridge = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            4,
            Duration::from_secs(1),
        )
        .expect("register");
    let mut svc = TinaTowerService::new(bridge);
    svc.close();
    match svc.call(1).await {
        Err(BridgeError::Closed) => {}
        other => panic!("expected Closed, got {other:?}"),
    }
}

#[tokio::test]
async fn timeout_surfaces_on_call_future() {
    let host = make_host();
    let bridge = host
        .register_bridge::<SlowIsolate, u32, u32, Infallible>(
            SlowIsolate {
                delay: Duration::from_millis(500),
            },
            4,
            Duration::from_millis(50),
        )
        .expect("register");
    let mut svc = TinaTowerService::new(bridge);
    match svc.call(1).await {
        Err(BridgeError::Timeout) => {}
        other => panic!("expected Timeout, got {other:?}"),
    }
}

#[tokio::test]
async fn with_timeout_overrides_handle_default() {
    let host = make_host();
    let bridge = host
        .register_bridge::<SlowIsolate, u32, u32, Infallible>(
            SlowIsolate {
                delay: Duration::from_millis(500),
            },
            4,
            Duration::from_secs(5),
        )
        .expect("register");
    let mut svc = TinaTowerService::new(bridge).with_timeout(Duration::from_millis(40));
    match svc.call(1).await {
        Err(BridgeError::Timeout) => {}
        other => panic!("expected Timeout via with_timeout, got {other:?}"),
    }
}

// --- cancellation -------------------------------------------------------

#[tokio::test]
async fn drop_future_before_poll_does_no_work() {
    let host = make_host();
    let bridge = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            8,
            Duration::from_secs(1),
        )
        .expect("register");
    let baseline = bridge.metrics();
    let mut svc = TinaTowerService::new(bridge.clone());

    // Construct the future and drop it without polling.
    {
        let _fut = svc.call(7);
    }

    tokio::time::sleep(Duration::from_millis(20)).await;

    let after = bridge.metrics();
    assert_eq!(
        after.attempts, baseline.attempts,
        "drop-before-poll must not record an attempt"
    );
    assert_eq!(
        after.accepted, baseline.accepted,
        "drop-before-poll must not admit"
    );
}

#[tokio::test]
async fn drop_future_after_admission_marks_response_dropped() {
    // Submit a slow call, abort the spawned task after admission has
    // happened but before the handler responds. The handler still
    // runs; the bridge counts the dropped responder.
    let host = make_host();
    let bridge = host
        .register_bridge::<SlowIsolate, u32, u32, Infallible>(
            SlowIsolate {
                delay: Duration::from_millis(300),
            },
            4,
            Duration::from_secs(2),
        )
        .expect("register");
    let baseline = bridge.metrics();
    let mut svc = TinaTowerService::new(bridge.clone());

    let task = tokio::spawn(async move { svc.call(3).await });

    // Poll until admission lands, with a generous deadline for slow CI.
    let admit_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if bridge.metrics().accepted > baseline.accepted {
            break;
        }
        if std::time::Instant::now() >= admit_deadline {
            panic!("admission did not land within deadline");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    task.abort();

    // Poll until the late response lands; deadline well past the
    // 300ms handler delay.
    let drop_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if bridge.metrics().dropped_responses > baseline.dropped_responses {
            return;
        }
        if std::time::Instant::now() >= drop_deadline {
            panic!(
                "dropped_responses did not increment within deadline; metrics={:?}",
                bridge.metrics()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
