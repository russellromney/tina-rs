use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
use tina_tokio_bridge::{BridgeError, BridgeHandle, BridgeRequest, BridgeResponder};
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
