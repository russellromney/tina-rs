use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use tina::prelude::*;
use tina::{Mailbox, TrySendError};
use tina_runtime::{
    BetelgeuseBackedRuntime, BetelgeuseBackedRuntimeConfig, MailboxFactory, RuntimeEventKind,
};
use tina_tokio_bridge::{
    BRIDGE_CAPABILITIES, BridgeBackpressure, BridgeError, BridgeHandle, BridgeHealth, BridgeHost,
    BridgeRequest, BridgeResponder, CapabilityStatus,
};
use tower::ServiceExt;

#[derive(Debug, Clone, Copy)]
struct BridgeShard;

impl Shard for BridgeShard {
    fn id(&self) -> ShardId {
        ShardId::new(33)
    }
}

struct BridgeMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> BridgeMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for BridgeMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed lock") {
            return Err(TrySendError::Closed(message));
        }

        let mut queue = self.queue.lock().expect("queue lock");
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }

        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue lock").pop_front()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed lock") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct BridgeMailboxFactory;

impl MailboxFactory for BridgeMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(BridgeMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrushRequest {
    llama: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrushReply {
    line: String,
}

#[derive(Debug)]
struct LlamaCounter {
    brushes: usize,
}

#[tina::isolate(
    message = BridgeRequest<BrushRequest, BrushReply>,
    shard = BridgeShard
)]
impl LlamaCounter {
    fn handle(
        &mut self,
        msg: BridgeRequest<BrushRequest, BrushReply>,
        _ctx: &mut Context<'_, BridgeShard>,
    ) -> Effect<Self> {
        let (request, responder) = msg.into_parts();
        self.brushes += 1;
        let _ = responder.respond(BrushReply {
            line: format!("{} brushed {}", request.llama, self.brushes),
        });
        noop()
    }
}

#[test]
fn bridge_capability_table_keeps_preserved_and_weakened_claims_explicit() {
    assert_eq!(
        BRIDGE_CAPABILITIES.bounded_ingress,
        CapabilityStatus::Preserved
    );
    assert_eq!(
        BRIDGE_CAPABILITIES.synchronous_handlers,
        CapabilityStatus::Preserved
    );
    assert_eq!(
        BRIDGE_CAPABILITIES.visible_failures,
        CapabilityStatus::Preserved
    );
    assert_eq!(
        BRIDGE_CAPABILITIES.deterministic_replay,
        CapabilityStatus::Weakened
    );
    assert_eq!(
        BRIDGE_CAPABILITIES.tokio_scheduler_control,
        CapabilityStatus::NotClaimed
    );
}

type LlamaBridge = BridgeHandle<BrushRequest, BrushReply, BridgeShard, BridgeMailboxFactory, ()>;

async fn brush(State(bridge): State<LlamaBridge>) -> (StatusCode, String) {
    match bridge.call(BrushRequest { llama: "Tina" }).await {
        Ok(reply) => (StatusCode::OK, reply.line),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, format!("{error:?}")),
    }
}

#[tokio::test]
async fn axum_route_calls_tina_over_bounded_bridge() {
    let runtime = Arc::new(BetelgeuseBackedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let address = runtime
        .register_with_capacity::<LlamaCounter, Infallible>(LlamaCounter { brushes: 0 }, 8)
        .expect("register llama isolate");
    let bridge = BridgeHandle::new(Arc::clone(&runtime), address, Duration::from_secs(1));

    let app = Router::new().route("/brush", get(brush)).with_state(bridge);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/brush")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .expect("body bytes");
    assert_eq!(&body[..], b"Tina brushed 1");

    let runtime = match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime,
        Err(_) => panic!("app dropped bridge handle"),
    };
    let trace = runtime.shutdown().expect("runtime shutdown");
    assert!(trace.iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::HandlerFinished {
                effect: tina_runtime::EffectKind::Noop
            }
        )
    }));
}

