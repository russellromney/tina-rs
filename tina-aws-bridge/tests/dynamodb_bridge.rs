//! End-to-end DynamoDB bridge tests against a tiny in-process fake
//! server. These exercise the bridge's admit/poll/reply state machine
//! through a real SDK round trip, plus close/drain, item-size cap,
//! and the supplied-client `sdk_max_attempts = 0` invariant.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request as HyperRequest, Response as HyperResponse, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tina::prelude::*;
use tina_aws_bridge::{
    DynamoAddress, DynamoCallOutcome, DynamoConfig, DynamoConsistency, DynamoCredentials,
    DynamoError, DynamoGetItem, DynamoItem, DynamoPutItem, DynamoRequest, DynamoResponse,
    DynamoValue, DynamoWorker, ReturnCapacity, send_dynamodb,
};
use tina_runtime::{
    DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig,
};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

type Body = Full<Bytes>;

#[derive(Default)]
struct Sink {
    state: Mutex<Option<DynamoCallOutcome>>,
    cv: Condvar,
}

impl Sink {
    fn put(&self, outcome: DynamoCallOutcome) {
        *self.state.lock().expect("sink lock") = Some(outcome);
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) -> DynamoCallOutcome {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().expect("sink lock");
        while guard.is_none() {
            let now = Instant::now();
            if now >= deadline {
                panic!("test caller did not complete within {timeout:?}");
            }
            let (g, _) = self
                .cv
                .wait_timeout(guard, deadline - now)
                .expect("sink wait");
            guard = g;
        }
        guard.take().expect("sink populated")
    }
}

#[derive(Debug)]
enum CallerMsg {
    Run(DynamoRequest),
    Done(DynamoCallOutcome),
}

struct CallerIsolate {
    worker: DynamoAddress,
    timeout: Duration,
    sink: Arc<Sink>,
}

impl Isolate for CallerIsolate {
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
                send_dynamodb(self.worker, request, self.timeout).then(CallerMsg::Done)
            }
            CallerMsg::Done(outcome) => {
                self.sink.put(outcome);
                stop()
            }
        }
    }
}

fn make_runtime() -> Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>> {
    Arc::new(ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ))
}

fn install_bridge(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    config: DynamoConfig,
) -> tina_aws_bridge::InstalledDynamoBridge<SingleShard> {
    DynamoWorker::<SingleShard>::install(runtime, config).expect("install bridge")
}

fn register_caller(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    worker: DynamoAddress,
    sink: Arc<Sink>,
    timeout: Duration,
) -> Address<CallerMsg, ()> {
    runtime
        .register_with_capacity::<_, Infallible>(
            CallerIsolate {
                worker,
                timeout,
                sink,
            },
            8,
        )
        .expect("register caller")
}

fn dynamo_config(endpoint_url: String) -> DynamoConfig {
    DynamoConfig::default()
        .with_endpoint_url(endpoint_url)
        .with_credentials(DynamoCredentials::new("test-access-key", "test-secret-key"))
        .with_poll_interval(Duration::from_millis(2))
        .with_default_timeout(Duration::from_secs(2))
        .with_return_capacity(ReturnCapacity::Total)
}

#[derive(Clone, Default)]
struct FakeState {
    inner: Arc<Mutex<FakeStateInner>>,
    delay: Duration,
}

#[derive(Default)]
struct FakeStateInner {
    request_count: u64,
    seen_targets: Vec<String>,
    items: HashMap<String, Value>,
}

struct FakeDynamoDb {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    runtime: Arc<Runtime>,
    join: Option<tokio::task::JoinHandle<()>>,
    state: FakeState,
}

impl FakeDynamoDb {
    fn spawn() -> Self {
        Self::spawn_with_delay(Duration::ZERO)
    }

    fn spawn_with_delay(delay: Duration) -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1)
                .build()
                .expect("test runtime"),
        );
        let state = FakeState {
            inner: Arc::new(Mutex::new(FakeStateInner::default())),
            delay,
        };
        let state_for_test = state.clone();
        let (addr_tx, addr_rx) = oneshot::channel::<SocketAddr>();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let runtime_for_spawn = Arc::clone(&runtime);
        let join = runtime.spawn(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("local addr");
            addr_tx.send(addr).expect("send addr");
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let (stream, _) = match accepted {
                            Ok(pair) => pair,
                            Err(_) => continue,
                        };
                        let state = state.clone();
                        runtime_for_spawn.spawn(async move {
                            let io = TokioIo::new(stream);
                            let svc = service_fn(move |req| {
                                let state = state.clone();
                                async move { Ok::<_, Infallible>(handle_dynamo(req, state).await) }
                            });
                            let _ = http1::Builder::new().serve_connection(io, svc).await;
                        });
                    }
                }
            }
        });
        let addr = addr_rx.blocking_recv().expect("server address");
        Self {
            addr,
            shutdown: Some(shutdown_tx),
            runtime,
            join: Some(join),
            state: state_for_test,
        }
    }

    fn endpoint_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn request_count(&self) -> u64 {
        self.state.inner.lock().expect("state lock").request_count
    }

    fn seen_targets(&self) -> Vec<String> {
        self.state
            .inner
            .lock()
            .expect("state lock")
            .seen_targets
            .clone()
    }
}

