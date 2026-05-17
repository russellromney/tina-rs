//! End-to-end tests for `tina-aws-bridge`.

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
use tina::{CallContext, RequestContext, reply_to_request};
use tina_aws_bridge::{
    InstalledS3Bridge, S3Address, S3CallOutcome, S3Config, S3Credentials, S3DeleteObject, S3Error,
    S3GetObject, S3HeadObject, S3Object, S3PutObject, S3Request, S3Response, S3Worker,
    SdkRetryPolicy, send_s3,
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
    state: Mutex<Option<S3CallOutcome>>,
    cv: Condvar,
}

impl Sink {
    fn put(&self, outcome: S3CallOutcome) {
        *self.state.lock().expect("sink lock") = Some(outcome);
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) -> S3CallOutcome {
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
    Run(S3Request),
    Done(S3CallOutcome),
}

struct CallerIsolate {
    worker: S3Address,
    timeout: Duration,
    sink: Arc<Sink>,
}

#[derive(Debug)]
enum S3RelayMsg {
    Put {
        bucket: String,
        key: String,
        body: Vec<u8>,
    },
    PutDone(RequestContext<&'static str>, S3CallOutcome),
}

struct S3Relay {
    worker: S3Address,
    timeout: Duration,
}

impl Isolate for S3Relay {
    tina::isolate_types! {
        message: S3RelayMsg,
        reply: &'static str,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<S3RelayMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: S3RelayMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            S3RelayMsg::Put { .. } => noop(),
            S3RelayMsg::PutDone(req, outcome) => match outcome {
                tina_runtime::CallOutcome::Replied(Ok(S3Response::PutObject(_))) => {
                    reply_to_request(req, "stored")
                }
                _ => reply_to_request(req, "failed"),
            },
        }
    }

    fn handle_call(&mut self, msg: S3RelayMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            S3RelayMsg::Put { bucket, key, body } => call
                .defer(send_s3(
                    self.worker,
                    S3Request::PutObject(S3PutObject {
                        bucket,
                        key,
                        body,
                        content_type: None,
                    }),
                    self.timeout,
                ))
                .reply(S3RelayMsg::PutDone),
            S3RelayMsg::PutDone(_, _) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
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
                send_s3(self.worker, request, self.timeout).then(CallerMsg::Done)
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
    config: S3Config,
) -> InstalledS3Bridge<SingleShard> {
    S3Worker::<SingleShard>::install(runtime, config).expect("install bridge")
}

fn register_caller(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    worker: S3Address,
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

fn s3_config(endpoint_url: String) -> S3Config {
    S3Config::default()
        .with_endpoint_url(endpoint_url)
        .with_force_path_style(true)
        .with_credentials(S3Credentials::new("test-access-key", "test-secret-key"))
        .with_poll_interval(Duration::from_millis(2))
}

type ObjectStore = Arc<Mutex<HashMap<(String, String), Vec<u8>>>>;

#[derive(Clone, Default)]
struct S3State {
    objects: ObjectStore,
    delay: Duration,
}

struct FakeS3 {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    runtime: Arc<Runtime>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl FakeS3 {
    fn spawn(delay: Duration) -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1)
                .build()
                .expect("test runtime"),
        );
        let state = S3State {
            delay,
            ..Default::default()
        };
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
                                async move { Ok::<_, Infallible>(handle_s3(req, state).await) }
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
        }
    }

