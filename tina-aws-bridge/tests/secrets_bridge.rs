//! End-to-end Secrets Manager bridge tests against a tiny in-process
//! fake server. Secrets Manager uses the AWS JSON-RPC 1.1 protocol
//! (POST + `X-Amz-Target` header), the same shape as DynamoDB.

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
    SecretsAddress, SecretsCallOutcome, SecretsConfig, SecretsCredentials, SecretsError,
    SecretsGetSecretValue, SecretsRequest, SecretsResponse, SecretsWorker, send_secrets,
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
    state: Mutex<Option<SecretsCallOutcome>>,
    cv: Condvar,
}

impl Sink {
    fn put(&self, outcome: SecretsCallOutcome) {
        *self.state.lock().expect("sink lock") = Some(outcome);
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) -> SecretsCallOutcome {
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
    Run(SecretsRequest),
    Done(SecretsCallOutcome),
}

struct CallerIsolate {
    worker: SecretsAddress,
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
                send_secrets(self.worker, request, self.timeout).then(CallerMsg::Done)
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
    config: SecretsConfig,
) -> tina_aws_bridge::InstalledSecretsBridge<SingleShard> {
    SecretsWorker::<SingleShard>::install(runtime, config).expect("install bridge")
}

fn register_caller(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    worker: SecretsAddress,
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

fn secrets_config(endpoint_url: String) -> SecretsConfig {
    SecretsConfig::default()
        .with_endpoint_url(endpoint_url)
        .with_credentials(SecretsCredentials::new(
            "test-access-key",
            "test-secret-key",
        ))
        .with_poll_interval(Duration::from_millis(2))
        .with_default_timeout(Duration::from_secs(2))
}

#[derive(Clone)]
struct FakeBehavior {
    secret_string: Option<String>,
    not_found: bool,
    throttling: bool,
}

#[derive(Clone)]
struct FakeState {
    inner: Arc<Mutex<FakeStateInner>>,
    behavior: Arc<Mutex<FakeBehavior>>,
}

#[derive(Default)]
struct FakeStateInner {
    request_count: u64,
}

struct FakeSecrets {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    runtime: Arc<Runtime>,
    join: Option<tokio::task::JoinHandle<()>>,
    state: FakeState,
}

impl FakeSecrets {
    fn spawn(behavior: FakeBehavior) -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1)
                .build()
                .expect("test runtime"),
        );
        let state = FakeState {
            inner: Arc::new(Mutex::new(FakeStateInner::default())),
            behavior: Arc::new(Mutex::new(behavior)),
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
                                async move { Ok::<_, Infallible>(handle_secrets(req, state).await) }
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
}

impl Drop for FakeSecrets {
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

async fn handle_secrets(req: HyperRequest<Incoming>, state: FakeState) -> HyperResponse<Body> {
    if req.method() != Method::POST {
        return error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "InvalidParameter",
            "bad method",
        );
    }
    let target = req
        .headers()
        .get("x-amz-target")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = req.into_body().collect().await.expect("body").to_bytes();
    let payload: Value = serde_json::from_slice(&body).expect("secrets json request");
    {
        state.inner.lock().expect("state lock").request_count += 1;
    }
    if target != "secretsmanager.GetSecretValue" {
        return error_response(
            StatusCode::BAD_REQUEST,
            "InvalidParameter",
            &format!("unknown target: {target}"),
        );
    }
    let secret_id = payload
        .get("SecretId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let behavior = state.behavior.lock().expect("behavior lock").clone();
    if behavior.not_found {
        return error_response(
            StatusCode::BAD_REQUEST,
            "ResourceNotFoundException",
            "no such secret",
        );
    }
    if behavior.throttling {
        return error_response(StatusCode::BAD_REQUEST, "ThrottlingException", "slow down");
    }
    let secret_string = behavior.secret_string.unwrap_or_else(|| "v".repeat(16));
    response(
        StatusCode::OK,
        json!({
            "ARN": format!("arn:aws:secretsmanager:us-east-1:0:secret:{}", secret_id),
            "Name": secret_id,
            "VersionId": "v1",
            "VersionStages": ["AWSCURRENT"],
            "SecretString": secret_string,
            "CreatedDate": 0,
        }),
    )
}

fn response(status: StatusCode, body: Value) -> HyperResponse<Body> {
    HyperResponse::builder()
        .status(status)
        .header("content-type", "application/x-amz-json-1.1")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("response")
}

fn error_response(status: StatusCode, code: &str, msg: &str) -> HyperResponse<Body> {
    HyperResponse::builder()
        .status(status)
        .header("content-type", "application/x-amz-json-1.1")
        .header("x-amzn-ErrorType", code)
        .body(Full::new(Bytes::from(
            json!({ "__type": code, "message": msg }).to_string(),
        )))
        .expect("response")
}

#[test]
fn happy_path_get_secret_value_returns_typed_secret_string() {
    let fake = FakeSecrets::spawn(FakeBehavior {
        secret_string: Some("super-secret".into()),
        not_found: false,
        throttling: false,
    });
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, secrets_config(fake.endpoint_url()));

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
            CallerMsg::Run(SecretsRequest::GetSecretValue(SecretsGetSecretValue {
                secret_id: "prod/db/password".into(),
                version_id: None,
                version_stage: None,
            })),
        )
        .expect("send get");