impl Drop for FakeDynamoDb {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            self.runtime.block_on(async move {
                let _ = tokio::time::timeout(Duration::from_secs(2), join).await;
            });
        }
    }
}

async fn handle_dynamo(req: HyperRequest<Incoming>, state: FakeState) -> HyperResponse<Body> {
    if req.method() != Method::POST {
        return response(StatusCode::METHOD_NOT_ALLOWED, json!({}));
    }
    let target = req
        .headers()
        .get("x-amz-target")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = req.into_body().collect().await.expect("body").to_bytes();
    let payload: Value = serde_json::from_slice(&body).expect("dynamodb json request");
    {
        let mut guard = state.inner.lock().expect("state lock");
        guard.request_count += 1;
        guard.seen_targets.push(target.clone());
    }
    if !state.delay.is_zero() {
        tokio::time::sleep(state.delay).await;
    }
    match target.as_str() {
        "DynamoDB_20120810.PutItem" => {
            let item = payload.get("Item").cloned().unwrap_or_else(|| json!({}));
            let key = payload
                .get("Item")
                .and_then(|v| v.get("pk"))
                .and_then(|v| v.get("S"))
                .and_then(Value::as_str)
                .unwrap_or("missing")
                .to_string();
            state
                .inner
                .lock()
                .expect("state lock")
                .items
                .insert(key, item);
            response(
                StatusCode::OK,
                json!({
                    "ConsumedCapacity": {
                        "TableName": "kv",
                        "CapacityUnits": 1.0,
                        "WriteCapacityUnits": 1.0,
                    },
                }),
            )
        }
        "DynamoDB_20120810.GetItem" => {
            let key = payload
                .get("Key")
                .and_then(|v| v.get("pk"))
                .and_then(|v| v.get("S"))
                .and_then(Value::as_str)
                .unwrap_or("missing")
                .to_string();
            let item = state
                .inner
                .lock()
                .expect("state lock")
                .items
                .get(&key)
                .cloned();
            let resp = match item {
                Some(item) => json!({
                    "Item": item,
                    "ConsumedCapacity": {
                        "TableName": "kv",
                        "CapacityUnits": 0.5,
                        "ReadCapacityUnits": 0.5,
                    },
                }),
                None => json!({
                    "ConsumedCapacity": {
                        "TableName": "kv",
                        "CapacityUnits": 0.5,
                        "ReadCapacityUnits": 0.5,
                    },
                }),
            };
            response(StatusCode::OK, resp)
        }
        "DynamoDB_20120810.DeleteItem" => response(StatusCode::OK, json!({})),
        _ => error_response("ValidationException", &format!("unknown target: {target}")),
    }
}

fn response(status: StatusCode, body: Value) -> HyperResponse<Body> {
    HyperResponse::builder()
        .status(status)
        .header("content-type", "application/x-amz-json-1.0")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("response")
}

fn error_response(code: &str, msg: &str) -> HyperResponse<Body> {
    HyperResponse::builder()
        .status(StatusCode::BAD_REQUEST)
        .header("content-type", "application/x-amz-json-1.0")
        .header("x-amzn-ErrorType", code)
        .body(Full::new(Bytes::from(
            json!({ "__type": code, "message": msg }).to_string(),
        )))
        .expect("response")
}

fn make_key(value: &str) -> DynamoItem {
    let mut key = HashMap::new();
    key.insert("pk".to_string(), DynamoValue::S(value.to_string()));
    key
}

fn get_request(value: &str) -> DynamoRequest {
    DynamoRequest::GetItem(DynamoGetItem {
        table_name: "kv".into(),
        key: make_key(value),
        consistency: DynamoConsistency::Eventual,
    })
}

