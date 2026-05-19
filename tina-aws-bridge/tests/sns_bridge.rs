//! End-to-end SNS bridge tests against a tiny in-process fake server.
//! SNS uses the AWS Query (form-encoded) protocol, so the fake parses
//! `application/x-www-form-urlencoded` request bodies.

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
use tina::prelude::*;
use tina_aws_bridge::{
    SnsAddress, SnsCallOutcome, SnsConfig, SnsCredentials, SnsDestination, SnsError, SnsPublish,
    SnsRequest, SnsResponse, SnsWorker, send_sns,
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
    state: Mutex<Option<SnsCallOutcome>>,
    cv: Condvar,
}

impl Sink {
    fn put(&self, outcome: SnsCallOutcome) {
        *self.state.lock().expect("sink lock") = Some(outcome);
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) -> SnsCallOutcome {
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
    Run(SnsRequest),
    Done(SnsCallOutcome),
}

struct CallerIsolate {
    worker: SnsAddress,
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
                send_sns(self.worker, request, self.timeout).then(CallerMsg::Done)
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
    config: SnsConfig,
) -> tina_aws_bridge::InstalledSnsBridge<SingleShard> {
    SnsWorker::<SingleShard>::install(runtime, config).expect("install bridge")
}

fn register_caller(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    worker: SnsAddress,
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

fn sns_config(endpoint_url: String) -> SnsConfig {
    SnsConfig::default()
        .with_endpoint_url(endpoint_url)
        .with_credentials(SnsCredentials::new("test-access-key", "test-secret-key"))
        .with_poll_interval(Duration::from_millis(2))
        .with_default_timeout(Duration::from_secs(2))
}

#[derive(Clone, Default)]
struct FakeState {
    inner: Arc<Mutex<FakeStateInner>>,
}

#[derive(Default)]
struct FakeStateInner {
    request_count: u64,
    last_message: Option<String>,
    last_topic: Option<String>,
}

struct FakeSns {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    runtime: Arc<Runtime>,
    join: Option<tokio::task::JoinHandle<()>>,
    state: FakeState,
}

impl FakeSns {
    fn spawn() -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1)
                .build()
                .expect("test runtime"),
        );
        let state = FakeState::default();
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
                                async move { Ok::<_, Infallible>(handle_sns(req, state).await) }
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

    fn last_message(&self) -> Option<String> {
        self.state
            .inner
            .lock()
            .expect("state lock")
            .last_message
            .clone()
    }

    fn last_topic(&self) -> Option<String> {
        self.state
            .inner
            .lock()
            .expect("state lock")
            .last_topic
            .clone()
    }
}

impl Drop for FakeSns {
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

fn parse_form(body: &[u8]) -> HashMap<String, String> {
    let body = std::str::from_utf8(body).unwrap_or_default();
    let mut out = HashMap::new();
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            out.insert(percent_decode(k), percent_decode(v));
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    // Tiny percent-decoder: handles `%XX` and `+`.
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'+' => out.push(' '),
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16).unwrap_or(0);
                let lo = (bytes[i + 2] as char).to_digit(16).unwrap_or(0);
                out.push(((hi << 4) | lo) as u8 as char);
                i += 2;
            }
            _ => out.push(b as char),
        }
        i += 1;
    }
    out
}

async fn handle_sns(req: HyperRequest<Incoming>, state: FakeState) -> HyperResponse<Body> {
    if req.method() != Method::POST {
        return error_response("InvalidParameter", "method not allowed");
    }
    let body = req.into_body().collect().await.expect("body").to_bytes();
    let form = parse_form(&body);
    let action = form.get("Action").cloned().unwrap_or_default();
    {
        let mut guard = state.inner.lock().expect("state lock");
        guard.request_count += 1;
        if let Some(msg) = form.get("Message") {
            guard.last_message = Some(msg.clone());
        }
        if let Some(arn) = form.get("TopicArn") {
            guard.last_topic = Some(arn.clone());
        }
    }
    match action.as_str() {
        "Publish" => xml_response(
            StatusCode::OK,
            "\
<PublishResponse>
  <PublishResult>
    <MessageId>fake-message-id-1</MessageId>
  </PublishResult>
</PublishResponse>",
        ),
        _ => error_response("InvalidParameter", "unknown action"),
    }
}

fn xml_response(status: StatusCode, body: &str) -> HyperResponse<Body> {
    HyperResponse::builder()
        .status(status)
        .header("content-type", "text/xml")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("response")
}

fn error_response(code: &str, msg: &str) -> HyperResponse<Body> {
    let body = format!(
        "<ErrorResponse><Error><Type>Sender</Type><Code>{code}</Code><Message>{msg}</Message></Error></ErrorResponse>"
    );
    HyperResponse::builder()
        .status(StatusCode::BAD_REQUEST)
        .header("content-type", "text/xml")
        .body(Full::new(Bytes::from(body)))
        .expect("response")
}