    let outcome = sink.wait(Duration::from_secs(3));
    match outcome {
        tina_runtime::CallOutcome::Replied(Ok(SecretsResponse::SecretValue(v))) => {
            assert_eq!(v.secret_string.as_deref(), Some("super-secret"));
            assert_eq!(v.name.as_deref(), Some("prod/db/password"));
            assert_eq!(v.version_id.as_deref(), Some("v1"));
            assert_eq!(v.version_stages, vec!["AWSCURRENT".to_string()]);
        }
        other => panic!("expected SecretValue, got {other:?}"),
    }
    assert_eq!(fake.request_count(), 1);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn secret_too_large_fires_after_sdk_when_response_exceeds_cap() {
    // Cap is 8 bytes; the fake returns a 16-byte string.
    let fake = FakeSecrets::spawn(FakeBehavior {
        secret_string: Some("v".repeat(16)),
        not_found: false,
        throttling: false,
    });
    let runtime = make_runtime();
    let bridge = install_bridge(
        &runtime,
        secrets_config(fake.endpoint_url()).with_secret_size_limit(8),
    );

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
            CallerMsg::Run(SecretsRequest::GetSecretValue(SecretsGetSecretValue {
                secret_id: "fat-secret".into(),
                version_id: None,
                version_stage: None,
            })),
        )
        .expect("send get");
    let outcome = sink.wait(Duration::from_secs(3));
    assert!(
        matches!(
            outcome,
            tina_runtime::CallOutcome::Replied(Err(SecretsError::SecretTooLarge))
        ),
        "expected SecretTooLarge, got {outcome:?}"
    );
    let metrics = bridge.metrics.snapshot();
    assert!(metrics.secret_too_large >= 1);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn not_found_classifies_as_typed_notfound() {
    let fake = FakeSecrets::spawn(FakeBehavior {
        secret_string: None,
        not_found: true,
        throttling: false,
    });
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, secrets_config(fake.endpoint_url()));

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
            CallerMsg::Run(SecretsRequest::GetSecretValue(SecretsGetSecretValue {
                secret_id: "ghost".into(),
                version_id: None,
                version_stage: None,
            })),
        )
        .expect("send get");
    let outcome = sink.wait(Duration::from_secs(3));
    assert!(
        matches!(
            outcome,
            tina_runtime::CallOutcome::Replied(Err(SecretsError::NotFound(_)))
        ),
        "expected NotFound, got {outcome:?}"
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn throttling_classifies_via_error_code_not_http_status() {
    // The fake returns HTTP 400 + x-amzn-ErrorType: ThrottlingException
    // — matching real Secrets Manager behavior. The bridge must read
    // the error code, not the HTTP status (which is 400, not 429).
    let fake = FakeSecrets::spawn(FakeBehavior {
        secret_string: None,
        not_found: false,
        throttling: true,
    });
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, secrets_config(fake.endpoint_url()));

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
            CallerMsg::Run(SecretsRequest::GetSecretValue(SecretsGetSecretValue {
                secret_id: "rate-limited".into(),
                version_id: None,
                version_stage: None,
            })),
        )
        .expect("send get");
    let outcome = sink.wait(Duration::from_secs(3));
    assert!(
        matches!(
            outcome,
            tina_runtime::CallOutcome::Replied(Err(SecretsError::Throttled(_)))
        ),
        "expected Throttled, got {outcome:?}"
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn closer_rejects_new_admissions_and_drain_truth() {
    let fake = FakeSecrets::spawn(FakeBehavior {
        secret_string: Some("never-reached".into()),
        not_found: false,
        throttling: false,
    });
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, secrets_config(fake.endpoint_url()));
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
            CallerMsg::Run(SecretsRequest::GetSecretValue(SecretsGetSecretValue {
                secret_id: "after-close".into(),
                version_id: None,
                version_stage: None,
            })),
        )
        .expect("send get");
    let outcome = sink.wait(Duration::from_secs(2));
    assert!(
        matches!(
            outcome,
            tina_runtime::CallOutcome::Replied(Err(SecretsError::Closed))
        ),
        "expected Closed, got {outcome:?}"
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn supplied_client_path_reports_sdk_max_attempts_zero() {
    let fake = FakeSecrets::spawn(FakeBehavior {
        secret_string: Some("ignored".into()),
        not_found: false,
        throttling: false,
    });
    let endpoint = fake.endpoint_url();
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .expect("tokio rt for supplied client");
    let handle = tokio_rt.handle().clone();
    let sdk_client = handle.block_on(async {
        let cfg = aws_sdk_secretsmanager::Config::builder()
            .behavior_version(aws_sdk_secretsmanager::config::BehaviorVersion::v2026_01_12())
            .region(aws_sdk_secretsmanager::config::Region::new("us-east-1"))
            .endpoint_url(endpoint.clone())
            .credentials_provider(aws_sdk_secretsmanager::config::Credentials::new(
                "k", "s", None, None, "test",
            ))
            .build();
        aws_sdk_secretsmanager::Client::from_conf(cfg)
    });

    let runtime = make_runtime();
    let cfg = SecretsConfig::default();
    let (worker, metrics) =
        SecretsWorker::<SingleShard>::with_supplied_client(cfg, sdk_client, handle)
            .expect("supplied client");
    let _address = runtime
        .register_with_capacity::<_, Infallible>(worker, 16)
        .expect("register supplied-client worker");

    assert_eq!(metrics.snapshot().sdk_max_attempts, 0);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
