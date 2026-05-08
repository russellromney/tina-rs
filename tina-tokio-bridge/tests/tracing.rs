//! Tracing emission for `tina-tokio-bridge`.

#![cfg(feature = "tracing")]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntimeConfig};
use tina_tokio_bridge::{BridgeError, BridgeHost, BridgeMessage, BridgeRequest};
use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

#[derive(Debug, Clone)]
struct CapturedEvent {
    target: String,
    level: Level,
    fields: BTreeMap<String, String>,
}

impl CapturedEvent {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
    fn kind(&self) -> Option<&str> {
        self.field("kind")
    }
}

#[derive(Debug, Clone, Default)]
struct Capture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl Capture {
    fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().expect("capture lock").clone()
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _attrs: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _span: &Id, _values: &Record<'_>) {}
    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("capture lock")
            .push(CapturedEvent {
                target: metadata.target().to_string(),
                level: *metadata.level(),
                fields: visitor.fields,
            });
    }
    fn enter(&self, _span: &Id) {}
    fn exit(&self, _span: &Id) {}
}

#[derive(Default)]
struct FieldVisitor {
    fields: BTreeMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
}

static GLOBAL_CAPTURE: OnceLock<Capture> = OnceLock::new();

fn install_global_capture() -> &'static Capture {
    GLOBAL_CAPTURE.get_or_init(|| {
        let capture = Capture::default();
        let _ = tracing::subscriber::set_global_default(capture.clone());
        capture
    })
}

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

#[tokio::test(flavor = "current_thread")]
async fn close_then_call_emits_admission_rejected_with_aligned_reason() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let mut host = make_host();
    let handle = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            8,
            Duration::from_millis(100),
        )
        .expect("register bridge");
    handle.close();
    let result = handle.call(7).await;
    assert!(matches!(result, Err(BridgeError::Closed)));
    drop(handle);
    let _ = host.drain_and_shutdown(Duration::from_millis(200));

    let new = &capture.events()[baseline..];
    let close = new
        .iter()
        .find(|e| e.kind() == Some("close"))
        .expect("close lifecycle event");
    assert_eq!(close.target, "tina_tokio.bridge");
    assert_eq!(close.level, Level::DEBUG);

    let rejected = new
        .iter()
        .find(|e| e.kind() == Some("admission_rejected"))
        .expect("admission_rejected event");
    assert_eq!(rejected.target, "tina_tokio.bridge.call");
    assert_eq!(rejected.level, Level::WARN);
    assert_eq!(rejected.field("reason"), Some("Closed"));
}

#[tokio::test(flavor = "current_thread")]
async fn happy_path_emits_admitted_then_replied() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let mut host = make_host();
    let handle = host
        .register_bridge::<EchoIsolate, u32, u32, Infallible>(
            EchoIsolate,
            8,
            Duration::from_millis(500),
        )
        .expect("register bridge");
    let response = handle.call(42).await.expect("call ok");
    assert_eq!(response, 42);
    drop(handle);
    let _ = host.drain_and_shutdown(Duration::from_millis(200));

    let new = capture.events()[baseline..].to_vec();
    assert!(
        new.iter().any(|e| e.kind() == Some("admitted")
            && e.target == "tina_tokio.bridge.call"
            && e.level == Level::DEBUG),
        "expected admitted event, got {:?}",
        new.iter().filter_map(|e| e.kind()).collect::<Vec<_>>(),
    );
    assert!(
        new.iter()
            .any(|e| e.kind() == Some("replied") && e.field("outcome") == Some("response")),
        "expected replied event, got {:?}",
        new.iter().filter_map(|e| e.kind()).collect::<Vec<_>>(),
    );
}

/// Black-hole isolate: keeps every request alive without responding
/// so the bridge per-attempt timeout fires. The isolate's own Drop
/// later releases the responders.
#[derive(Debug, Default)]
struct StuckIsolate {
    stuck: Vec<BridgeRequest<u32, u32>>,
}

#[derive(Debug)]
enum StuckMsg {
    Request(BridgeRequest<u32, u32>),
}

impl From<BridgeRequest<u32, u32>> for StuckMsg {
    fn from(value: BridgeRequest<u32, u32>) -> Self {
        Self::Request(value)
    }
}

impl BridgeMessage for StuckMsg {
    fn bridge_cancelled(&self) -> bool {
        match self {
            Self::Request(request) => request.bridge_cancelled(),
        }
    }
}

#[tina_runtime::isolate(message = StuckMsg)]
impl StuckIsolate {
    fn handle(
        &mut self,
        msg: StuckMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            StuckMsg::Request(request) => {
                self.stuck.push(request);
                noop()
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn per_attempt_timeout_event_carries_scope_and_elapsed_ms() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let mut host = make_host();
    let handle = host
        .register_bridge::<StuckIsolate, u32, u32, Infallible>(
            StuckIsolate { stuck: Vec::new() },
            8,
            Duration::from_millis(50),
        )
        .expect("register bridge");
    let result = handle.call(7).await;
    assert!(matches!(result, Err(BridgeError::Timeout)));
    drop(handle);
    let _ = host.drain_and_shutdown(Duration::from_millis(200));

    let new = capture.events()[baseline..].to_vec();
    let timeout = new
        .iter()
        .find(|e| {
            e.target == "tina_tokio.bridge.call"
                && e.kind() == Some("timeout")
                && e.field("scope") == Some("per_attempt")
        })
        .expect("per-attempt timeout event with scope=per_attempt");
    assert_eq!(timeout.level, Level::WARN);
    assert_eq!(timeout.field("reason"), Some("Timeout"));
    assert!(timeout.field("elapsed_ms").is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn retry_within_total_timeout_emits_event_with_scope() {
    use tina_tokio_bridge::BridgeBackpressure;

    let capture = install_global_capture();
    let baseline = capture.events().len();

    let mut host = make_host();
    // StuckIsolate never replies. We use call_with_policy(RetryWithin)
    // with a per-attempt timeout longer than the total timeout, so
    // the per-attempt path cannot fire first — the only timeout
    // signal must come from the RetryWithin total-budget site.
    let handle = host
        .register_bridge::<StuckIsolate, u32, u32, Infallible>(
            StuckIsolate { stuck: Vec::new() },
            8,
            Duration::from_millis(500),
        )
        .expect("register bridge");
    let policy =
        BridgeBackpressure::retry_within(99, Duration::from_millis(1), Duration::from_millis(20));
    let result = handle
        .call_with_policy(7, policy, Duration::from_millis(500))
        .await;
    assert!(matches!(result, Err(BridgeError::Timeout)));
    drop(handle);
    let _ = host.drain_and_shutdown(Duration::from_millis(200));

    let new = capture.events()[baseline..].to_vec();
    let total = new
        .iter()
        .find(|e| {
            e.target == "tina_tokio.bridge.call"
                && e.kind() == Some("timeout")
                && e.field("scope") == Some("retry_within_total")
        })
        .expect("retry_within total-budget timeout event");
    assert_eq!(total.level, Level::WARN);
    assert_eq!(total.field("reason"), Some("Timeout"));
    assert!(total.field("elapsed_ms").is_some());
}