#[test]
fn happy_path_publish_returns_message_id_and_hits_the_topic() {
    let fake = FakeSns::spawn();
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, sns_config(fake.endpoint_url()));

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        sink.clone(),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SnsRequest::Publish(SnsPublish {
                destination: SnsDestination::TopicArn(
                    "arn:aws:sns:us-east-1:000000000000:test-topic".into(),
                ),
                message: "hello from Tina".into(),
                subject: None,
                message_group_id: None,
                message_deduplication_id: None,
                attributes: HashMap::new(),
            })),
        )
        .expect("send publish");

    let outcome = sink.wait(Duration::from_secs(3));
    match outcome {
        tina_runtime::CallOutcome::Replied(Ok(SnsResponse::Published(p))) => {
            assert_eq!(p.message_id.as_deref(), Some("fake-message-id-1"));
        }
        other => panic!("expected Replied(Ok), got {other:?}"),
    }

    assert_eq!(fake.request_count(), 1);
    assert_eq!(fake.last_message().as_deref(), Some("hello from Tina"));
    assert_eq!(
        fake.last_topic().as_deref(),
        Some("arn:aws:sns:us-east-1:000000000000:test-topic")
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn message_body_cap_rejects_before_sdk() {
    let fake = FakeSns::spawn();
    let runtime = make_runtime();
    let bridge = install_bridge(
        &runtime,
        sns_config(fake.endpoint_url()).with_message_body_limit(8),
    );

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        sink.clone(),
        Duration::from_secs(1),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SnsRequest::Publish(SnsPublish {
                destination: SnsDestination::TopicArn(
                    "arn:aws:sns:us-east-1:000000000000:test-topic".into(),
                ),
                message: "this message is well over eight bytes".into(),
                subject: None,
                message_group_id: None,
                message_deduplication_id: None,
                attributes: HashMap::new(),
            })),
        )
        .expect("send publish");

    let outcome = sink.wait(Duration::from_secs(2));
    assert!(
        matches!(
            outcome,
            tina_runtime::CallOutcome::Replied(Err(SnsError::MessageTooLarge))
        ),
        "expected MessageTooLarge, got {outcome:?}"
    );
    assert_eq!(fake.request_count(), 0);

    let metrics = bridge.metrics.snapshot();
    assert_eq!(metrics.admitted, 0);
    assert!(metrics.message_too_large >= 1);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn closer_rejects_new_admissions_and_drain_truth() {
    let fake = FakeSns::spawn();
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, sns_config(fake.endpoint_url()));
    let closer = bridge.closer.clone();

    let report = closer.close_and_drain(Duration::from_millis(50));
    assert!(report.closed);
    assert!(report.drained);
    assert_eq!(report.in_flight_remaining, 0);

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        sink.clone(),
        Duration::from_secs(1),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SnsRequest::Publish(SnsPublish {
                destination: SnsDestination::TopicArn("arn:aws:sns:us-east-1:0:t".into()),
                message: "after close".into(),
                subject: None,
                message_group_id: None,
                message_deduplication_id: None,
                attributes: HashMap::new(),
            })),
        )
        .expect("send publish");
    let outcome = sink.wait(Duration::from_secs(2));
    assert!(
        matches!(
            outcome,
            tina_runtime::CallOutcome::Replied(Err(SnsError::Closed))
        ),
        "expected Closed, got {outcome:?}"
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn supplied_client_path_reports_sdk_max_attempts_zero() {
    let fake = FakeSns::spawn();
    let endpoint = fake.endpoint_url();
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .expect("tokio rt for supplied client");
    let handle = tokio_rt.handle().clone();

    let sdk_client = handle.block_on(async {
        let cfg = aws_sdk_sns::Config::builder()
            .behavior_version(aws_sdk_sns::config::BehaviorVersion::v2026_01_12())
            .region(aws_sdk_sns::config::Region::new("us-east-1"))
            .endpoint_url(endpoint.clone())
            .credentials_provider(aws_sdk_sns::config::Credentials::new(
                "k", "s", None, None, "test",
            ))
            .build();
        aws_sdk_sns::Client::from_conf(cfg)
    });

    let runtime = make_runtime();
    let cfg = SnsConfig::default();
    let (worker, metrics) = SnsWorker::<SingleShard>::with_supplied_client(cfg, sdk_client, handle)
        .expect("supplied client");
    let _address = runtime
        .register_with_capacity::<_, Infallible>(worker, 16)
        .expect("register supplied-client worker");

    assert_eq!(metrics.snapshot().sdk_max_attempts, 0);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