#[tokio::test]
async fn bridge_host_registers_service_and_shutdown_requires_dropped_handles() {
    let host = BridgeHost::new(
        BridgeShard,
        BridgeMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let bridge = host
        .register_bridge::<LlamaCounter, BrushRequest, BrushReply, Infallible>(
            LlamaCounter { brushes: 0 },
            8,
            Duration::from_secs(1),
        )
        .expect("register bridge service");

    assert_eq!(
        bridge.call(BrushRequest { llama: "Tina" }).await,
        Ok(BrushReply {
            line: "Tina brushed 1".to_string()
        })
    );
    assert!(
        host.shutdown().is_err(),
        "live bridge handle keeps host shared"
    );

    let host = BridgeHost::new(
        BridgeShard,
        BridgeMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let bridge = host
        .register_bridge::<LlamaCounter, BrushRequest, BrushReply, Infallible>(
            LlamaCounter { brushes: 0 },
            8,
            Duration::from_secs(1),
        )
        .expect("register bridge service");
    drop(bridge);
    let _ = host.shutdown().expect("host shutdown");
}

#[tokio::test]
async fn bridge_close_health_and_metrics_are_visible() {
    let runtime = Arc::new(BetelgeuseBackedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let address = runtime
        .register_with_capacity::<LlamaCounter, Infallible>(LlamaCounter { brushes: 0 }, 8)
        .expect("register llama isolate");
    let bridge = BridgeHandle::new(Arc::clone(&runtime), address, Duration::from_secs(1));

    assert_eq!(bridge.health(), BridgeHealth::Accepting);
    bridge.close();
    assert_eq!(bridge.health(), BridgeHealth::Closed);
    assert_eq!(
        bridge.call(BrushRequest { llama: "Tina" }).await,
        Err(BridgeError::Closed)
    );
    assert_eq!(bridge.metrics().attempts, 1);
    assert_eq!(bridge.metrics().closed, 1);

    drop(bridge);
    let runtime = match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime,
        Err(_) => panic!("bridge handle dropped"),
    };
    let _ = runtime.shutdown().expect("runtime shutdown");
}

#[tokio::test]
async fn bridge_reports_target_mailbox_full_without_waiting_for_timeout() {
    let runtime = Arc::new(BetelgeuseBackedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let address = runtime
        .register_with_capacity::<LlamaCounter, Infallible>(LlamaCounter { brushes: 0 }, 0)
        .expect("register zero-capacity llama isolate");
    let bridge = BridgeHandle::new(Arc::clone(&runtime), address, Duration::from_secs(5));

    let started = std::time::Instant::now();
    assert_eq!(
        bridge.call(BrushRequest { llama: "Tina" }).await,
        Err(BridgeError::Full)
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "mailbox Full should surface as Full, not timeout"
    );
    assert_eq!(bridge.metrics().attempts, 1);
    assert_eq!(bridge.metrics().full, 1);

    drop(bridge);
    let runtime = match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime,
        Err(_) => panic!("bridge handle dropped"),
    };
    let _ = runtime.shutdown().expect("runtime shutdown");
}

#[tokio::test]
async fn bridge_retry_policy_is_bounded_and_explicit() {
    let runtime = Arc::new(BetelgeuseBackedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let address = runtime
        .register_with_capacity::<LlamaCounter, Infallible>(LlamaCounter { brushes: 0 }, 0)
        .expect("register zero-capacity llama isolate");
    let bridge = BridgeHandle::new(Arc::clone(&runtime), address, Duration::from_secs(1));

    assert_eq!(
        bridge
            .call_with_policy(
                BrushRequest { llama: "Tina" },
                BridgeBackpressure::retry(2, Duration::from_millis(1)),
                Duration::from_secs(1),
            )
            .await,
        Err(BridgeError::Full)
    );
    assert_eq!(bridge.metrics().attempts, 3);
    assert_eq!(bridge.metrics().full, 3);

    drop(bridge);
    let runtime = match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime,
        Err(_) => panic!("bridge handle dropped"),
    };
    let _ = runtime.shutdown().expect("runtime shutdown");
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GateRequest(&'static str);

#[derive(Debug, Clone, PartialEq, Eq)]
struct GateReply(&'static str);

#[derive(Debug)]
struct HoldingWorker {
    held: Vec<BridgeResponder<GateReply>>,
}

#[tina::isolate(
    message = BridgeRequest<GateRequest, GateReply>,
    shard = BridgeShard
)]
impl HoldingWorker {
    fn handle(
        &mut self,
        msg: BridgeRequest<GateRequest, GateReply>,
        _ctx: &mut Context<'_, BridgeShard>,
    ) -> Effect<Self> {
        let (_request, responder) = msg.into_parts();
        self.held.push(responder);
        noop()
    }
}

#[derive(Debug)]
struct BlockingWorker {
    entered: SyncSender<()>,
    release: Receiver<()>,
}

#[tina::isolate(
    message = BridgeRequest<GateRequest, GateReply>,
    shard = BridgeShard
)]
impl BlockingWorker {
    fn handle(
        &mut self,
        msg: BridgeRequest<GateRequest, GateReply>,
        _ctx: &mut Context<'_, BridgeShard>,
    ) -> Effect<Self> {
        let (request, responder) = msg.into_parts();
        self.entered.send(()).expect("entered signal");
        self.release.recv().expect("release signal");
        let _ = responder.respond(GateReply(request.0));
        noop()
    }
}

#[tokio::test]
async fn bridge_records_worker_observed_full_even_after_caller_timeout() {
    let runtime = Arc::new(BetelgeuseBackedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let full_address = runtime
        .register_with_capacity::<LlamaCounter, Infallible>(LlamaCounter { brushes: 0 }, 0)
        .expect("register zero-capacity llama isolate");
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let blocker_address = runtime
        .register_with_capacity::<BlockingWorker, Infallible>(
            BlockingWorker {
                entered: entered_tx,
                release: release_rx,
            },
            8,
        )
        .expect("register blocking worker");

    let blocker = BridgeHandle::new(
        Arc::clone(&runtime),
        blocker_address,
        Duration::from_secs(5),
    );
    let full_bridge =
        BridgeHandle::new(Arc::clone(&runtime), full_address, Duration::from_millis(1));
    let blocker_task = tokio::spawn(async move { blocker.call(GateRequest("released")).await });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(1)))
        .await
        .expect("entered wait task")
        .expect("blocking handler entered");

    assert_eq!(
        full_bridge.call(BrushRequest { llama: "Tina" }).await,
        Err(BridgeError::Timeout)
    );
    release_tx.send(()).expect("release blocking handler");
    assert_eq!(
        blocker_task.await.expect("blocker task"),
        Ok(GateReply("released"))
    );

    let deadline = Instant::now() + Duration::from_secs(1);
    while full_bridge.metrics().full == 0 {
        assert!(
            Instant::now() < deadline,
            "worker-observed mailbox Full should still be counted after caller timeout"
        );
        tokio::task::yield_now().await;
    }

    assert_eq!(full_bridge.metrics().attempts, 1);
    assert_eq!(full_bridge.metrics().timeout, 1);
    assert_eq!(full_bridge.metrics().full, 1);

    drop(full_bridge);
    let runtime = match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime,
        Err(_) => panic!("bridge handles dropped"),
    };
    let _ = runtime.shutdown().expect("runtime shutdown");
}

