//! Tracing emission for `tina-aws-bridge` SQS worker.
//!
//! Pins the convention rule that SQS emits the same `tina_aws.bridge` /
//! `tina_aws.bridge.call` event vocabulary as S3 (close, admitted,
//! timeout, admission_rejected). Bridge events fire on the shard
//! worker thread, so the subscriber is installed as the *global*
//! default and the assertions read from a per-test baseline offset.

#![cfg(feature = "tracing")]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tina::SingleShard;
use tina::prelude::*;
use tina_aws_bridge::{
    SqsAddress, SqsConfig, SqsCredentials, SqsMsg, SqsRequest, SqsSendMessage, install_sqs,
};
use tina_runtime::{
    DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig, call,
};
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

#[derive(Clone, Debug)]
struct CapturedEvent {
    target: String,
    fields: BTreeMap<String, String>,
}

impl CapturedEvent {
    fn kind(&self) -> Option<&str> {
        self.fields.get("kind").map(String::as_str)
    }

    fn reason(&self) -> Option<&str> {
        self.fields.get("reason").map(String::as_str)
    }
}

#[derive(Default, Clone)]
struct Capture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl Capture {
    fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().expect("capture lock").clone()
    }
}

#[derive(Default)]
struct FieldVisitor {
    fields: BTreeMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
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
        if !metadata.target().starts_with("tina_aws.") {
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("capture lock")
            .push(CapturedEvent {
                target: metadata.target().to_string(),
                fields: visitor.fields,
            });
    }
    fn enter(&self, _span: &Id) {}
    fn exit(&self, _span: &Id) {}
}

fn install_global_capture() -> Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let capture = Capture::default();
            // First test to run wins the global; later tests share it.
            // `set_global_default` returns Err if anything already set
            // a global, which is harmless here.
            let _ = tracing::subscriber::set_global_default(capture.clone());
            capture
        })
        .clone()
}

fn make_runtime() -> ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig::default(),
    )
}

fn sqs_config() -> SqsConfig {
    SqsConfig::new()
        .with_region("us-east-1")
        .with_endpoint_url("http://127.0.0.1:1") // unreachable; admission rejects before network
        .with_credentials(SqsCredentials::new("ak", "sk"))
        .with_mailbox_capacity(4)
        .with_max_in_flight(1)
        .with_message_body_limit(64)
        .with_max_receive_messages(1)
        .with_default_timeout(Duration::from_secs(1))
        .with_poll_interval(Duration::from_millis(5))
}

type SqsResult = Result<tina_aws_bridge::SqsResponse, tina_aws_bridge::SqsError>;
type SqsCallOutcome = tina_runtime::CallOutcome<SqsResult>;
type SinkSlot = Arc<Mutex<Option<SqsCallOutcome>>>;

/// Tiny caller isolate that issues one `SqsMsg::Send` and stops once it
/// has the outcome. Stays in this test file so it shares the message
/// type with the worker.
struct Driver {
    target: SqsAddress,
    request: Option<SqsRequest>,
    sink: SinkSlot,
}

enum DriverMsg {
    Kick,
    Done(SqsCallOutcome),
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<DriverMsg>,
        shard: SingleShard,
    }

    fn handle(&mut self, msg: DriverMsg, _ctx: &mut Context<'_, SingleShard, ()>) -> Effect<Self> {
        match msg {
            DriverMsg::Kick => {
                let request = self.request.take().expect("kick once");
                call(self.target, SqsMsg::Send(request), Duration::from_secs(2))
                    .then(DriverMsg::Done)
            }
            DriverMsg::Done(outcome) => {
                *self.sink.lock().expect("sink lock") = Some(outcome);
                stop()
            }
        }
    }
}