#[test]
fn happy_path_put_then_get_returns_typed_item_and_consumed_capacity() {
    let fake = FakeDynamoDb::spawn();
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, dynamo_config(fake.endpoint_url()));
    let address = bridge.address;

    // Put.
    let put_sink = Arc::new(Sink::default());
    let mut put_item = make_key("tenant#1");
    put_item.insert("v".into(), DynamoValue::N("42".into()));
    let caller = register_caller(&runtime, address, put_sink.clone(), Duration::from_secs(2));
    runtime
        .try_send(
            caller,
            CallerMsg::Run(DynamoRequest::PutItem(DynamoPutItem {
                table_name: "kv".into(),
                item: put_item,
                condition_expression: None,
                expression_attribute_names: HashMap::new(),
                expression_attribute_values: HashMap::new(),
            })),
        )
        .expect("send put");
    let put_outcome = put_sink.wait(Duration::from_secs(3));
    match put_outcome {
        tina_runtime::CallOutcome::Replied(Ok(DynamoResponse::PutItem(ok))) => {
            let cap = ok.consumed_capacity.expect("consumed capacity reported");
            assert_eq!(cap.capacity_units, Some(1.0));
            assert_eq!(cap.write_capacity_units, Some(1.0));
        }
        other => panic!("put expected Replied(Ok), got {other:?}"),
    }

    // Get.
    let get_sink = Arc::new(Sink::default());
    let caller = register_caller(&runtime, address, get_sink.clone(), Duration::from_secs(2));
    runtime
        .try_send(
            caller,
            CallerMsg::Run(DynamoRequest::GetItem(DynamoGetItem {
                table_name: "kv".into(),
                key: make_key("tenant#1"),
                consistency: DynamoConsistency::Eventual,
            })),
        )
        .expect("send get");
    let get_outcome = get_sink.wait(Duration::from_secs(3));
    match get_outcome {
        tina_runtime::CallOutcome::Replied(Ok(DynamoResponse::GetItem(got))) => {
            let item = got.item.expect("item returned");
            let v = item.get("v").expect("v present");
            assert!(matches!(v, DynamoValue::N(n) if n == "42"));
            let cap = got.consumed_capacity.expect("consumed capacity reported");
            assert_eq!(cap.read_capacity_units, Some(0.5));
        }
        other => panic!("get expected Replied(Ok), got {other:?}"),
    }

    assert_eq!(fake.request_count(), 2);
    let mut targets = fake.seen_targets();
    targets.sort();
    assert_eq!(
        targets,
        vec![
            "DynamoDB_20120810.GetItem".to_string(),
            "DynamoDB_20120810.PutItem".to_string(),
        ]
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn item_size_cap_rejects_request_before_sdk() {
    let fake = FakeDynamoDb::spawn();
    let runtime = make_runtime();
    // Very small cap so an ordinary item overflows.
    let bridge = install_bridge(
        &runtime,
        dynamo_config(fake.endpoint_url()).with_item_size_limit(4),
    );

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        sink.clone(),
        Duration::from_secs(1),
    );
    let mut item = make_key("very-long-partition-key");
    item.insert("v".into(), DynamoValue::S("x".repeat(1000)));
    runtime
        .try_send(
            caller,
            CallerMsg::Run(DynamoRequest::PutItem(DynamoPutItem {
                table_name: "kv".into(),
                item,
                condition_expression: None,
                expression_attribute_names: HashMap::new(),
                expression_attribute_values: HashMap::new(),
            })),
        )
        .expect("send put");

    let outcome = sink.wait(Duration::from_secs(2));
    assert!(
        matches!(
            outcome,
            tina_runtime::CallOutcome::Replied(Err(DynamoError::ItemTooLarge))
        ),
        "expected ItemTooLarge, got {outcome:?}"
    );
    // The SDK never saw this request — the cap fired at admission.
    assert_eq!(
        fake.request_count(),
        0,
        "item-size cap must reject before SDK work begins"
    );

    let metrics = bridge.metrics.snapshot();
    assert!(metrics.item_too_large >= 1);
    assert_eq!(metrics.admitted, 0);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn max_in_flight_rejects_full_and_pressure_reports() {
    let fake = FakeDynamoDb::spawn_with_delay(Duration::from_millis(250));
    let runtime = make_runtime();
    let bridge = install_bridge(
        &runtime,
        dynamo_config(fake.endpoint_url()).with_max_in_flight(1),
    );
    let first_sink = Arc::new(Sink::default());
    let second_sink = Arc::new(Sink::default());
    let first = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&first_sink),
        Duration::from_secs(2),
    );
    let second = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&second_sink),
        Duration::from_secs(2),
    );

    runtime
        .try_send(first, CallerMsg::Run(get_request("first")))
        .expect("first");
    std::thread::sleep(Duration::from_millis(25));
    runtime
        .try_send(second, CallerMsg::Run(get_request("second")))
        .expect("second");

    match second_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Err(DynamoError::Full)) => {}
        other => panic!("expected worker Full, got {other:?}"),
    }
    match first_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Ok(DynamoResponse::GetItem(_))) => {}
        other => panic!("expected first success, got {other:?}"),
    }
    let pressure = bridge.metrics.pressure_report();
    assert_eq!(pressure.capacity, 1);
    assert_eq!(pressure.full_count, 1);
    assert_eq!(pressure.high_water, 1);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn close_and_drain_reports_remaining_then_drained() {
    let fake = FakeDynamoDb::spawn_with_delay(Duration::from_millis(180));
    let runtime = make_runtime();
    let bridge = install_bridge(
        &runtime,
        dynamo_config(fake.endpoint_url()).with_max_in_flight(1),
    );
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );

    runtime
        .try_send(caller, CallerMsg::Run(get_request("drain")))
        .expect("send");
    std::thread::sleep(Duration::from_millis(20));

    let first = bridge.closer.close_and_drain(Duration::from_millis(10));
    assert!(first.closed);
    assert!(!first.drained);
    assert_eq!(first.in_flight_remaining, 1);
    assert_eq!(first.in_flight_kinds, vec![("dynamodb_get_item", 1)]);

    match sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Ok(DynamoResponse::GetItem(_))) => {}
        other => panic!("expected accepted request to finish, got {other:?}"),
    }
    let second = bridge.closer.close_and_drain(Duration::from_millis(10));
    assert!(second.drained);
    assert_eq!(second.in_flight_remaining, 0);
    assert!(second.in_flight_kinds.is_empty());

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn timeout_after_admission_counts_late_result_when_sdk_finishes() {
    let fake = FakeDynamoDb::spawn_with_delay(Duration::from_millis(250));
    let runtime = make_runtime();
    let bridge = install_bridge(
        &runtime,
        dynamo_config(fake.endpoint_url())
            .with_default_timeout(Duration::from_millis(40))
            .with_poll_interval(Duration::from_millis(2)),
    );
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );

    runtime
        .try_send(caller, CallerMsg::Run(get_request("timeout")))
        .expect("send");
    match sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Err(DynamoError::Timeout)) => {}
        other => panic!("expected worker timeout, got {other:?}"),
    }
    assert_eq!(
        bridge.metrics.snapshot().in_flight_current,
        1,
        "timed-out SDK work still occupies the bounded slot until it reports terminal truth"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snap = bridge.metrics.snapshot();
        if snap.late_results == 1 && snap.responses == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "late result was not counted: {snap:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn closer_rejects_new_admissions_and_drain_reports_zero_in_flight() {
    let fake = FakeDynamoDb::spawn();
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, dynamo_config(fake.endpoint_url()));
    let address = bridge.address;
    let closer = bridge.closer.clone();

    // Close before any work — drain reports drained=true, 0 remaining.
    let report = closer.close_and_drain(Duration::from_millis(50));
    assert!(report.closed);
    assert!(report.drained);
    assert_eq!(report.in_flight_remaining, 0);
    assert!(report.in_flight_kinds.is_empty());

    // Subsequent admit replies Closed.
    let sink = Arc::new(Sink::default());
    let caller = register_caller(&runtime, address, sink.clone(), Duration::from_secs(1));
    runtime
        .try_send(
            caller,
            CallerMsg::Run(DynamoRequest::GetItem(DynamoGetItem {
                table_name: "kv".into(),
                key: make_key("k"),
                consistency: DynamoConsistency::Eventual,
            })),
        )
        .expect("send get");
    let outcome = sink.wait(Duration::from_secs(2));
    assert!(
        matches!(
            outcome,
            tina_runtime::CallOutcome::Replied(Err(DynamoError::Closed))
        ),
        "expected Closed, got {outcome:?}"
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn supplied_client_path_reports_sdk_max_attempts_zero() {
    // Build a real SDK client outside the bridge.
    let fake = FakeDynamoDb::spawn();
    let endpoint = fake.endpoint_url();
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .expect("tokio rt for supplied client");
    let handle = tokio_rt.handle().clone();

    let sdk_client = handle.block_on(async {
        let cfg = aws_sdk_dynamodb::Config::builder()
            .behavior_version(aws_sdk_dynamodb::config::BehaviorVersion::v2026_01_12())
            .region(aws_sdk_dynamodb::config::Region::new("us-east-1"))
            .endpoint_url(endpoint.clone())
            .credentials_provider(aws_sdk_dynamodb::config::Credentials::new(
                "k", "s", None, None, "test",
            ))
            .build();
        aws_sdk_dynamodb::Client::from_conf(cfg)
    });

    let runtime = make_runtime();
    let cfg = DynamoConfig::default();
    let (worker, metrics) =
        DynamoWorker::<SingleShard>::with_supplied_client(cfg, sdk_client, handle)
            .expect("supplied client");
    let _address = runtime
        .register_with_capacity::<_, Infallible>(worker, 16)
        .expect("register supplied-client worker");

    let snap = metrics.snapshot();
    assert_eq!(
        snap.sdk_max_attempts, 0,
        "supplied-client path leaves SDK retry policy with the caller; metrics report 0 (unknown)"
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