    fn endpoint_url(&self) -> String {
        format!("http://{}", self.addr)
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

impl Drop for FakeS3 {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn handle_s3(req: HyperRequest<Incoming>, state: S3State) -> HyperResponse<Body> {
    if !state.delay.is_zero() {
        tokio::time::sleep(state.delay).await;
    }
    let method = req.method().clone();
    let path = req.uri().path().trim_start_matches('/');
    let mut parts = path.splitn(2, '/');
    let bucket = parts.next().unwrap_or("").to_string();
    let key = parts.next().unwrap_or("").to_string();

    match (method, bucket.is_empty(), key.is_empty()) {
        (Method::HEAD, false, false) => HyperResponse::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/octet-stream")
            .header("etag", "\"fake-etag\"")
            .body(Full::new(Bytes::new()))
            .expect("head response"),
        (Method::PUT, false, false) => {
            let body = req.into_body().collect().await.expect("body").to_bytes();
            state
                .objects
                .lock()
                .expect("objects lock")
                .insert((bucket, key), body.to_vec());
            HyperResponse::builder()
                .status(StatusCode::OK)
                .header("etag", "\"fake-etag\"")
                .body(Full::new(Bytes::new()))
                .expect("put response")
        }
        (Method::GET, false, false) => {
            let maybe = state
                .objects
                .lock()
                .expect("objects lock")
                .get(&(bucket, key))
                .cloned();
            match maybe {
                Some(body) => HyperResponse::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/octet-stream")
                    .header("etag", "\"fake-etag\"")
                    .body(Full::new(Bytes::from(body)))
                    .expect("get response"),
                None => response(StatusCode::NOT_FOUND, Bytes::new()),
            }
        }
        (Method::DELETE, false, false) => {
            state
                .objects
                .lock()
                .expect("objects lock")
                .remove(&(bucket, key));
            response(StatusCode::NO_CONTENT, Bytes::new())
        }
        _ => response(StatusCode::BAD_REQUEST, Bytes::new()),
    }
}

fn response(status: StatusCode, body: Bytes) -> HyperResponse<Body> {
    HyperResponse::builder()
        .status(status)
        .body(Full::new(body))
        .expect("response")
}

#[test]
fn happy_path_put_get_delete_against_local_s3_shape() {
    let s3 = FakeS3::spawn(Duration::ZERO);
    let cfg = s3_config(s3.endpoint_url());
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, cfg);

    let put_sink = Arc::new(Sink::default());
    let put_caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&put_sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            put_caller,
            CallerMsg::Run(S3Request::PutObject(S3PutObject {
                bucket: "bucket".into(),
                key: "key".into(),
                body: b"hello aws".to_vec(),
                content_type: Some("text/plain".into()),
            })),
        )
        .expect("put");
    match put_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Ok(S3Response::PutObject(ok))) => {
            assert_eq!(ok.e_tag.as_deref(), Some("\"fake-etag\""));
        }
        other => panic!("unexpected put outcome: {other:?}"),
    }

    let get_sink = Arc::new(Sink::default());
    let get_caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&get_sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            get_caller,
            CallerMsg::Run(S3Request::GetObject(S3GetObject {
                bucket: "bucket".into(),
                key: "key".into(),
                max_bytes: None,
            })),
        )
        .expect("get");
    match get_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Ok(S3Response::Object(S3Object { body, .. }))) => {
            assert_eq!(body, b"hello aws");
        }
        other => panic!("unexpected get outcome: {other:?}"),
    }

    let head_sink = Arc::new(Sink::default());
    let head_caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&head_sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            head_caller,
            CallerMsg::Run(S3Request::HeadObject(S3HeadObject {
                bucket: "bucket".into(),
                key: "key".into(),
            })),
        )
        .expect("head");
    match head_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Ok(S3Response::HeadObject(head))) => {
            assert_eq!(head.e_tag.as_deref(), Some("\"fake-etag\""));
        }
        other => panic!("unexpected head outcome: {other:?}"),
    }

    let delete_sink = Arc::new(Sink::default());
    let delete_caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&delete_sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            delete_caller,
            CallerMsg::Run(S3Request::DeleteObject(S3DeleteObject {
                bucket: "bucket".into(),
                key: "key".into(),
            })),
        )
        .expect("delete");
    match delete_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Ok(S3Response::DeletedObject(_))) => {}
        other => panic!("unexpected delete outcome: {other:?}"),
    }

    let snap = bridge.metrics.snapshot();
    assert_eq!(snap.responses, 4);
    assert_eq!(snap.sdk_max_attempts, 1, "SDK retries disabled by default");
    assert_eq!(snap.in_flight_current, 0);
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    s3.stop();
}

