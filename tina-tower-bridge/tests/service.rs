//! End-to-end tests for `tina-tower-bridge`.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntimeConfig};
use tina_tokio_bridge::{BridgeError, BridgeHost, BridgeMessage, BridgeRequest};
use tina_tower_bridge::TinaTowerService;
use tower_service::Service;

// --- shared echo isolate -------------------------------------------------

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
            EchoMsg::Request(req) => {
                let (value, responder) = req.into_parts();
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
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match svc.poll_ready(&mut cx) {
        Poll::Ready(Ok(())) => {}
        other => panic!("expected Ready(Ok), got {other:?}"),
    }
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
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match svc.poll_ready(&mut cx) {
        Poll::Ready(Err(BridgeError::Closed)) => {}
        other => panic!("expected Ready(Err(Closed)), got {other:?}"),
    }
}

#[tokio::test]
async fn poll_ready_never_pending_under_saturation() {
    // Block the worker with a slow isolate (mailbox capacity 1) so a
    // second call would saturate. poll_ready must still return Ready;
    // saturation surfaces via the Future of `call`, never via Pending
    // on poll_ready.
    let host = make_host();
    let bridge = host
        .register_bridge::<SlowIsolate, u32, u32, Infallible>(
            SlowIsolate {
                delay: Duration::from_millis(100),
            },
            1,
            Duration::from_secs(2),
        )
        .expect("register");
    let svc = TinaTowerService::new(bridge);

    // Hold one in-flight call.
    let svc_a = svc.clone();
    let in_flight = tokio::spawn(async move {
        let mut svc_a = svc_a;
        svc_a.call(1).await
    });

    // Give the worker a moment to admit the first call.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // poll_ready must still be Ready.
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut svc_b = svc.clone();
    let polled = svc_b.poll_ready(&mut cx);
    assert!(
        matches!(polled, Poll::Ready(Ok(()))),
        "poll_ready must never be Pending: got {polled:?}"
    );

    let _ = in_flight.await.expect("await first");
}

// --- pressure ----------------------------------------------------------

#[tokio::test]
async fn full_surfaces_on_call_future_not_poll_ready() {
    // Mailbox capacity 1 with a slow handler. Submit one call, hold it,
    // submit a second — second should fail with Full immediately.
    let host = make_host();
    let bridge = host
        .register_bridge::<SlowIsolate, u32, u32, Infallible>(
            SlowIsolate {
                delay: Duration::from_millis(120),
            },
            1,
            Duration::from_secs(2),
        )
        .expect("register");
    let svc = TinaTowerService::new(bridge);

    let svc_a = svc.clone();
    let in_flight = tokio::spawn(async move {
        let mut svc_a = svc_a;
        svc_a.call(1).await
    });

    tokio::time::sleep(Duration::from_millis(15)).await;

    let mut svc_b = svc.clone();
    // poll_ready first — must be Ready.
    std::future::poll_fn(|cx| svc_b.poll_ready(cx))
        .await
        .expect("ready");
    let res = svc_b.call(2).await;
    match res {
        Err(BridgeError::Full) => {}
        other => panic!("expected Full on saturation, got {other:?}"),
    }

    let _ = in_flight.await.expect("await first");
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
    let res = svc.call(1).await;
    match res {
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
    let res = svc.call(1).await;
    match res {
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
    let res = svc.call(1).await;
    match res {
        Err(BridgeError::Timeout) => {}
        other => panic!("expected Timeout via with_timeout, got {other:?}"),
    }
}

// --- cancellation ------------------------------------------------------

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

    // Construct the future and drop it without polling — the future
    // contains the request value but never spawns the boxed body.
    {
        let _fut = svc.call(7);
    }

    // Give any spurious scheduling a moment.
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
    // Submit a slow call, drop the future after admission has happened
    // but before the handler responds. The handler still runs; the
    // bridge counts the late response as dropped.
    let host = make_host();
    let bridge = host
        .register_bridge::<SlowIsolate, u32, u32, Infallible>(
            SlowIsolate {
                delay: Duration::from_millis(120),
            },
            4,
            Duration::from_secs(2),
        )
        .expect("register");
    let mut svc = TinaTowerService::new(bridge.clone());

    let baseline = bridge.metrics();
    let mut fut = Box::pin(svc.call(3));
    // Poll once to start admission.
    let _ = futures_first_poll(fut.as_mut()).await;
    // Wait long enough for admission to land.
    tokio::time::sleep(Duration::from_millis(25)).await;
    let mid = bridge.metrics();
    assert!(
        mid.accepted > baseline.accepted,
        "admission must have happened after first poll"
    );

    drop(fut);

    // Wait long enough for the slow handler to finish and try to send
    // its reply into the now-dropped responder.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let after = bridge.metrics();
    assert!(
        after.dropped_responses > baseline.dropped_responses,
        "after-admission drop must record a dropped response"
    );
}

// --- helpers -----------------------------------------------------------

#[derive(Debug)]
struct SlowIsolate {
    delay: Duration,
}

#[derive(Debug)]
enum SlowMsg {
    Request(BridgeRequest<u32, u32>),
    Done(Result<(), tina_runtime::CallError>, u32, tina_tokio_bridge::BridgeResponder<u32>),
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
            Self::Done(_, _, _) => false,
        }
    }
}

impl Isolate for SlowIsolate {
    tina::isolate_types! {
        message: SlowMsg,
        reply: u32,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: tina_runtime::RuntimeCall<SlowMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: SlowMsg,
        _ctx: &mut tina::Context<'_, SingleShard>,
    ) -> Effect<Self> {
        match msg {
            SlowMsg::Request(req) => {
                let (value, responder) = req.into_parts();
                let delay = self.delay;
                tina_runtime::sleep(delay).reply(move |result| SlowMsg::Done(result, value, responder))
            }
            SlowMsg::Done(_, value, responder) => {
                let _ = responder.respond(value * 2);
                noop()
            }
        }
    }
}

/// Polls a future once, returning whatever it yields (Ready or
/// Pending). Used to start admission without awaiting completion.
async fn futures_first_poll<F: Future + Unpin>(mut fut: Pin<&mut F>) -> Poll<F::Output> {
    std::future::poll_fn(move |cx| {
        Poll::Ready(fut.as_mut().poll(cx))
    })
    .await
}

fn noop_waker() -> Waker {
    use std::task::{RawWaker, RawWakerVTable};
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    fn wake(_: *const ()) {}
    fn wake_by_ref(_: *const ()) {}
    fn drop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    // Safety: VTABLE only operates on the null pointer in safe ways.
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}
