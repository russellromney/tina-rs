//! Tracing emission for `tina-sqlite-bridge`.
//!
//! Bridge events fire on the shard worker thread. Capture is process-
//! global so the assertions can read them from the test thread.

#![cfg(feature = "tracing")]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig,
};
use tina_sqlite_bridge::{
    InstalledSqliteBridge, SqliteAddress, SqliteCallOutcome, SqliteConfig, SqliteError,
    SqliteRequest, SqliteResponse, SqliteWorker, send_request,
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

// ---------------------------------------------------------------------------
// Caller harness — minimal copy of the patterns in tests/bridge.rs that
// keeps tracing-only test scope from depending on bridge.rs.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Sink {
    state: Mutex<Vec<SqliteCallOutcome>>,
    cv: Condvar,
}

impl Sink {
    fn put(&self, outcome: SqliteCallOutcome) {
        self.state.lock().expect("sink lock").push(outcome);
        self.cv.notify_all();
    }

    fn wait_one(&self, timeout: Duration) -> SqliteCallOutcome {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().expect("sink lock");
        while guard.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                panic!("sink wait timed out after {timeout:?}");
            }
            let (g, _) = self
                .cv
                .wait_timeout(guard, deadline - now)
                .expect("sink wait");
            guard = g;
        }
        guard.remove(0)
    }
}

#[derive(Debug)]
enum CallerMsg {
    Run(SqliteRequest),
    Done(SqliteCallOutcome),
}

struct Caller {
    bridge: SqliteAddress,
    deadline: Duration,
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
                send_request(self.bridge, request, self.deadline).reply(CallerMsg::Done)
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
    config: SqliteConfig,
) -> InstalledSqliteBridge<SingleShard> {
    SqliteWorker::install(runtime, config).expect("install bridge")
}

fn register_caller(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    bridge: SqliteAddress,
    sink: Arc<Sink>,
    deadline: Duration,
) -> Address<CallerMsg, ()> {
    runtime
        .register_with_capacity::<Caller, Infallible>(
            Caller {
                bridge,
                deadline,
                sink,
            },
            8,
        )
        .expect("register caller")
}

fn fresh_db_config() -> SqliteConfig {
    SqliteConfig::memory()
        .with_default_timeout(Duration::from_secs(2))
        .with_poll_interval(Duration::from_millis(2))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn admission_and_replied_emit_with_aligned_vocabulary() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, fresh_db_config());
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(5),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::execute(
                "CREATE TABLE t (k INTEGER PRIMARY KEY)",
            )),
        )
        .expect("kick caller");
    let outcome = sink.wait_one(Duration::from_secs(5));
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Ok(SqliteResponse::Executed { .. }))
    ));
    let _ = runtime.shutdown();

    let new = &capture.events()[baseline..];
    let admitted = new
        .iter()
        .find(|e| e.kind() == Some("admitted") && e.field("request_kind") == Some("execute"))
        .expect("admitted event for execute");
    assert_eq!(admitted.target, "tina_sqlite.bridge.call");
    assert_eq!(admitted.level, Level::DEBUG);

    let replied = new
        .iter()
        .find(|e| {
            e.kind() == Some("replied")
                && e.field("outcome") == Some("executed")
                && e.field("request_kind") == Some("execute")
        })
        .expect("replied event for execute");
    assert_eq!(replied.level, Level::DEBUG);
    assert!(replied.field("rows_changed").is_some());
}

#[test]
fn replied_event_carries_request_kind_for_query() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, fresh_db_config());

    // Seed an empty table so the query is structurally valid.
    let setup = Arc::new(Sink::default());
    let setup_caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&setup),
        Duration::from_secs(5),
    );
    runtime
        .try_send(
            setup_caller,
            CallerMsg::Run(SqliteRequest::execute(
                "CREATE TABLE q (k INTEGER PRIMARY KEY)",
            )),
        )
        .expect("kick setup");
    let _ = setup.wait_one(Duration::from_secs(5));

    // Now run a query and assert request_kind=query rides on the replied event.
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(5),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::query_rows("SELECT * FROM q", 16)),
        )
        .expect("kick caller");
    let outcome = sink.wait_one(Duration::from_secs(5));
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Ok(SqliteResponse::Rows { .. }))
    ));
    let _ = runtime.shutdown();

    let new = capture.events()[baseline..].to_vec();
    let replied = new
        .iter()
        .find(|e| {
            e.kind() == Some("replied")
                && e.field("outcome") == Some("rows")
                && e.field("request_kind") == Some("query")
        })
        .expect("replied event for query carries request_kind");
    assert_eq!(replied.level, Level::DEBUG);
    assert!(replied.field("row_count").is_some());
}

