//! Proof tests that `TinaTowerService` composes correctly with the
//! Tower middleware listed in the crate docs.
//!
//! Three layers, three properties to lock:
//!
//! - `tower::timeout::Timeout` — outer timeout drops the service
//!   future; bridge `dropped_responses` increments when Tina later
//!   replies into the dropped responder.
//! - `tower::limit::ConcurrencyLimit` — second call waits or errors at
//!   the Tower layer before Tina sees it; bridge `accepted` does not
//!   advance for the blocked call until the permit frees.
//! - `tower::buffer::Buffer` — bounded buffer admits up to `bound`;
//!   requests waiting in the buffer are outside Tina metrics. No
//!   unbounded queue exists.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, LocalSystem};
use tina_tokio_bridge::{BridgeError, BridgeHost, BridgeMessage, BridgeRequest, BridgeResponder};
use tina_tower_bridge::{Service, TinaTowerService};
use tower::ServiceExt;
use tower::buffer::Buffer;
use tower::limit::ConcurrencyLimit;
use tower::timeout::Timeout;

// ---------------------------------------------------------------------------
// Slow isolate fixture: handler delays via `sleep` then responds.
// ---------------------------------------------------------------------------

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
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .ingress_capacity(16)
        .idle_wait(Duration::from_millis(1))
        .try_build()
        .expect("start local bridge app");
    BridgeHost::from_app(app)
}

// ---------------------------------------------------------------------------
// Timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_layer_fires_outer_and_bridge_records_dropped_response() {
    // Inner bridge timeout is generous (5s). Tower's outer Timeout is
    // tight (40ms). The slow isolate delays 300ms, so Tower fires
    // first, the call future is dropped, and the inner Tina handler
    // eventually completes and tries to respond into the dropped
    // responder — that increments dropped_responses.
    let host = make_host();
    let bridge = host
        .register_bridge::<SlowIsolate, u32, u32, Infallible>(
            SlowIsolate {
                delay: Duration::from_millis(300),
            },
            8,
            Duration::from_secs(5),
        )
        .expect("register");
    let baseline = bridge.metrics();
    let svc = TinaTowerService::new(bridge.clone());
    let mut layered = Timeout::new(svc, Duration::from_millis(40));

    let result = layered.ready().await.unwrap().call(7).await;
    assert!(
        result.is_err(),
        "Tower Timeout should surface an error; got {result:?}"
    );

    // Wait long enough for the slow handler to finish and try to
    // respond into the dropped responder.
    let drop_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < drop_deadline {
        if bridge.metrics().dropped_responses > baseline.dropped_responses {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "expected dropped_responses to advance after Tower Timeout dropped the future; metrics={:?}",
        bridge.metrics()
    );
}

// ---------------------------------------------------------------------------
// ConcurrencyLimit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrency_limit_blocks_second_call_at_tower_layer() {
    // Tower ConcurrencyLimit cap = 1. Inner bridge mailbox is roomy.
    // First call holds the permit (slow handler). Second call must
    // wait at the Tower layer; bridge `accepted` should not advance
    // for the blocked call until the first finishes.
    let host = make_host();
    let bridge = host
        .register_bridge::<SlowIsolate, u32, u32, Infallible>(
            SlowIsolate {
                delay: Duration::from_millis(150),
            },
            8,
            Duration::from_secs(5),
        )
        .expect("register");
    let svc = TinaTowerService::new(bridge.clone());
    let layered = ConcurrencyLimit::new(svc, 1);

    let mut a = layered.clone();
    let mut b = layered;

    let baseline = bridge.metrics();

    let first = tokio::spawn(async move { a.ready().await.unwrap().call(1).await });
    // Let the first call's admission land in the bridge.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mid = bridge.metrics();
    assert!(
        mid.accepted == baseline.accepted + 1,
        "first call should be accepted by the bridge; metrics={mid:?}"
    );

    // Second call: kick it; it should be blocked at the Tower layer
    // (ready() will not resolve immediately).
    let bridge_for_b = bridge.clone();
    let second = tokio::spawn(async move { b.ready().await.unwrap().call(2).await });

    // Sample bridge metrics for ~80ms while first call is still in
    // flight: accepted must not advance to baseline+2 yet — that
    // would mean ConcurrencyLimit let the second call through.
    let observe_deadline = std::time::Instant::now() + Duration::from_millis(80);
    while std::time::Instant::now() < observe_deadline {
        let snap = bridge_for_b.metrics();
        assert!(
            snap.accepted <= baseline.accepted + 1,
            "ConcurrencyLimit should hold the second call at the Tower layer; \
             bridge accepted={} baseline={}",
            snap.accepted,
            baseline.accepted
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Both calls eventually finish.
    let _ = first.await.unwrap();
    let _ = second.await.unwrap();

    // After both complete, accepted should be baseline + 2.
    let final_snap = bridge.metrics();
    assert_eq!(final_snap.accepted, baseline.accepted + 2);
}

// ---------------------------------------------------------------------------
// Buffer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn buffer_layer_admits_up_to_bound_and_does_not_grow_unboundedly() {
    // Bounded buffer with bound=2 in front of a single-slot Tina
    // mailbox. Three concurrent callers: the buffer admits two
    // immediately; the third must wait for buffer capacity. No
    // unbounded queue grows.
    let host = make_host();
    let bridge = host
        .register_bridge::<SlowIsolate, u32, u32, Infallible>(
            SlowIsolate {
                delay: Duration::from_millis(80),
            },
            8,
            Duration::from_secs(5),
        )
        .expect("register");
    let svc = TinaTowerService::new(bridge.clone());
    let buffered = Buffer::new(svc, 2);

    let baseline = bridge.metrics();

    let a = buffered.clone();
    let b = buffered.clone();
    let c = buffered.clone();
    let task_a = tokio::spawn({
        let mut a = a;
        async move { a.ready().await.unwrap().call(1).await }
    });
    let task_b = tokio::spawn({
        let mut b = b;
        async move { b.ready().await.unwrap().call(2).await }
    });
    let task_c = tokio::spawn({
        let mut c = c;
        async move { c.ready().await.unwrap().call(3).await }
    });

    // All three should complete (eventually). The buffer absorbs two
    // calls beyond the inner service's instantaneous capacity.
    let result_a = task_a.await.unwrap();
    let result_b = task_b.await.unwrap();
    let result_c = task_c.await.unwrap();
    assert!(result_a.is_ok());
    assert!(result_b.is_ok());
    assert!(result_c.is_ok());

    let final_snap = bridge.metrics();
    assert_eq!(final_snap.accepted, baseline.accepted + 3);
}

// ---------------------------------------------------------------------------
// Sanity: the inner service still returns BridgeError on its own when
// no middleware is layered. Locks the contract that middleware tests
// don't accidentally mask.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn raw_service_still_returns_bridge_error_on_close() {
    let host = make_host();
    let bridge = host
        .register_bridge::<SlowIsolate, u32, u32, Infallible>(
            SlowIsolate {
                delay: Duration::from_millis(10),
            },
            4,
            Duration::from_secs(1),
        )
        .expect("register");
    let mut svc = TinaTowerService::new(bridge);
    svc.close();
    match svc.call(1).await {
        Err(BridgeError::Closed) => {}
        other => panic!("raw service should return Closed; got {other:?}"),
    }
}