fn run_one_send(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    target: SqsAddress,
    request: SqsRequest,
) -> SqsCallOutcome {
    let sink: SinkSlot = Arc::new(Mutex::new(None));
    let driver = Driver {
        target,
        request: Some(request),
        sink: Arc::clone(&sink),
    };
    let addr = runtime
        .register_with_capacity::<Driver, Infallible>(driver, 4)
        .expect("register driver");
    runtime.try_send(addr, DriverMsg::Kick).expect("kick");

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(outcome) = sink.lock().expect("sink lock").take() {
            return outcome;
        }
        if std::time::Instant::now() >= deadline {
            panic!("driver did not produce outcome");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn sqs_close_emits_bridge_lifecycle_event() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let runtime = make_runtime();
    let bridge = install_sqs(&runtime, sqs_config()).expect("install_sqs");

    bridge.closer.close();
    // Idempotent re-close: the second call must not emit a second
    // event for THIS bridge. We can't assert "exactly one in the
    // captured slice" because the global capture sees concurrent
    // tests' closes too; the no-double-emit guarantee is enforced by
    // the `was_closed = swap()` guard in SqsCloser::close and pinned
    // by code review.
    bridge.closer.close();

    let new = capture.events()[baseline..].to_vec();
    assert!(
        new.iter()
            .any(|e| e.target == "tina_aws.bridge" && e.kind() == Some("close")),
        "expected a close event on tina_aws.bridge; saw events: {new:?}"
    );
}

#[test]
fn sqs_admission_rejected_after_close_carries_reason_closed() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let runtime = make_runtime();
    let bridge = install_sqs(&runtime, sqs_config()).expect("install_sqs");
    bridge.closer.close();

    let outcome = run_one_send(
        &runtime,
        bridge.address,
        SqsRequest::SendMessage(SqsSendMessage {
            queue_url: "https://sqs.us-east-1.amazonaws.com/123/q".into(),
            body: "x".into(),
            message_group_id: None,
            message_deduplication_id: None,
        }),
    );
    assert!(
        matches!(
            outcome,
            tina_runtime::CallOutcome::Replied(Err(tina_aws_bridge::SqsError::Closed))
        ),
        "expected SqsError::Closed after close; got {outcome:?}"
    );

    let new = capture.events()[baseline..].to_vec();
    let rejected = new
        .iter()
        .find(|e| {
            e.target == "tina_aws.bridge.call"
                && e.kind() == Some("admission_rejected")
                && e.reason() == Some("Closed")
        })
        .unwrap_or_else(|| {
            panic!("expected admission_rejected(Closed) on tina_aws.bridge.call; saw {new:?}")
        });
    assert_eq!(
        rejected.fields.get("request_kind").map(String::as_str),
        Some("sqs_send_message")
    );
}

#[test]
fn sqs_admission_rejected_invalid_request_carries_reason() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let runtime = make_runtime();
    let bridge = install_sqs(&runtime, sqs_config()).expect("install_sqs");

    // Empty queue_url fails validate_request before any SDK call.
    let outcome = run_one_send(
        &runtime,
        bridge.address,
        SqsRequest::SendMessage(SqsSendMessage {
            queue_url: String::new(),
            body: "x".into(),
            message_group_id: None,
            message_deduplication_id: None,
        }),
    );
    assert!(
        matches!(
            outcome,
            tina_runtime::CallOutcome::Replied(Err(tina_aws_bridge::SqsError::InvalidRequest(_)))
        ),
        "expected SqsError::InvalidRequest for empty queue url; got {outcome:?}"
    );

    let new = capture.events()[baseline..].to_vec();
    let rejected = new
        .iter()
        .find(|e| {
            e.target == "tina_aws.bridge.call"
                && e.kind() == Some("admission_rejected")
                && e.reason() == Some("InvalidRequest")
        })
        .unwrap_or_else(|| {
            panic!(
                "expected admission_rejected(InvalidRequest) on tina_aws.bridge.call; saw {new:?}"
            )
        });
    assert_eq!(
        rejected.fields.get("request_kind").map(String::as_str),
        Some("sqs_send_message")
    );
}