#[test]
fn handle_call_defer_through_s3_bridge_replies_to_original_caller() {
    let s3 = FakeS3::spawn(Duration::ZERO);
    let cfg = s3_config(s3.endpoint_url());
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, cfg);
    let relay = runtime
        .register_with_capacity::<_, Infallible>(
            S3Relay {
                worker: bridge.address,
                timeout: Duration::from_secs(2),
            },
            8,
        )
        .expect("register relay");

    let outcome = runtime
        .call_blocking(
            relay,
            S3RelayMsg::Put {
                bucket: "bucket".into(),
                key: "via-relay".into(),
                body: b"hello".to_vec(),
            },
            Duration::from_secs(5),
        )
        .expect("call relay");

    assert_eq!(outcome, tina_runtime::CallOutcome::Replied("stored"));
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    s3.stop();
}

#[test]
fn max_in_flight_rejects_full_and_pressure_reports() {
    let s3 = FakeS3::spawn(Duration::from_millis(250));
    let cfg = s3_config(s3.endpoint_url()).with_max_in_flight(1);
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, cfg);
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
        .try_send(
            first,
            CallerMsg::Run(S3Request::HeadObject(S3HeadObject {
                bucket: "bucket".into(),
                key: "key".into(),
            })),
        )
        .expect("first");
    std::thread::sleep(Duration::from_millis(25));
    runtime
        .try_send(
            second,
            CallerMsg::Run(S3Request::HeadObject(S3HeadObject {
                bucket: "bucket".into(),
                key: "key".into(),
            })),
        )
        .expect("second");

    match second_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Err(S3Error::Full)) => {}
        other => panic!("expected worker Full, got {other:?}"),
    }
    match first_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Ok(S3Response::HeadObject(_))) => {}
        other => panic!("expected first success, got {other:?}"),
    }
    let pressure = bridge.metrics.pressure_report();
    assert_eq!(pressure.capacity, 1);
    assert_eq!(pressure.full_count, 1);
    assert_eq!(pressure.high_water, 1);
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    s3.stop();
}

#[test]
fn closer_stops_tina_side_admission() {
    let s3 = FakeS3::spawn(Duration::ZERO);
    let cfg = s3_config(s3.endpoint_url());
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, cfg);
    bridge.closer.close();
    assert!(bridge.closer.is_closed());
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(S3Request::HeadObject(S3HeadObject {
                bucket: "bucket".into(),
                key: "key".into(),
            })),
        )
        .expect("send");
    match sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Err(S3Error::Closed)) => {}
        other => panic!("expected closed, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().closed, 1);
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    s3.stop();
}

#[test]
fn close_and_drain_reports_remaining_then_drained() {
    let s3 = FakeS3::spawn(Duration::from_millis(180));
    let cfg = s3_config(s3.endpoint_url()).with_max_in_flight(1);
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, cfg);
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(S3Request::HeadObject(S3HeadObject {
                bucket: "bucket".into(),
                key: "key".into(),
            })),
        )
        .expect("send");
    std::thread::sleep(Duration::from_millis(20));

    let first = bridge.closer.close_and_drain(Duration::from_millis(10));
    assert!(first.closed);
    assert!(!first.drained);
    assert_eq!(first.in_flight_remaining, 1);
    assert_eq!(first.in_flight_kinds, vec![("s3_head_object", 1)]);

    match sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Ok(S3Response::HeadObject(_))) => {}
        other => panic!("expected accepted request to finish, got {other:?}"),
    }
    let second = bridge.closer.close_and_drain(Duration::from_millis(10));
    assert!(second.drained);
    assert_eq!(second.in_flight_remaining, 0);
    assert!(second.in_flight_kinds.is_empty());

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    s3.stop();
}

#[test]
fn timeout_after_admission_counts_late_result_when_sdk_finishes() {
    let s3 = FakeS3::spawn(Duration::from_millis(250));
    let cfg = s3_config(s3.endpoint_url())
        .with_default_timeout(Duration::from_millis(40))
        .with_poll_interval(Duration::from_millis(2));
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, cfg);
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(S3Request::HeadObject(S3HeadObject {
                bucket: "bucket".into(),
                key: "key".into(),
            })),
        )
        .expect("send");
    let outcome = sink.wait(Duration::from_secs(5));
    match outcome {
        tina_runtime::CallOutcome::Replied(Err(S3Error::Timeout)) => {}
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
    s3.stop();
}

