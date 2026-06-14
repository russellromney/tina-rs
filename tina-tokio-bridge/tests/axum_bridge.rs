use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::path::PathBuf;
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
    FileId, LocalSystem, MailboxFactory, RuntimeEventKind, ThreadedRuntime, ThreadedRuntimeConfig,
    file_close, file_create, file_fsync, file_read, file_size, file_write, sleep,
};
use tina_tokio_bridge::{
    BRIDGE_CAPABILITIES, BridgeBackpressure, BridgeError, BridgeHandle, BridgeHealth, BridgeHost,
    BridgeMessage, BridgeRequest, BridgeResponder, BridgeShutdownError, CapabilityStatus,
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
    fn is_empty(&self) -> bool {
        self.queue.lock().expect("queue lock").is_empty()
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
        _ctx: &mut Context<'_, BridgeShard, Self::Reply>,
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
async fn llama_http_bridge_service_routes_axum_to_tower_bridge() {
    let runtime = Arc::new(ThreadedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
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
    let mut host = BridgeHost::new(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
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
        matches!(host.try_shutdown(), Err(BridgeShutdownError::StillShared)),
        "live bridge handle keeps host shared"
    );
    drop(bridge);
    let _ = host.try_shutdown().expect("retryable host shutdown");

    let mut host = BridgeHost::new(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
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
    let _ = host.try_shutdown().expect("host shutdown");
}

#[tokio::test]
async fn bridge_host_can_be_built_from_canonical_local_system() {
    let app = LocalSystem::single_shard(BridgeShard, BridgeMailboxFactory)
        .ingress_capacity(8)
        .build();
    let mut host = BridgeHost::from_app(app);
    let bridge = host
        .register_bridge::<LlamaCounter, BrushRequest, BrushReply, Infallible>(
            LlamaCounter { brushes: 0 },
            8,
            Duration::from_secs(1),
        )
        .expect("register bridge service from local system");

    assert_eq!(
        bridge.call(BrushRequest { llama: "Tina" }).await,
        Ok(BrushReply {
            line: "Tina brushed 1".to_string()
        })
    );

    drop(bridge);
    let terminal_trace = host.try_shutdown().expect("shutdown local-app bridge host");
    assert!(terminal_trace.iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::HandlerFinished {
                effect: tina_runtime::EffectKind::Noop
            }
        )
    }));
}

#[tokio::test]
async fn bridge_close_health_and_metrics_are_visible() {
    let runtime = Arc::new(ThreadedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
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
    let runtime = Arc::new(ThreadedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
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
    let runtime = Arc::new(ThreadedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
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

#[tokio::test]
async fn bridge_retry_policy_can_have_total_deadline() {
    let runtime = Arc::new(ThreadedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let address = runtime
        .register_with_capacity::<LlamaCounter, Infallible>(LlamaCounter { brushes: 0 }, 0)
        .expect("register zero-capacity llama isolate");
    let bridge = BridgeHandle::new(Arc::clone(&runtime), address, Duration::from_secs(1));

    let started = Instant::now();
    assert_eq!(
        bridge
            .call_with_policy(
                BrushRequest { llama: "Tina" },
                BridgeBackpressure::retry_within(
                    100,
                    Duration::from_millis(25),
                    Duration::from_millis(30),
                ),
                Duration::from_secs(1),
            )
            .await,
        Err(BridgeError::Timeout)
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "total retry deadline should bound all attempts plus delays"
    );
    assert_eq!(bridge.metrics().timeout, 1);
    assert!(bridge.metrics().attempts >= 1);

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
        _ctx: &mut Context<'_, BridgeShard, Self::Reply>,
    ) -> Effect<Self> {
        let (_request, responder) = msg.into_parts();
        self.held.push(responder);
        noop()
    }
}

#[derive(Debug)]
enum ScheduledMsg {
    Request(BridgeRequest<BrushRequest, BrushReply>),
    Ready(BridgeRequest<BrushRequest, BrushReply>),
}

impl From<BridgeRequest<BrushRequest, BrushReply>> for ScheduledMsg {
    fn from(request: BridgeRequest<BrushRequest, BrushReply>) -> Self {
        Self::Request(request)
    }
}

impl BridgeMessage for ScheduledMsg {
    fn bridge_cancelled(&self) -> bool {
        match self {
            Self::Request(request) | Self::Ready(request) => request.is_cancelled(),
        }
    }
}

#[derive(Debug)]
struct ScheduledWorker {
    completed: usize,
}

#[tina_runtime::isolate(message = ScheduledMsg, shard = BridgeShard)]
impl ScheduledWorker {
    fn handle(
        &mut self,
        msg: ScheduledMsg,
        _ctx: &mut Context<'_, BridgeShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ScheduledMsg::Request(request) => {
                sleep(Duration::from_millis(1)).then(move |_| ScheduledMsg::Ready(request))
            }
            ScheduledMsg::Ready(request) => {
                let (request, responder) = request.into_parts();
                self.completed += 1;
                let _ = responder.respond(BrushReply {
                    line: format!("{} scheduled {}", request.llama, self.completed),
                });
                noop()
            }
        }
    }
}

#[derive(Debug)]
enum FileBridgeMsg {
    Request(BridgeRequest<BrushRequest, BrushReply>),
    Opened {
        result: Result<FileId, tina_runtime::CallError>,
        request: BridgeRequest<BrushRequest, BrushReply>,
        path: PathBuf,
        payload: Vec<u8>,
    },
    Wrote {
        result: Result<usize, tina_runtime::CallError>,
        request: BridgeRequest<BrushRequest, BrushReply>,
        path: PathBuf,
        file: FileId,
        payload_len: usize,
    },
    Synced {
        result: Result<(), tina_runtime::CallError>,
        request: BridgeRequest<BrushRequest, BrushReply>,
        path: PathBuf,
        file: FileId,
    },
    Sized {
        result: Result<u64, tina_runtime::CallError>,
        request: BridgeRequest<BrushRequest, BrushReply>,
        path: PathBuf,
        file: FileId,
    },
    Read {
        result: Result<Vec<u8>, tina_runtime::CallError>,
        request: BridgeRequest<BrushRequest, BrushReply>,
        path: PathBuf,
        file: FileId,
    },
    Closed {
        result: Result<(), tina_runtime::CallError>,
        request: BridgeRequest<BrushRequest, BrushReply>,
        path: PathBuf,
        bytes: Vec<u8>,
    },
}

impl From<BridgeRequest<BrushRequest, BrushReply>> for FileBridgeMsg {
    fn from(request: BridgeRequest<BrushRequest, BrushReply>) -> Self {
        Self::Request(request)
    }
}

impl BridgeMessage for FileBridgeMsg {
    fn bridge_cancelled(&self) -> bool {
        match self {
            Self::Request(request)
            | Self::Opened { request, .. }
            | Self::Wrote { request, .. }
            | Self::Synced { request, .. }
            | Self::Sized { request, .. }
            | Self::Read { request, .. }
            | Self::Closed { request, .. } => request.is_cancelled(),
        }
    }
}

#[derive(Debug)]
struct FileBridgeWorker;

#[tina_runtime::isolate(message = FileBridgeMsg, shard = BridgeShard)]
impl FileBridgeWorker {
    fn handle(
        &mut self,
        msg: FileBridgeMsg,
        _ctx: &mut Context<'_, BridgeShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FileBridgeMsg::Request(request) => {
                let path = std::env::temp_dir().join(format!(
                    "tina-bridge-file-{}-{}.txt",
                    std::process::id(),
                    request.request().llama
                ));
                let payload = format!("{} via file", request.request().llama).into_bytes();
                file_create(path.clone()).then(move |result| FileBridgeMsg::Opened {
                    result,
                    request,
                    path,
                    payload,
                })
            }
            FileBridgeMsg::Opened {
                result: Ok(file),
                request,
                path,
                payload,
            } => {
                let payload_len = payload.len();
                file_write(file, payload).then(move |result| FileBridgeMsg::Wrote {
                    result,
                    request,
                    path,
                    file,
                    payload_len,
                })
            }
            FileBridgeMsg::Wrote {
                result: Ok(count),
                request,
                path,
                file,
                payload_len,
            } if count == payload_len => {
                file_fsync(file).then(move |result| FileBridgeMsg::Synced {
                    result,
                    request,
                    path,
                    file,
                })
            }
            FileBridgeMsg::Synced {
                result: Ok(()),
                request,
                path,
                file,
            } => file_size(file).then(move |result| FileBridgeMsg::Sized {
                result,
                request,
                path,
                file,
            }),
            FileBridgeMsg::Sized {
                result: Ok(size),
                request,
                path,
                file,
            } => file_read(file, size as usize).then(move |result| FileBridgeMsg::Read {
                result,
                request,
                path,
                file,
            }),
            FileBridgeMsg::Read {
                result: Ok(bytes),
                request,
                path,
                file,
            } => file_close(file).then(move |result| FileBridgeMsg::Closed {
                result,
                request,
                path,
                bytes,
            }),
            FileBridgeMsg::Closed {
                result: Ok(()),
                request,
                path,
                bytes,
            } => {
                let _ = fs::remove_file(path);
                let line = String::from_utf8(bytes).expect("bridge file payload utf8");
                let (_, responder) = request.into_parts();
                let _ = responder.respond(BrushReply { line });
                noop()
            }
            FileBridgeMsg::Opened { request, .. }
            | FileBridgeMsg::Wrote { request, .. }
            | FileBridgeMsg::Synced { request, .. }
            | FileBridgeMsg::Sized { request, .. }
            | FileBridgeMsg::Read { request, .. }
            | FileBridgeMsg::Closed { request, .. } => {
                let (_, responder) = request.into_parts();
                let _ = responder.respond(BrushReply {
                    line: "file failed".to_string(),
                });
                noop()
            }
        }
    }
}

#[tokio::test]
async fn bridge_host_can_register_runtime_call_service_message_enum() {
    let host = BridgeHost::new(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let bridge = host
        .register_bridge::<ScheduledWorker, BrushRequest, BrushReply, Infallible>(
            ScheduledWorker { completed: 0 },
            8,
            Duration::from_secs(1),
        )
        .expect("register scheduled worker");

    assert_eq!(
        bridge.call(BrushRequest { llama: "Tina" }).await,
        Ok(BrushReply {
            line: "Tina scheduled 1".to_string()
        })
    );

    drop(bridge);
    let _ = host.shutdown().expect("host shutdown");
}

#[tokio::test]
async fn bridge_hosted_service_can_use_runtime_owned_file_io() {
    let host = BridgeHost::new(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let bridge = host
        .register_bridge::<FileBridgeWorker, BrushRequest, BrushReply, Infallible>(
            FileBridgeWorker,
            8,
            Duration::from_secs(1),
        )
        .expect("register file bridge worker");

    assert_eq!(
        bridge.call(BrushRequest { llama: "Tina" }).await,
        Ok(BrushReply {
            line: "Tina via file".to_string()
        })
    );

    drop(bridge);
    let _ = host.shutdown().expect("host shutdown");
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
        _ctx: &mut Context<'_, BridgeShard, Self::Reply>,
    ) -> Effect<Self> {
        let (request, responder) = msg.into_parts();
        self.entered.send(()).expect("entered signal");
        self.release.recv().expect("release signal");
        let _ = responder.respond(GateReply(request.0));
        noop()
    }
}

#[tokio::test]
async fn bridge_does_not_count_preflight_cancel_as_closed_after_caller_timeout() {
    let runtime = Arc::new(ThreadedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
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

    tokio::time::sleep(Duration::from_millis(20)).await;

    assert_eq!(full_bridge.metrics().attempts, 1);
    assert_eq!(full_bridge.metrics().timeout, 1);
    assert_eq!(full_bridge.metrics().closed, 0);

    drop(full_bridge);
    let runtime = match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime,
        Err(_) => panic!("bridge handles dropped"),
    };
    let _ = runtime.shutdown().expect("runtime shutdown");
}

#[tokio::test]
async fn bridge_host_skips_cancelled_queued_request_before_user_state_mutates() {
    let host = BridgeHost::new(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let blocker = host
        .register_bridge::<BlockingWorker, GateRequest, GateReply, Infallible>(
            BlockingWorker {
                entered: entered_tx,
                release: release_rx,
            },
            8,
            Duration::from_secs(5),
        )
        .expect("register blocking worker");
    let scheduled = host
        .register_bridge::<ScheduledWorker, BrushRequest, BrushReply, Infallible>(
            ScheduledWorker { completed: 0 },
            8,
            Duration::from_secs(1),
        )
        .expect("register scheduled worker");

    let blocker_task = tokio::spawn(async move { blocker.call(GateRequest("released")).await });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(1)))
        .await
        .expect("entered wait task")
        .expect("blocking handler entered");

    assert_eq!(
        scheduled
            .call_with_timeout(BrushRequest { llama: "Tina" }, Duration::from_millis(1))
            .await,
        Err(BridgeError::Timeout)
    );

    release_tx.send(()).expect("release blocking handler");
    assert_eq!(
        blocker_task.await.expect("blocker task"),
        Ok(GateReply("released"))
    );

    assert_eq!(
        scheduled.call(BrushRequest { llama: "Tina" }).await,
        Ok(BrushReply {
            line: "Tina scheduled 1".to_string()
        })
    );

    drop(scheduled);
    let _ = host.shutdown().expect("host shutdown");
}

#[tokio::test]
async fn bridge_timeout_is_explicit_when_tina_keeps_responder_open() {
    let runtime = Arc::new(ThreadedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
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
    entered: SyncSender<()>,
}

#[tina::isolate(
    message = BridgeRequest<GateRequest, GateReply>,
    shard = BridgeShard
)]
impl CapturingWorker {
    fn handle(
        &mut self,
        msg: BridgeRequest<GateRequest, GateReply>,
        _ctx: &mut Context<'_, BridgeShard, Self::Reply>,
    ) -> Effect<Self> {
        let (_request, responder) = msg.into_parts();
        *self.captured.lock().expect("captured responder lock") = Some(responder);
        self.entered.send(()).expect("entered signal");
        noop()
    }
}

#[tokio::test]
async fn bridge_caller_timeout_closes_responder_and_counts_late_response() {
    let runtime = Arc::new(ThreadedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let captured = Arc::new(Mutex::new(None));
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let address = runtime
        .register_with_capacity::<CapturingWorker, Infallible>(
            CapturingWorker {
                captured: Arc::clone(&captured),
                entered: entered_tx,
            },
            8,
        )
        .expect("register capturing worker");
    let bridge = BridgeHandle::new(Arc::clone(&runtime), address, Duration::from_millis(10));

    let bridge_call = bridge.clone();
    let call_task = tokio::spawn(async move { bridge_call.call(GateRequest("held")).await });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(1)))
        .await
        .expect("entered wait task")
        .expect("capturing handler entered");
    assert_eq!(
        call_task.await.expect("bridge call"),
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
        _ctx: &mut Context<'_, BridgeShard, Self::Reply>,
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
    let runtime = Arc::new(ThreadedRuntime::with_config(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
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