#[tokio::test]
async fn bridge_timeout_is_explicit_when_tina_keeps_responder_open() {
    let runtime = Arc::new(BetelgeuseBackedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let address = runtime
        .register_with_capacity::<HoldingWorker, Infallible>(HoldingWorker { held: Vec::new() }, 8)
        .expect("register holding worker");
    let bridge = BridgeHandle::new(Arc::clone(&runtime), address, Duration::from_millis(10));

    assert_eq!(
        bridge.call(GateRequest("held")).await,
        Err(BridgeError::Timeout)
    );

    drop(bridge);
    let runtime = match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime,
        Err(_) => panic!("bridge handle dropped"),
    };
    let _ = runtime.shutdown().expect("runtime shutdown");
}

#[derive(Debug)]
struct CapturingWorker {
    captured: Arc<Mutex<Option<BridgeResponder<GateReply>>>>,
}

#[tina::isolate(
    message = BridgeRequest<GateRequest, GateReply>,
    shard = BridgeShard
)]
impl CapturingWorker {
    fn handle(
        &mut self,
        msg: BridgeRequest<GateRequest, GateReply>,
        _ctx: &mut Context<'_, BridgeShard>,
    ) -> Effect<Self> {
        let (_request, responder) = msg.into_parts();
        *self.captured.lock().expect("captured responder lock") = Some(responder);
        noop()
    }
}