#[test]
fn response_cap_rejects_large_get_without_unbounded_collect() {
    let s3 = FakeS3::spawn(Duration::ZERO);
    let cfg = s3_config(s3.endpoint_url()).with_response_body_limit(4);
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, cfg);
    let put_sink = Arc::new(Sink::default());
    let put = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&put_sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            put,
            CallerMsg::Run(S3Request::PutObject(S3PutObject {
                bucket: "bucket".into(),
                key: "key".into(),
                body: b"too large".to_vec(),
                content_type: None,
            })),
        )
        .expect("put");
    assert!(matches!(
        put_sink.wait(Duration::from_secs(5)),
        tina_runtime::CallOutcome::Replied(Ok(S3Response::PutObject(_)))
    ));

    let get_sink = Arc::new(Sink::default());
    let get = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&get_sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            get,
            CallerMsg::Run(S3Request::GetObject(S3GetObject {
                bucket: "bucket".into(),
                key: "key".into(),
                max_bytes: None,
            })),
        )
        .expect("get");
    match get_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Err(S3Error::ResponseTooLarge)) => {}
        other => panic!("expected ResponseTooLarge, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().response_too_large, 1);
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    s3.stop();
}

#[test]
fn get_object_request_max_bytes_can_tighten_config_cap() {
    let s3 = FakeS3::spawn(Duration::ZERO);
    let cfg = s3_config(s3.endpoint_url()).with_response_body_limit(64);
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, cfg);
    let put_sink = Arc::new(Sink::default());
    let put = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&put_sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            put,
            CallerMsg::Run(S3Request::PutObject(S3PutObject {
                bucket: "bucket".into(),
                key: "key".into(),
                body: b"small but above request cap".to_vec(),
                content_type: None,
            })),
        )
        .expect("put");
    assert!(matches!(
        put_sink.wait(Duration::from_secs(5)),
        tina_runtime::CallOutcome::Replied(Ok(S3Response::PutObject(_)))
    ));

    let get_sink = Arc::new(Sink::default());
    let get = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&get_sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            get,
            CallerMsg::Run(S3Request::GetObject(S3GetObject {
                bucket: "bucket".into(),
                key: "key".into(),
                max_bytes: Some(4),
            })),
        )
        .expect("get");
    match get_sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Err(S3Error::ResponseTooLarge)) => {}
        other => panic!("expected ResponseTooLarge, got {other:?}"),
    }
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    s3.stop();
}

#[test]
fn request_body_cap_rejects_before_sdk_work() {
    let s3 = FakeS3::spawn(Duration::ZERO);
    let cfg = s3_config(s3.endpoint_url()).with_request_body_limit(4);
    let (outcome, metrics) = {
        let runtime = make_runtime();
        let bridge = install_bridge(&runtime, cfg);
        let sink = Arc::new(Sink::default());
        let caller = register_caller(
            &runtime,
            bridge.address,
            Arc::clone(&sink),
            Duration::from_secs(2),
        );
        runtime
            .try_send(
                caller,
                CallerMsg::Run(S3Request::PutObject(S3PutObject {
                    bucket: "bucket".into(),
                    key: "key".into(),
                    body: b"bigger than four".to_vec(),
                    content_type: None,
                })),
            )
            .expect("send");
        let outcome = sink.wait(Duration::from_secs(5));
        let metrics = bridge.metrics;
        if let Ok(rt) = Arc::try_unwrap(runtime) {
            let _ = rt.shutdown();
        }
        (outcome, metrics)
    };
    match outcome {
        tina_runtime::CallOutcome::Replied(Err(S3Error::RequestTooLarge)) => {}
        other => panic!("expected RequestTooLarge, got {other:?}"),
    }
    let snap = metrics.snapshot();
    assert_eq!(snap.request_too_large, 1);
    assert_eq!(snap.admitted, 0, "SDK work must not start");
    s3.stop();
}