#[test]
fn admission_rejected_after_close_uses_aligned_reason() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, fresh_db_config());
    bridge.closer.close();

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(caller, CallerMsg::Run(SqliteRequest::execute("SELECT 1")))
        .expect("kick caller");
    let outcome = sink.wait_one(Duration::from_secs(5));
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Err(SqliteError::Closed))
    ));
    let _ = runtime.shutdown();

    let new = &capture.events()[baseline..];
    let close = new
        .iter()
        .find(|e| e.kind() == Some("close"))
        .expect("close lifecycle event");
    assert_eq!(close.target, "tina_sqlite.bridge");
    assert_eq!(close.level, Level::DEBUG);

    let rejected = new
        .iter()
        .find(|e| e.kind() == Some("admission_rejected"))
        .expect("admission_rejected event");
    assert_eq!(rejected.target, "tina_sqlite.bridge.call");
    assert_eq!(rejected.level, Level::WARN);
    assert_eq!(rejected.field("reason"), Some("Closed"));
    assert_eq!(rejected.field("request_kind"), Some("execute"));
}

#[test]
fn invalid_request_emits_admission_rejected_with_detail() {
    let capture = install_global_capture();
    let baseline = capture.events().len();

    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, fresh_db_config());
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(caller, CallerMsg::Run(SqliteRequest::execute("")))
        .expect("kick caller");
    let outcome = sink.wait_one(Duration::from_secs(5));
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Err(SqliteError::InvalidRequest(_)))
    ));
    let _ = runtime.shutdown();

    let new = &capture.events()[baseline..];
    let rejected = new
        .iter()
        .find(|e| {
            e.kind() == Some("admission_rejected") && e.field("reason") == Some("InvalidRequest")
        })
        .expect("admission_rejected InvalidRequest event");
    assert_eq!(rejected.level, Level::WARN);
    assert!(rejected.field("detail").is_some());
}

#[test]
fn timeout_event_carries_request_kind() {
    // Tight bridge default_timeout + a recursive CTE that runs longer
    // than the deadline forces a per-attempt timeout. Asserts the
    // emitted timeout event carries `request_kind=query` so operators
    // can filter timeouts by request shape.
    const HEAVY_QUERY: &str = "WITH RECURSIVE seq(x) AS (\
        SELECT 1 UNION ALL SELECT x + 1 FROM seq WHERE x < 1000000\
        ) SELECT SUM(x) FROM seq";

    let capture = install_global_capture();
    let baseline = capture.events().len();

    let runtime = make_runtime();
    let cfg = SqliteConfig::memory()
        .with_default_timeout(Duration::from_millis(20))
        .with_poll_interval(Duration::from_millis(1));
    let bridge = install_bridge(&runtime, cfg);

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(5),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::query_rows(HEAVY_QUERY, 1)),
        )
        .expect("kick caller");
    let outcome = sink.wait_one(Duration::from_secs(5));
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Err(SqliteError::Timeout))
    ));
    let _ = runtime.shutdown();

    let new = capture.events()[baseline..].to_vec();
    let timeout = new
        .iter()
        .find(|e| e.kind() == Some("timeout") && e.field("request_kind") == Some("query"))
        .expect("timeout event carries request_kind=query");
    assert_eq!(timeout.target, "tina_sqlite.bridge.call");
    assert_eq!(timeout.level, Level::WARN);
    assert_eq!(timeout.field("reason"), Some("Timeout"));
    assert!(timeout.field("elapsed_ms").is_some());
}
