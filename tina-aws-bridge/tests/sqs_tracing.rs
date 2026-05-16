//! Tracing emission for `tina-aws-bridge` SQS worker.
//!
//! Pins the convention rule that SQS emits on the same `tina_aws.bridge`
//! lifecycle target as S3. Without this test the bridge convention table
//! in `docs/tina-user-guide/18-bridge-crates.md` could drift back to
//! "SQS is silent."

#![cfg(feature = "tracing")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::SingleShard;
use tina_aws_bridge::{SqsConfig, SqsCredentials, install_sqs};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};
use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

#[derive(Clone, Debug)]
struct CapturedEvent {
    target: String,
    fields: BTreeMap<String, String>,
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
        let _ = metadata.level();
        let _ = Level::DEBUG;
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
        .with_endpoint_url("http://127.0.0.1:1") // never reached; we only close
        .with_credentials(SqsCredentials::new("ak", "sk"))
        .with_mailbox_capacity(2)
        .with_max_in_flight(1)
        .with_message_body_limit(64)
        .with_max_receive_messages(1)
        .with_default_timeout(Duration::from_secs(1))
        .with_poll_interval(Duration::from_millis(5))
}

#[test]
fn sqs_close_emits_bridge_lifecycle_event() {
    let capture = Capture::default();
    let dispatch = tracing::Dispatch::new(capture.clone());

    let _guard = tracing::dispatcher::set_default(&dispatch);

    let runtime = make_runtime();
    let bridge = install_sqs(&runtime, sqs_config()).expect("install_sqs");

    bridge.closer.close();

    // Idempotent re-close must not emit a second event.
    bridge.closer.close();

    drop(_guard);

    let events = capture.events();
    let close_events: Vec<_> = events
        .iter()
        .filter(|e| {
            e.target == "tina_aws.bridge"
                && e.fields.get("kind").map(String::as_str) == Some("close")
        })
        .collect();
    assert_eq!(
        close_events.len(),
        1,
        "expected exactly one close event on tina_aws.bridge; saw events: {events:?}"
    );
}