#[test]
fn config_validation_rejects_zero_caps() {
    assert_eq!(
        S3Config::new()
            .with_mailbox_capacity(0)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::S3ConfigError::ZeroMailboxCapacity
    );
    assert_eq!(
        S3Config::new()
            .with_max_in_flight(0)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::S3ConfigError::ZeroMaxInFlight
    );
    assert_eq!(
        S3Config::new()
            .with_request_body_limit(0)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::S3ConfigError::ZeroRequestBodyLimit
    );
    assert_eq!(
        S3Config::new()
            .with_response_body_limit(0)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::S3ConfigError::ZeroResponseBodyLimit
    );
    assert_eq!(
        S3Config::new()
            .with_default_timeout(Duration::ZERO)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::S3ConfigError::ZeroDefaultTimeout
    );
    assert_eq!(
        S3Config::new()
            .with_poll_interval(Duration::ZERO)
            .validate()
            .unwrap_err(),
        tina_aws_bridge::S3ConfigError::ZeroPollInterval
    );
}

#[test]
fn sdk_http_error_maps_to_typed_error() {
    let s3 = FakeS3::spawn(Duration::ZERO);
    let cfg = s3_config(s3.endpoint_url());
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, cfg);
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(S3Request::GetObject(S3GetObject {
                bucket: "bucket".into(),
                key: "missing".into(),
                max_bytes: None,
            })),
        )
        .expect("send");
    match sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Err(S3Error::Sdk(detail))) => {
            assert!(!detail.is_empty());
        }
        other => panic!("expected SDK error, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().sdk_errors, 1);
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    s3.stop();
}

#[test]
fn supplied_client_path_keeps_sdk_retry_metrics_caller_owned() {
    use aws_sdk_s3::Client;
    use aws_sdk_s3::config::retry::RetryConfig;
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};

    let s3 = FakeS3::spawn(Duration::ZERO);
    let runtime_for_client = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .expect("user tokio runtime");
    let handle = runtime_for_client.handle().clone();
    let client = Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::v2026_01_12())
            .region(Region::new("us-east-1"))
            .endpoint_url(s3.endpoint_url())
            .force_path_style(true)
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

    let runtime = make_runtime();
    let (worker, metrics) =
        S3Worker::<SingleShard>::with_supplied_client(S3Config::default(), client, handle)
            .expect("with supplied client");
    let cap = worker.mailbox_capacity();
    let worker_addr = runtime
        .register_with_capacity::<_, Infallible>(worker, cap)
        .expect("register worker");
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        worker_addr,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(S3Request::HeadObject(S3HeadObject {
                bucket: "bucket".into(),
                key: "key".into(),
            })),
        )
        .expect("send");
    match sink.wait(Duration::from_secs(5)) {
        tina_runtime::CallOutcome::Replied(Ok(S3Response::HeadObject(_))) => {}
        other => panic!("unexpected supplied-client outcome: {other:?}"),
    }
    let snap = metrics.snapshot();
    assert_eq!(snap.responses, 1);
    assert_eq!(
        snap.sdk_max_attempts, 0,
        "supplied client owns SDK retry config"
    );
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    runtime_for_client.shutdown_background();
    s3.stop();
}

#[test]
fn supplied_client_path_ignores_client_owned_config_fields() {
    use aws_sdk_s3::Client;
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};

    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .expect("user tokio runtime");
    let client = Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::v2026_01_12())
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new("real", "client-owned", None, None, "test"))
            .build(),
    );

    let cfg = S3Config::new()
        .with_region("")
        .with_credentials(S3Credentials::new("", ""))
        .with_retry_policy(SdkRetryPolicy::Standard { max_attempts: 0 })
        .with_max_in_flight(1);
    let result =
        S3Worker::<SingleShard>::with_supplied_client(cfg, client, client_runtime.handle().clone());
    assert!(
        result.is_ok(),
        "supplied-client path must validate only Tina-owned bridge fields"
    );
    client_runtime.shutdown_background();
}