#[tokio::test]
async fn bridge_caller_timeout_closes_responder_and_counts_late_response() {
    let runtime = Arc::new(BetelgeuseBackedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let captured = Arc::new(Mutex::new(None));
    let address = runtime
        .register_with_capacity::<CapturingWorker, Infallible>(
            CapturingWorker {
                captured: Arc::clone(&captured),
            },
            8,
        )
        .expect("register capturing worker");
    let bridge = BridgeHandle::new(Arc::clone(&runtime), address, Duration::from_millis(10));

    assert_eq!(
        bridge.call(GateRequest("held")).await,
        Err(BridgeError::Timeout)
    );
    let responder = captured
        .lock()
        .expect("captured responder lock")
        .take()
        .expect("handler captured responder");
    assert!(responder.is_closed());
    assert_eq!(responder.respond(GateReply("late")), Err(GateReply("late")));
    assert_eq!(bridge.metrics().timeout, 1);
    assert_eq!(bridge.metrics().dropped_responses, 1);

    drop(bridge);
    let runtime = match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime,
        Err(_) => panic!("bridge handle dropped"),
    };
    let _ = runtime.shutdown().expect("runtime shutdown");
}

#[derive(Debug)]
struct GateWorker {
    entered: SyncSender<()>,
    release: Receiver<()>,
}

#[tina::isolate(
    message = BridgeRequest<GateRequest, GateReply>,
    shard = BridgeShard
)]
impl GateWorker {
    fn handle(
        &mut self,
        msg: BridgeRequest<GateRequest, GateReply>,
        _ctx: &mut Context<'_, BridgeShard>,
    ) -> Effect<Self> {
        let (request, responder) = msg.into_parts();
        self.entered.send(()).expect("entered signal");
        self.release.recv().expect("release signal");
        let _ = responder.respond(GateReply(request.0));
        noop()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_ingress_full_is_visible_without_hidden_tokio_queue() {
    let runtime = Arc::new(BetelgeuseBackedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 1,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let address = runtime
        .register_with_capacity::<GateWorker, Infallible>(
            GateWorker {
                entered: entered_tx,
                release: release_rx,
            },
            8,
        )
        .expect("register gate worker");
    let bridge = BridgeHandle::new(Arc::clone(&runtime), address, Duration::from_secs(5));

    let first = {
        let bridge = bridge.clone();
        tokio::task::spawn(async move { bridge.call(GateRequest("first")).await })
    };
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker entered first request");

    let (second_tx, second_rx) = tokio::sync::oneshot::channel();
    runtime
        .try_send(
            address,
            BridgeRequest::new(GateRequest("second"), second_tx),
        )
        .expect("second request fills bounded worker queue");

    assert_eq!(
        bridge.call(GateRequest("third")).await,
        Err(BridgeError::Full)
    );

    release_tx.send(()).expect("release first");
    assert_eq!(first.await.expect("first task"), Ok(GateReply("first")));
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker entered second request");
    release_tx.send(()).expect("release second");
    assert_eq!(second_rx.await.expect("second reply"), GateReply("second"));

    drop(bridge);
    let runtime = match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime,
        Err(_) => panic!("bridge handles dropped"),
    };
    let _ = runtime.shutdown().expect("runtime shutdown");
}
