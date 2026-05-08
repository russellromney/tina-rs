//! Tracing emission for `tina-reqwest-bridge`.
//!
//! Admission events fire before any HTTP work, so these tests avoid
//! the heavier hyper fake-server setup and target the close path.

#![cfg(feature = "tracing")]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_reqwest_bridge::{
    InstalledReqwestBridge, ReqwestAddress, ReqwestConfig, ReqwestError, ReqwestRequest,
    ReqwestResponse, ReqwestWorker, send_request,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig,
};
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
    fn field(&self, name: &str) -> Option<&str> { self.fields.get(name).map(String::as_str) }
    fn kind(&self) -> Option<&str> { self.field("kind") }
}

#[derive(Debug, Clone, Default)]
struct Capture { events: Arc<Mutex<Vec<CapturedEvent>>> }

impl Capture {
    fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().expect("capture lock").clone()
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool { true }
    fn new_span(&self, _attrs: &Attributes<'_>) -> Id { Id::from_u64(1) }
    fn record(&self, _span: &Id, _values: &Record<'_>) {}
    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events.lock().expect("capture lock").push(CapturedEvent {
            target: metadata.target().to_string(),
            level: *metadata.level(),
            fields: visitor.fields,
        });
    }
    fn enter(&self, _span: &Id) {}
    fn exit(&self, _span: &Id) {}
}

#[derive(Default)]
struct FieldVisitor { fields: BTreeMap<String, String> }

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields.insert(field.name().to_string(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(field.name().to_string(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_string(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_string(), value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().to_string(), value.to_string());
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

// --- minimal caller harness: fires one request, drops outcome into a Sink. ---

type Outcome = CallOutcome<Result<ReqwestResponse, ReqwestError>>;

#[derive(Default)]
struct Sink {
    state: Mutex<Option<Outcome>>,
    cv: Condvar,
}

impl Sink {
    fn put(&self, outcome: Outcome) {
        *self.state.lock().expect("sink lock") = Some(outcome);
        self.cv.notify_all();
    }
    fn wait(&self, timeout: Duration) -> Outcome {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().expect("sink lock");
        while guard.is_none() {
            let now = Instant::now();
            if now >= deadline {
                panic!("caller did not complete within {timeout:?}");
            }
            let (g, _) = self
                .cv
                .wait_timeout(guard, deadline - now)
                .expect("sink wait");
            guard = g;
        }
        guard.take().unwrap()
    }
}

#[derive(Debug)]
enum CallerMsg {
    Run(ReqwestRequest),
    Done(Outcome),
}

struct Caller {
    bridge: ReqwestAddress,
    sink: Arc<Sink>,
}

impl Isolate for Caller {
    tina::isolate_types! {
        message: CallerMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<CallerMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: CallerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CallerMsg::Run(request) => {
                send_request(self.bridge, request, Duration::from_secs(2))
                    .reply(CallerMsg::Done)
            }
            CallerMsg::Done(outcome) => {
                self.sink.put(outcome);
                stop()
            }
        }
    }
}

fn make_runtime() -> ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig::default(),
    )
}

fn install_bridge(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
) -> InstalledReqwestBridge<SingleShard> {
    ReqwestWorker::install(runtime, ReqwestConfig::default()).expect("install reqwest bridge")
}

fn register_caller(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    bridge: ReqwestAddress,
    sink: Arc<Sink>,
) -> Address<CallerMsg, ()> {
    runtime
        .register_with_capacity::<Caller, Infallible>(Caller { bridge, sink }, 8)
        .expect("register caller")
}

#[test]
fn admission_rejected_after_close_emits_aligned_reason() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let runtime = make_runtime();
    let bridge = install_bridge(&runtime);
    bridge.closer.close();

    let sink = Arc::new(Sink::default());
    let caller = register_caller(&runtime, bridge.address, Arc::clone(&sink));
    runtime
        .try_send(caller, CallerMsg::Run(ReqwestRequest::get("http://example/")))
        .expect("kick caller");
    let outcome = sink.wait(Duration::from_secs(5));
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Err(ReqwestError::Closed))
    ));
    let _ = runtime.shutdown();

    let new = capture.events()[baseline..].to_vec();
    let close = new
        .iter()
        .find(|e| e.kind() == Some("close") && e.target == "tina_reqwest.bridge")
        .expect("close lifecycle event");
    assert_eq!(close.level, Level::DEBUG);

    let rejected = new
        .iter()
        .find(|e| {
            e.target == "tina_reqwest.bridge.call"
                && e.kind() == Some("admission_rejected")
                && e.field("reason") == Some("Closed")
        })
        .expect("admission_rejected reason=Closed event");
    assert_eq!(rejected.level, Level::WARN);
    assert_eq!(rejected.field("method"), Some("GET"));
}

#[test]
fn oversized_request_body_emits_request_too_large() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let runtime = make_runtime();
    let cfg = ReqwestConfig::default().with_request_body_limit(8);
    let bridge = ReqwestWorker::install(&runtime, cfg).expect("install bridge");

    let sink = Arc::new(Sink::default());
    let caller = register_caller(&runtime, bridge.address, Arc::clone(&sink));
    let big_body = vec![b'x'; 64];
    runtime
        .try_send(
            caller,
            CallerMsg::Run(ReqwestRequest::post("http://example/", big_body)),
        )
        .expect("kick caller");
    let outcome = sink.wait(Duration::from_secs(5));
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Err(ReqwestError::RequestTooLarge))
    ));
    let _ = runtime.shutdown();

    let new = capture.events()[baseline..].to_vec();
    let rejected = new
        .iter()
        .find(|e| {
            e.target == "tina_reqwest.bridge.call"
                && e.kind() == Some("admission_rejected")
                && e.field("reason") == Some("RequestTooLarge")
        })
        .expect("admission_rejected RequestTooLarge event");
    assert_eq!(rejected.level, Level::WARN);
    assert_eq!(rejected.field("method"), Some("POST"));
}
