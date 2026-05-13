//! End-to-end tests for the SQS side of `tina-aws-bridge`.

use std::collections::VecDeque;
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
    InstalledSqsBridge, SqsAddress, SqsCallOutcome, SqsConfig, SqsCredentials, SqsDeleteMessage,
    SqsDeletedMessage, SqsError, SqsMessage, SqsReceiveMessage, SqsReceivedMessages, SqsRequest,
    SqsResponse, SqsSendMessage, SqsSentMessage, SqsWorker, send_sqs,
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
    state: Mutex<Option<SqsCallOutcome>>,
    cv: Condvar,
}

impl Sink {
    fn put(&self, outcome: SqsCallOutcome) {
        *self.state.lock().expect("sink lock") = Some(outcome);
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) -> SqsCallOutcome {
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
    Run(SqsRequest),
    Done(SqsCallOutcome),
}

struct CallerIsolate {
    worker: SqsAddress,
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
                send_sqs(self.worker, request, self.timeout).reply(CallerMsg::Done)
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
    config: SqsConfig,
) -> InstalledSqsBridge<SingleShard> {
    SqsWorker::<SingleShard>::install(runtime, config).expect("install bridge")
}

fn register_caller(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    worker: SqsAddress,
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

fn sqs_config(endpoint_url: String) -> SqsConfig {
    SqsConfig::default()
        .with_endpoint_url(endpoint_url)
        .with_credentials(SqsCredentials::new("test-access-key", "test-secret-key"))
        .with_poll_interval(Duration::from_millis(2))
}

#[derive(Clone)]
struct FakeMessage {
    id: String,
    receipt: String,
    body: String,
}

#[derive(Default)]
struct FakeStateInner {
    messages: VecDeque<FakeMessage>,
    next_id: u64,
    request_count: u64,
    seen_targets: Vec<String>,
    seen_queue_urls: Vec<String>,
    seen_visibility_timeout: Option<i64>,
}

#[derive(Clone, Default)]
struct SqsState {
    inner: Arc<Mutex<FakeStateInner>>,
    delay: Duration,
}

struct FakeSqs {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    runtime: Arc<Runtime>,
    join: Option<tokio::task::JoinHandle<()>>,
    state: SqsState,
}

impl FakeSqs {
    fn spawn(delay: Duration) -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1)
                .build()
                .expect("test runtime"),
        );
        let state = SqsState {
            delay,
            ..Default::default()
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
                                async move { Ok::<_, Infallible>(handle_sqs(req, state).await) }
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

    fn queue_url(&self) -> String {
        format!("http://{}/queue/test", self.addr)
    }

    fn missing_queue_url(&self) -> String {
        format!("http://{}/queue/missing", self.addr)
    }

    fn over_return_queue_url(&self) -> String {
        format!("http://{}/queue/over-return", self.addr)
    }

    fn seen_visibility_timeout(&self) -> Option<i64> {
        self.state
            .inner
            .lock()
            .expect("state lock")
            .seen_visibility_timeout
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

    fn seen_queue_urls(&self) -> Vec<String> {
        self.state
            .inner
            .lock()
            .expect("state lock")
            .seen_queue_urls
            .clone()
    }

    fn push_raw_message(&self, body: impl Into<String>) {
        let mut guard = self.state.inner.lock().expect("state lock");
        guard.next_id += 1;
        let id = format!("msg-{}", guard.next_id);
        let receipt = format!("receipt-{}", guard.next_id);
        guard.messages.push_back(FakeMessage {
            id,
            receipt,
            body: body.into(),
        });
    }

    fn stop(mut self) {
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

impl Drop for FakeSqs {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn handle_sqs(req: HyperRequest<Incoming>, state: SqsState) -> HyperResponse<Body> {
    if !state.delay.is_zero() {
        tokio::time::sleep(state.delay).await;
    }
    if req.method() != Method::POST {
        return response(StatusCode::METHOD_NOT_ALLOWED, json!({}));
    }
    let target = req
        .headers()
        .get("x-amz-target")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = req.into_body().collect().await.expect("body").to_bytes();
    let payload: Value = serde_json::from_slice(&body).expect("sqs json request");
    let queue_url = payload
        .get("QueueUrl")
        .and_then(Value::as_str)
        .unwrap_or_default();
    {
        let mut guard = state.inner.lock().expect("state lock");
        guard.request_count += 1;
        guard.seen_targets.push(target.clone());
        guard.seen_queue_urls.push(queue_url.to_string());
    }
    if queue_url.contains("missing") {
        return error_response("AWS.SimpleQueueService.NonExistentQueue", "missing queue");
    }

    match target.as_str() {
        "AmazonSQS.SendMessage" => {
            let body = payload
                .get("MessageBody")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut guard = state.inner.lock().expect("state lock");
            guard.next_id += 1;
            let id = format!("msg-{}", guard.next_id);
            let receipt = format!("receipt-{}", guard.next_id);
            guard.messages.push_back(FakeMessage {
                id: id.clone(),
                receipt,
                body,
            });
            response(
                StatusCode::OK,
                json!({
                    "MessageId": id,
                    "MD5OfMessageBody": "fake-md5",
                }),
            )
        }
        "AmazonSQS.ReceiveMessage" => {
            let max = payload
                .get("MaxNumberOfMessages")
                .and_then(Value::as_i64)
                .unwrap_or(1)
                .max(0) as usize;
            let visibility = payload
                .get("VisibilityTimeout")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let mut guard = state.inner.lock().expect("state lock");
            guard.seen_visibility_timeout = Some(visibility);
            let returned = if queue_url.contains("over-return") {
                max.saturating_add(1)
            } else {
                max
            };
            let messages: Vec<_> = guard
                .messages
                .iter()
                .take(returned)
                .map(|msg| {
                    json!({
                        "MessageId": msg.id,
                        "ReceiptHandle": msg.receipt,
                        "Body": msg.body,
                    })
                })
                .collect();
            response(StatusCode::OK, json!({ "Messages": messages }))
        }
        "AmazonSQS.DeleteMessage" => {
            let receipt = payload
                .get("ReceiptHandle")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut guard = state.inner.lock().expect("state lock");
            guard.messages.retain(|msg| msg.receipt != receipt);
            response(StatusCode::OK, json!({}))
        }
        _ => response(StatusCode::BAD_REQUEST, json!({ "unknown": target })),
    }
}

fn response(status: StatusCode, body: Value) -> HyperResponse<Body> {
    HyperResponse::builder()
        .status(status)
        .header("content-type", "application/x-amz-json-1.0")
        .header("x-amzn-requestid", "fake-request")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("response")
}

fn error_response(code: &str, message: &str) -> HyperResponse<Body> {
    HyperResponse::builder()
        .status(StatusCode::BAD_REQUEST)
        .header("content-type", "application/x-amz-json-1.0")
        .header("x-amzn-requestid", "fake-request")
        .body(Full::new(Bytes::from(
            json!({ "__type": code, "message": message }).to_string(),
        )))
        .expect("error response")
}

fn run_call(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    address: SqsAddress,
    request: SqsRequest,
) -> SqsCallOutcome {
    let sink = Arc::new(Sink::default());
    let caller = register_caller(runtime, address, Arc::clone(&sink), Duration::from_secs(2));
    runtime
        .try_send(caller, CallerMsg::Run(request))
        .expect("send");
    sink.wait(Duration::from_secs(5))
}

#[test]
fn config_validation_rejects_zero_and_invalid_caps() {
    assert_eq!(
        SqsConfig::new()
            .with_mailbox_capacity(0)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::SqsConfigError::ZeroMailboxCapacity
    );
    assert_eq!(
        SqsConfig::new()
            .with_max_in_flight(0)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::SqsConfigError::ZeroMaxInFlight
    );
    assert_eq!(
        SqsConfig::new()
            .with_message_body_limit(0)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::SqsConfigError::ZeroMessageBodyLimit
    );
    assert_eq!(
        SqsConfig::new()
            .with_max_receive_messages(0)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::SqsConfigError::InvalidMaxReceiveMessages { requested: 0 }
    );
    assert_eq!(
        SqsConfig::new()
            .with_max_receive_messages(11)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::SqsConfigError::InvalidMaxReceiveMessages { requested: 11 }
    );
    assert_eq!(
        SqsConfig::new()
            .with_default_timeout(Duration::ZERO)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::SqsConfigError::ZeroDefaultTimeout
    );
    assert_eq!(
        SqsConfig::new()
            .with_poll_interval(Duration::ZERO)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::SqsConfigError::ZeroPollInterval
    );
}

#[test]
fn happy_path_send_receive_delete_and_empty_receive() {
    let sqs = FakeSqs::spawn(Duration::ZERO);
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, sqs_config(sqs.endpoint_url()));

    match run_call(
        &runtime,
        bridge.address,
        SqsRequest::SendMessage(SqsSendMessage {
            queue_url: sqs.queue_url(),
            body: "hello sqs".into(),
            message_group_id: None,
            message_deduplication_id: None,
        }),
    ) {
        tina_runtime::CallOutcome::Replied(Ok(SqsResponse::SentMessage(SqsSentMessage {
            message_id: Some(id),
            ..
        }))) => assert_eq!(id, "msg-1"),
        other => panic!("unexpected send outcome: {other:?}"),
    }

    let receipt = match run_call(
        &runtime,
        bridge.address,
        SqsRequest::ReceiveMessage(SqsReceiveMessage {
            queue_url: sqs.queue_url(),
            max_messages: 1,
            visibility_timeout_seconds: 7,
            wait_time_seconds: 0,
        }),
    ) {
        tina_runtime::CallOutcome::Replied(Ok(SqsResponse::ReceivedMessages(
            SqsReceivedMessages { messages },
        ))) => {
            assert_eq!(
                messages,
                vec![SqsMessage {
                    message_id: Some("msg-1".into()),
                    receipt_handle: "receipt-1".into(),
                    body: "hello sqs".into(),
                }]
            );
            messages[0].receipt_handle.clone()
        }
        other => panic!("unexpected receive outcome: {other:?}"),
    };
    assert_eq!(sqs.seen_visibility_timeout(), Some(7));

    match run_call(
        &runtime,
        bridge.address,
        SqsRequest::DeleteMessage(SqsDeleteMessage {
            queue_url: sqs.queue_url(),
            receipt_handle: receipt,
        }),
    ) {
        tina_runtime::CallOutcome::Replied(Ok(SqsResponse::DeletedMessage(SqsDeletedMessage))) => {}
        other => panic!("unexpected delete outcome: {other:?}"),
    }

    match run_call(
        &runtime,
        bridge.address,
        SqsRequest::ReceiveMessage(SqsReceiveMessage {
            queue_url: sqs.queue_url(),
            max_messages: 1,
            visibility_timeout_seconds: 1,
            wait_time_seconds: 0,
        }),
    ) {
        tina_runtime::CallOutcome::Replied(Ok(SqsResponse::ReceivedMessages(
            SqsReceivedMessages { messages },
        ))) => assert!(messages.is_empty(), "empty receive is success"),
        other => panic!("unexpected empty receive outcome: {other:?}"),
    }

    let snap = bridge.metrics.snapshot();
    assert_eq!(snap.responses, 4);
    assert_eq!(snap.sdk_max_attempts, 1);
    assert_eq!(
        sqs.seen_targets(),
        vec![
            "AmazonSQS.SendMessage",
            "AmazonSQS.ReceiveMessage",
            "AmazonSQS.DeleteMessage",
            "AmazonSQS.ReceiveMessage",
        ]
    );
    assert_eq!(sqs.seen_queue_urls(), vec![sqs.queue_url(); 4]);
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    sqs.stop();
}

#[test]
fn caps_reject_message_before_sdk_and_receive_body_after_sdk() {
    let sqs = FakeSqs::spawn(Duration::ZERO);
    let runtime = make_runtime();
    let bridge = install_bridge(
        &runtime,
        sqs_config(sqs.endpoint_url()).with_message_body_limit(4),
    );

    match run_call(
        &runtime,
        bridge.address,
        SqsRequest::SendMessage(SqsSendMessage {
            queue_url: sqs.queue_url(),
            body: "too large".into(),
            message_group_id: None,
            message_deduplication_id: None,
        }),
    ) {
        tina_runtime::CallOutcome::Replied(Err(SqsError::MessageTooLarge)) => {}
        other => panic!("expected MessageTooLarge, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().admitted, 0);
    assert_eq!(sqs.request_count(), 0, "send cap must not call SDK");

    match run_call(
        &runtime,
        bridge.address,
        SqsRequest::DeleteMessage(SqsDeleteMessage {
            queue_url: sqs.queue_url(),
            receipt_handle: String::new(),
        }),
    ) {
        tina_runtime::CallOutcome::Replied(Err(SqsError::InvalidRequest(detail))) => {
            assert!(detail.contains("receipt_handle"));
        }
        other => panic!("expected invalid delete receipt handle, got {other:?}"),
    }
    assert_eq!(sqs.request_count(), 0, "invalid delete must not call SDK");

    sqs.push_raw_message("larger than cap");
    match run_call(
        &runtime,
        bridge.address,
        SqsRequest::ReceiveMessage(SqsReceiveMessage {
            queue_url: sqs.queue_url(),
            max_messages: 1,
            visibility_timeout_seconds: 1,
            wait_time_seconds: 0,
        }),
    ) {
        tina_runtime::CallOutcome::Replied(Err(SqsError::ResponseTooLarge)) => {}
        other => panic!("expected ResponseTooLarge, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().response_too_large, 1);

    sqs.push_raw_message("one");
    sqs.push_raw_message("two");
    match run_call(
        &runtime,
        bridge.address,
        SqsRequest::ReceiveMessage(SqsReceiveMessage {
            queue_url: sqs.over_return_queue_url(),
            max_messages: 1,
            visibility_timeout_seconds: 1,
            wait_time_seconds: 0,
        }),
    ) {
        tina_runtime::CallOutcome::Replied(Err(SqsError::ResponseTooLarge)) => {}
        other => panic!("expected ResponseTooLarge for over-return, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().response_too_large, 2);
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    sqs.stop();
}

#[test]
fn full_closed_and_close_drain_truth() {
    let sqs = FakeSqs::spawn(Duration::from_millis(180));
    let runtime = make_runtime();
    let bridge = install_bridge(
        &runtime,
        sqs_config(sqs.endpoint_url()).with_max_in_flight(1),
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
    let receive = || {
        CallerMsg::Run(SqsRequest::ReceiveMessage(SqsReceiveMessage {
            queue_url: sqs.queue_url(),
            max_messages: 1,
            visibility_timeout_seconds: 1,
            wait_time_seconds: 0,
        }))
    };
    runtime.try_send(first, receive()).expect("first");
    std::thread::sleep(Duration::from_millis(20));
    runtime.try_send(second, receive()).expect("second");

    match second_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Err(SqsError::Full)) => {}
        other => panic!("expected worker Full, got {other:?}"),
    }
    let report = bridge.closer.close_and_drain(Duration::from_millis(10));
    assert!(!report.drained);
    assert_eq!(report.in_flight_kinds, vec![("sqs_receive_message", 1)]);
    match first_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Ok(SqsResponse::ReceivedMessages(_))) => {}
        other => panic!("expected first success, got {other:?}"),
    }
    assert!(
        bridge
            .closer
            .close_and_drain(Duration::from_millis(10))
            .drained
    );

    match run_call(&runtime, bridge.address, receive().clone_request()) {
        tina_runtime::CallOutcome::Replied(Err(SqsError::Closed)) => {}
        other => panic!("expected Closed, got {other:?}"),
    }
    assert_eq!(bridge.metrics.pressure_report().full_count, 1);
    assert_eq!(bridge.metrics.pressure_report().closed_count, 1);
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    sqs.stop();
}

#[test]
fn timeout_after_admission_counts_late_result_when_sdk_finishes() {
    let sqs = FakeSqs::spawn(Duration::from_millis(250));
    let runtime = make_runtime();
    let bridge = install_bridge(
        &runtime,
        sqs_config(sqs.endpoint_url())
            .with_default_timeout(Duration::from_millis(40))
            .with_poll_interval(Duration::from_millis(2)),
    );

    match run_call(
        &runtime,
        bridge.address,
        SqsRequest::ReceiveMessage(SqsReceiveMessage {
            queue_url: sqs.queue_url(),
            max_messages: 1,
            visibility_timeout_seconds: 1,
            wait_time_seconds: 0,
        }),
    ) {
        tina_runtime::CallOutcome::Replied(Err(SqsError::Timeout)) => {}
        other => panic!("expected worker timeout, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().in_flight_current, 1);
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
    sqs.stop();
}

#[test]
fn typed_sdk_error_and_supplied_client_retry_ownership() {
    use aws_sdk_sqs::Client;
    use aws_sdk_sqs::config::retry::RetryConfig;
    use aws_sdk_sqs::config::{BehaviorVersion, Credentials, Region};

    let sqs = FakeSqs::spawn(Duration::ZERO);
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, sqs_config(sqs.endpoint_url()));

    let missing_requests = [
        SqsRequest::SendMessage(SqsSendMessage {
            queue_url: sqs.missing_queue_url(),
            body: "missing".into(),
            message_group_id: None,
            message_deduplication_id: None,
        }),
        SqsRequest::ReceiveMessage(SqsReceiveMessage {
            queue_url: sqs.missing_queue_url(),
            max_messages: 1,
            visibility_timeout_seconds: 1,
            wait_time_seconds: 0,
        }),
        SqsRequest::DeleteMessage(SqsDeleteMessage {
            queue_url: sqs.missing_queue_url(),
            receipt_handle: "receipt".into(),
        }),
    ];
    for request in missing_requests {
        match run_call(&runtime, bridge.address, request) {
            tina_runtime::CallOutcome::Replied(Err(SqsError::QueueDoesNotExist(detail))) => {
                assert!(detail.contains("NonExistentQueue"));
            }
            other => panic!("expected QueueDoesNotExist, got {other:?}"),
        }
    }

    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .expect("user tokio runtime");
    let client = Client::from_conf(
        aws_sdk_sqs::Config::builder()
            .behavior_version(BehaviorVersion::v2026_01_12())
            .region(Region::new("us-east-1"))
            .endpoint_url(sqs.endpoint_url())
            .credentials_provider(Credentials::new(
                "test-access-key",
                "test-secret-key",
                None,
                None,
                "test",
            ))
            .retry_config(RetryConfig::standard().with_max_attempts(3))
            .build(),
    );
    let (worker, metrics) = SqsWorker::<SingleShard>::with_supplied_client(
        SqsConfig::default(),
        client,
        client_runtime.handle().clone(),
    )
    .expect("with supplied client");
    let cap = worker.mailbox_capacity();
    let worker_addr = runtime
        .register_with_capacity::<_, Infallible>(worker, cap)
        .expect("register worker");
    match run_call(
        &runtime,
        worker_addr,
        SqsRequest::ReceiveMessage(SqsReceiveMessage {
            queue_url: sqs.queue_url(),
            max_messages: 1,
            visibility_timeout_seconds: 1,
            wait_time_seconds: 0,
        }),
    ) {
        tina_runtime::CallOutcome::Replied(Ok(SqsResponse::ReceivedMessages(_))) => {}
        other => panic!("unexpected supplied-client outcome: {other:?}"),
    }
    assert_eq!(metrics.snapshot().sdk_max_attempts, 0);
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    client_runtime.shutdown_background();
    sqs.stop();
}

trait CloneRequest {
    fn clone_request(&self) -> SqsRequest;
}

impl CloneRequest for CallerMsg {
    fn clone_request(&self) -> SqsRequest {
        match self {
            CallerMsg::Run(request) => request.clone(),
            CallerMsg::Done(_) => panic!("not a request"),
        }
    }
}
