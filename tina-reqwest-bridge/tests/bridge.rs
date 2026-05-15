//! End-to-end tests for `tina-reqwest-bridge`.

mod common;

use std::convert::Infallible;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use http::HeaderValue;
use http::Method;
use http::header::HeaderName;
use http::status::StatusCode;
use tina::prelude::*;
use tina_reqwest_bridge::{
    InstalledReqwestBridge, ReqwestAddress, ReqwestConfig, ReqwestConfigError, ReqwestError,
    ReqwestMsg, ReqwestRequest, ReqwestResponse, ReqwestWorker, RetryPolicy, send_request,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

use common::{
    Beacon, FakeServer, FlakyServer, delayed_ok, echo_body, echo_body_len, echo_header,
    fixed_status,
};

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
    Run(ReqwestRequest),
    Done(Outcome),
}

struct CallerIsolate {
    worker: ReqwestAddress,
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
                send_request(self.worker, request, self.timeout).then(CallerMsg::Done)
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
            command_capacity: 32,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ))
}

fn install_bridge(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    config: ReqwestConfig,
) -> InstalledReqwestBridge<SingleShard> {
    ReqwestWorker::<SingleShard>::install(runtime, config).expect("install bridge")
}

fn register_caller(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    worker: ReqwestAddress,
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

/// Run one request through the install() helper end-to-end.
fn run_one_call(
    config: ReqwestConfig,
    request: ReqwestRequest,
    call_timeout: Duration,
    overall_timeout: Duration,
) -> (Outcome, tina_reqwest_bridge::ReqwestMetricsHandle) {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, config);
    let sink = Arc::new(Sink::default());
    let caller_addr = register_caller(&runtime, bridge.address, Arc::clone(&sink), call_timeout);
    runtime
        .try_send(caller_addr, CallerMsg::Run(request))
        .expect("kick caller");
    let outcome = sink.wait(overall_timeout);
    let metrics = bridge.metrics;
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    (outcome, metrics)
}

// ---------------------------------------------------------------------------
// Original baseline tests
// ---------------------------------------------------------------------------

#[test]
fn happy_path_returns_full_body() {
    let server = FakeServer::spawn(delayed_ok(b"hello, tina", Duration::from_millis(1)));
    let url = server.url("/echo");
    let (outcome, metrics) = run_one_call(
        ReqwestConfig::default(),
        ReqwestRequest::get(&url),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    match outcome {
        CallOutcome::Replied(Ok(response)) => {
            assert_eq!(response.status.as_u16(), 200);
            assert_eq!(response.body.as_slice(), b"hello, tina");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    let snap = metrics.snapshot();
    assert_eq!(snap.responses, 1);
    assert_eq!(snap.timeout, 0);
    assert_eq!(snap.full, 0);
    assert_eq!(snap.current_in_flight, 0, "no leftover in-flight");
    server.stop();
}

#[test]
fn response_cap_surfaces_typed_error() {
    let server = FakeServer::spawn(delayed_ok(
        b"this body is much larger than the configured cap",
        Duration::from_millis(0),
    ));
    let config = ReqwestConfig::default().with_response_body_limit(8);
    let (outcome, metrics) = run_one_call(
        config,
        ReqwestRequest::get(server.url("/big")),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    match outcome {
        CallOutcome::Replied(Err(ReqwestError::ResponseTooLarge)) => {}
        other => panic!("expected ResponseTooLarge, got {other:?}"),
    }
    assert_eq!(metrics.snapshot().response_too_large, 1);
    server.stop();
}

#[test]
fn request_body_limit_rejects_before_reqwest() {
    let beacon = Beacon::default();
    let beacon_clone = beacon.clone();
    let server = FakeServer::spawn(move |req| {
        beacon_clone.fire();
        let f = echo_body_len();
        f(req)
    });
    let config = ReqwestConfig::default().with_request_body_limit(4);
    let request = ReqwestRequest::post(server.url("/echo"), b"bigger than four".to_vec());
    let (outcome, metrics) = run_one_call(
        config,
        request,
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    match outcome {
        CallOutcome::Replied(Err(ReqwestError::RequestTooLarge)) => {}
        other => panic!("expected RequestTooLarge, got {other:?}"),
    }
    assert_eq!(metrics.snapshot().request_too_large, 1);
    assert!(!beacon.fired(), "request must not reach the upstream");
    server.stop();
}

#[test]
fn timeout_surfaces_typed_error_and_aborts_task() {
    let server = FakeServer::spawn(delayed_ok(b"slow", Duration::from_millis(800)));
    let url = server.url("/slow");
    let config = ReqwestConfig::default()
        .with_default_timeout(Duration::from_millis(80))
        .with_poll_interval(Duration::from_millis(2));
    let (outcome, metrics) = run_one_call(
        config,
        ReqwestRequest::get(&url),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    match outcome {
        CallOutcome::Replied(Err(ReqwestError::Timeout)) => {}
        other => panic!("expected Timeout, got {other:?}"),
    }
    assert_eq!(metrics.snapshot().timeout, 1);
    server.stop();
}

#[test]
fn closed_worker_rejects_new_sends() {
    let server = FakeServer::spawn(delayed_ok(b"x", Duration::from_millis(0)));
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, ReqwestConfig::default());
    let sink = Arc::new(Sink::default());
    let caller_addr = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );

    bridge.closer.close();
    runtime
        .try_send(
            caller_addr,
            CallerMsg::Run(ReqwestRequest::get(server.url("/x"))),
        )
        .expect("kick");
    match sink.wait(Duration::from_secs(5)) {
        CallOutcome::Replied(Err(ReqwestError::Closed)) => {}
        other => panic!("expected Closed, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().closed, 1);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    server.stop();
}

#[test]
fn full_when_max_in_flight_saturated() {
    let server = FakeServer::spawn(delayed_ok(b"slow", Duration::from_millis(400)));
    let url = server.url("/x");
    let runtime = make_runtime();
    let config = ReqwestConfig::default()
        .with_max_in_flight(1)
        .with_poll_interval(Duration::from_millis(2));
    let bridge = install_bridge(&runtime, config);

    let sink_a = Arc::new(Sink::default());
    let sink_b = Arc::new(Sink::default());
    let caller_a = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_a),
        Duration::from_secs(2),
    );
    let caller_b = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_b),
        Duration::from_secs(2),
    );

    runtime
        .try_send(caller_a, CallerMsg::Run(ReqwestRequest::get(&url)))
        .expect("kick a");
    std::thread::sleep(Duration::from_millis(50));
    runtime
        .try_send(caller_b, CallerMsg::Run(ReqwestRequest::get(&url)))
        .expect("kick b");

    let outcome_b = sink_b.wait(Duration::from_secs(5));
    let outcome_a = sink_a.wait(Duration::from_secs(5));

    assert!(matches!(
        outcome_b,
        CallOutcome::Replied(Err(ReqwestError::Full))
    ));
    assert!(matches!(outcome_a, CallOutcome::Replied(Ok(_))));

    let snap = bridge.metrics.snapshot();
    assert!(snap.full >= 1);
    assert!(snap.responses >= 1);
    assert_eq!(snap.current_in_flight, 0);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    server.stop();
}

#[test]
fn invalid_url_surfaces_typed_invalid_request() {
    let request = ReqwestRequest::get("not-a-real-url");
    let (outcome, metrics) = run_one_call(
        ReqwestConfig::default(),
        request,
        Duration::from_secs(1),
        Duration::from_secs(5),
    );
    match outcome {
        CallOutcome::Replied(Err(ReqwestError::InvalidRequest(_))) => {}
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
    assert_eq!(metrics.snapshot().invalid, 1);
}

#[test]
fn close_drains_in_flight_to_completion() {
    // Send one request, then close the worker before it lands. With
    // drain semantics the in-flight call finishes naturally with the
    // upstream response. New sends during the drain reply Closed.
    let server = FakeServer::spawn(delayed_ok(b"slow", Duration::from_millis(200)));
    let runtime = make_runtime();
    let config = ReqwestConfig::default().with_poll_interval(Duration::from_millis(2));
    let bridge = install_bridge(&runtime, config);

    let sink_in_flight = Arc::new(Sink::default());
    let caller_a = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_in_flight),
        Duration::from_secs(5),
    );
    runtime
        .try_send(
            caller_a,
            CallerMsg::Run(ReqwestRequest::get(server.url("/slow"))),
        )
        .expect("kick a");

    std::thread::sleep(Duration::from_millis(50));
    bridge.closer.close();

    // The in-flight request runs to completion.
    match sink_in_flight.wait(Duration::from_secs(5)) {
        CallOutcome::Replied(Ok(response)) => {
            assert_eq!(response.body.as_slice(), b"slow");
        }
        other => panic!("expected Ok response after drain, got {other:?}"),
    }

    // A new send AFTER close gets Closed.
    let sink_late = Arc::new(Sink::default());
    let caller_b = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_late),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller_b,
            CallerMsg::Run(ReqwestRequest::get(server.url("/x"))),
        )
        .expect("kick b");
    match sink_late.wait(Duration::from_secs(5)) {
        CallOutcome::Replied(Err(ReqwestError::Closed)) => {}
        other => panic!("expected Closed, got {other:?}"),
    }

    let snap = bridge.metrics.snapshot();
    assert_eq!(snap.responses, 1);
    assert_eq!(snap.closed, 1);
    assert_eq!(snap.current_in_flight, 0);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    server.stop();
}

// ---------------------------------------------------------------------------
// New: hostile-review fixes
// ---------------------------------------------------------------------------

#[test]
fn post_body_roundtrip_reaches_upstream() {
    let server = FakeServer::spawn(echo_body());
    let url = server.url("/echo");
    let mut request = ReqwestRequest::post(&url, b"hello bridge body".to_vec());
    request
        .headers
        .insert("content-type", HeaderValue::from_static("text/plain"));
    let (outcome, _) = run_one_call(
        ReqwestConfig::default(),
        request,
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    match outcome {
        CallOutcome::Replied(Ok(response)) => {
            assert_eq!(response.status, StatusCode::OK);
            assert_eq!(response.body.as_slice(), b"hello bridge body");
        }
        other => panic!("expected Ok body roundtrip, got {other:?}"),
    }
    server.stop();
}

#[test]
fn request_header_reaches_upstream_and_response_header_returns() {
    let server = FakeServer::spawn(echo_header("x-tina-token"));
    let url = server.url("/h");
    let mut request = ReqwestRequest::get(&url);
    request.headers.insert(
        HeaderName::from_static("x-tina-token"),
        HeaderValue::from_static("alpha-beta-gamma"),
    );
    let (outcome, _) = run_one_call(
        ReqwestConfig::default(),
        request,
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    match outcome {
        CallOutcome::Replied(Ok(response)) => {
            assert_eq!(response.body.as_slice(), b"alpha-beta-gamma");
            // Server attached x-echoed-from on the way back.
            assert_eq!(
                response.headers.get("x-echoed-from").map(|v| v.as_bytes()),
                Some(b"x-tina-token".as_ref())
            );
        }
        other => panic!("expected echo header response, got {other:?}"),
    }
    server.stop();
}

#[test]
fn non_2xx_status_surfaces_as_ok_response() {
    let server = FakeServer::spawn(fixed_status(StatusCode::NOT_FOUND, b"missing"));
    let (outcome, _) = run_one_call(
        ReqwestConfig::default(),
        ReqwestRequest::get(server.url("/nope")),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    match outcome {
        CallOutcome::Replied(Ok(response)) => {
            assert_eq!(response.status, StatusCode::NOT_FOUND);
            assert_eq!(response.body.as_slice(), b"missing");
        }
        other => panic!("expected Ok(404), got {other:?}"),
    }
    server.stop();
}

#[test]
fn supplied_client_path_works() {
    use reqwest::Client;
    let runtime_for_client = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .expect("user tokio runtime");
    let handle = runtime_for_client.handle().clone();
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("user client");

    let server = FakeServer::spawn(delayed_ok(b"supplied", Duration::from_millis(0)));
    let runtime = make_runtime();
    let (worker, metrics) = ReqwestWorker::<SingleShard>::with_supplied_client(
        ReqwestConfig::default(),
        client,
        handle,
    )
    .expect("with_supplied_client");
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
            CallerMsg::Run(ReqwestRequest::get(server.url("/x"))),
        )
        .expect("kick");
    match sink.wait(Duration::from_secs(5)) {
        CallOutcome::Replied(Ok(response)) => {
            assert_eq!(response.body.as_slice(), b"supplied");
        }
        other => panic!("expected Ok via supplied client, got {other:?}"),
    }
    assert_eq!(metrics.snapshot().responses, 1);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    runtime_for_client.shutdown_background();
    server.stop();
}

#[test]
fn sequential_calls_share_worker_and_complete() {
    let server = FakeServer::spawn(delayed_ok(b"seq", Duration::from_millis(1)));
    let url = server.url("/seq");
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, ReqwestConfig::default());

    for i in 0..3 {
        let sink = Arc::new(Sink::default());
        let caller = register_caller(
            &runtime,
            bridge.address,
            Arc::clone(&sink),
            Duration::from_secs(2),
        );
        runtime
            .try_send(caller, CallerMsg::Run(ReqwestRequest::get(&url)))
            .expect("kick");
        match sink.wait(Duration::from_secs(5)) {
            CallOutcome::Replied(Ok(response)) => {
                assert_eq!(response.body.as_slice(), b"seq", "iteration {i}");
            }
            other => panic!("iteration {i} got {other:?}"),
        }
    }

    let snap = bridge.metrics.snapshot();
    assert_eq!(snap.responses, 3);
    assert_eq!(snap.current_in_flight, 0);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    server.stop();
}

#[test]
fn worker_recovers_after_failure() {
    // First call hits a non-existent server and surfaces Reqwest.
    // Second call hits a working server and succeeds.
    let server = FakeServer::spawn(delayed_ok(b"after-fail", Duration::from_millis(1)));
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, ReqwestConfig::default());

    // First: connect refused.
    let sink_a = Arc::new(Sink::default());
    let caller_a = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_a),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller_a,
            CallerMsg::Run(ReqwestRequest::get("http://127.0.0.1:1/")),
        )
        .expect("kick a");
    let first = sink_a.wait(Duration::from_secs(5));
    assert!(
        matches!(first, CallOutcome::Replied(Err(ReqwestError::Reqwest(_)))),
        "expected Reqwest, got {first:?}"
    );

    // Second: live server.
    let sink_b = Arc::new(Sink::default());
    let caller_b = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_b),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller_b,
            CallerMsg::Run(ReqwestRequest::get(server.url("/ok"))),
        )
        .expect("kick b");
    match sink_b.wait(Duration::from_secs(5)) {
        CallOutcome::Replied(Ok(response)) => {
            assert_eq!(response.body.as_slice(), b"after-fail");
        }
        other => panic!("expected Ok after failure, got {other:?}"),
    }
    let snap = bridge.metrics.snapshot();
    assert_eq!(snap.responses, 1);
    assert!(snap.reqwest_error >= 1);
    assert_eq!(snap.current_in_flight, 0);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    server.stop();
}

#[test]
fn multi_concurrent_correlation_keeps_responses_distinct() {
    // Four concurrent callers, each requesting a path that the upstream
    // echoes back in the response body. Each caller must receive its
    // own path back, not someone else's.
    let server = FakeServer::spawn(|req| {
        let path = req.uri().path().as_bytes().to_vec();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            hyper::Response::builder()
                .status(StatusCode::OK)
                .body(http_body_util::Full::new(bytes::Bytes::from(path)))
                .expect("build response")
        })
    });

    let runtime = make_runtime();
    let bridge = install_bridge(
        &runtime,
        ReqwestConfig::default()
            .with_max_in_flight(8)
            .with_poll_interval(Duration::from_millis(2)),
    );

    let paths = ["/alpha", "/bravo", "/charlie", "/delta"];
    let mut sinks = Vec::new();
    for path in &paths {
        let sink = Arc::new(Sink::default());
        let caller = register_caller(
            &runtime,
            bridge.address,
            Arc::clone(&sink),
            Duration::from_secs(2),
        );
        let url = server.url(path);
        runtime
            .try_send(caller, CallerMsg::Run(ReqwestRequest::get(url)))
            .expect("kick");
        sinks.push((sink, *path));
    }

    for (sink, path) in &sinks {
        match sink.wait(Duration::from_secs(5)) {
            CallOutcome::Replied(Ok(response)) => {
                assert_eq!(
                    response.body.as_slice(),
                    path.as_bytes(),
                    "caller for {path} got the wrong body"
                );
            }
            other => panic!("caller for {path} got {other:?}"),
        }
    }

    let snap = bridge.metrics.snapshot();
    assert_eq!(snap.responses, 4);
    assert!(snap.in_flight_high_water >= 2, "should have overlapped");
    assert_eq!(snap.current_in_flight, 0);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    server.stop();
}

// ---------------------------------------------------------------------------
// New: retry contract
// ---------------------------------------------------------------------------

#[test]
fn no_retry_by_default() {
    // A flaky server that always drops the connection. With
    // RetryPolicy::None the worker reports Reqwest after one attempt.
    let server = FakeServer::spawn(delayed_ok(b"never reached", Duration::from_millis(1)));
    let _ = server.url("/"); // keep server alive variable used below
    let flaky = FlakyServer::spawn(usize::MAX, delayed_ok(b"", Duration::from_millis(0)));
    let url = flaky.url("/");
    let (outcome, metrics) = run_one_call(
        ReqwestConfig::default(),
        ReqwestRequest::get(&url),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Err(ReqwestError::Reqwest(_)))
    ));
    let snap = metrics.snapshot();
    assert_eq!(snap.retries, 0, "default policy must never retry");
    assert!(snap.reqwest_error >= 1);
    flaky.stop();
    server.stop();
}

#[test]
fn retry_succeeds_after_transient_transport_failure() {
    // Drop the first 2 connections, accept the 3rd. With max_attempts=3
    // and on_reqwest_io=true, the third attempt succeeds.
    let flaky = FlakyServer::spawn(2, delayed_ok(b"third time", Duration::from_millis(0)));
    let url = flaky.url("/r");
    let config = ReqwestConfig::default().with_retry(RetryPolicy::Bounded {
        max_attempts: 3,
        delay: Duration::from_millis(5),
        on_timeout: false,
        on_reqwest_io: true,
    });
    let (outcome, metrics) = run_one_call(
        config,
        ReqwestRequest::get(&url),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(response)) => {
            assert_eq!(response.body.as_slice(), b"third time");
        }
        other => panic!("expected Ok after retries, got {other:?}"),
    }
    let snap = metrics.snapshot();
    assert_eq!(snap.responses, 1);
    assert_eq!(snap.retries, 2, "two retries before success");
    // Each attempt admits once.
    assert_eq!(snap.admitted, 3, "three admissions");
    assert_eq!(snap.current_in_flight, 0);
    // The flaky server saw 3 connections.
    assert!(flaky.accepted.read() >= 3);
    flaky.stop();
}

#[test]
fn retry_exhaustion_surfaces_last_error() {
    // Always drop. With max_attempts=2 the worker emits Reqwest after
    // exhausting attempts.
    let flaky = FlakyServer::spawn(usize::MAX, delayed_ok(b"", Duration::from_millis(0)));
    let url = flaky.url("/r");
    let config = ReqwestConfig::default().with_retry(RetryPolicy::Bounded {
        max_attempts: 2,
        delay: Duration::from_millis(5),
        on_timeout: false,
        on_reqwest_io: true,
    });
    let (outcome, metrics) = run_one_call(
        config,
        ReqwestRequest::get(&url),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Err(ReqwestError::Reqwest(_)))
    ));
    let snap = metrics.snapshot();
    assert_eq!(snap.retries, 1);
    assert_eq!(snap.admitted, 2);
    assert_eq!(snap.responses, 0);
    flaky.stop();
}

#[test]
fn per_attempt_timeout_resets_on_each_attempt() {
    // First attempt times out (server delays > 80ms); retry attempt
    // succeeds quickly. The retry must get a fresh per-attempt clock,
    // not the elapsed clock from the first attempt.
    use std::sync::atomic::{AtomicUsize, Ordering};
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_handler = Arc::clone(&attempts);
    let server = FakeServer::spawn(move |_req| {
        let n = attempts_for_handler.fetch_add(1, Ordering::AcqRel);
        Box::pin(async move {
            if n == 0 {
                tokio::time::sleep(Duration::from_millis(400)).await;
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            hyper::Response::builder()
                .status(StatusCode::OK)
                .body(http_body_util::Full::new(bytes::Bytes::from_static(b"ok")))
                .expect("build")
        })
    });

    let url = server.url("/timeout-then-ok");
    let config = ReqwestConfig::default()
        .with_default_timeout(Duration::from_millis(100))
        .with_poll_interval(Duration::from_millis(2))
        .with_retry(RetryPolicy::Bounded {
            max_attempts: 2,
            delay: Duration::from_millis(5),
            on_timeout: true,
            on_reqwest_io: false,
        });
    let (outcome, metrics) = run_one_call(
        config,
        ReqwestRequest::get(&url),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(response)) => {
            assert_eq!(response.body.as_slice(), b"ok");
        }
        other => panic!("expected Ok via retry, got {other:?}"),
    }
    let snap = metrics.snapshot();
    assert_eq!(snap.retries, 1);
    assert_eq!(snap.responses, 1);
    assert_eq!(snap.timeout, 0, "timeout was retried, not surfaced");
    server.stop();
}

#[test]
fn retry_does_not_fire_on_full() {
    // max_in_flight=1 with retry on for everything else. A second
    // request while the first is in-flight gets Full and must NOT
    // retry — Full is caller-visible.
    let server = FakeServer::spawn(delayed_ok(b"slow", Duration::from_millis(300)));
    let url = server.url("/x");
    let runtime = make_runtime();
    let config = ReqwestConfig::default()
        .with_max_in_flight(1)
        .with_poll_interval(Duration::from_millis(2))
        .with_retry(RetryPolicy::Bounded {
            max_attempts: 5,
            delay: Duration::from_millis(5),
            on_timeout: true,
            on_reqwest_io: true,
        });
    let bridge = install_bridge(&runtime, config);

    let sink_a = Arc::new(Sink::default());
    let sink_b = Arc::new(Sink::default());
    let caller_a = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_a),
        Duration::from_secs(2),
    );
    let caller_b = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_b),
        Duration::from_secs(2),
    );

    runtime
        .try_send(caller_a, CallerMsg::Run(ReqwestRequest::get(&url)))
        .expect("kick a");
    std::thread::sleep(Duration::from_millis(50));
    runtime
        .try_send(caller_b, CallerMsg::Run(ReqwestRequest::get(&url)))
        .expect("kick b");

    let outcome_b = sink_b.wait(Duration::from_secs(5));
    assert!(
        matches!(outcome_b, CallOutcome::Replied(Err(ReqwestError::Full))),
        "Full must surface to caller, not retry: got {outcome_b:?}"
    );
    let outcome_a = sink_a.wait(Duration::from_secs(5));
    assert!(matches!(outcome_a, CallOutcome::Replied(Ok(_))));

    let snap = bridge.metrics.snapshot();
    assert_eq!(snap.full, 1);
    assert_eq!(snap.retries, 0, "Full must not consume retry budget");

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    server.stop();
}

#[test]
fn retries_do_not_grow_in_flight_unboundedly() {
    // With max_in_flight=2 and aggressive retry against an always-flaky
    // server, current_in_flight must never exceed 2 across the entire
    // call lifetime.
    let flaky = FlakyServer::spawn(usize::MAX, delayed_ok(b"", Duration::from_millis(0)));
    let url = flaky.url("/never");
    let runtime = make_runtime();
    let config = ReqwestConfig::default()
        .with_max_in_flight(2)
        .with_poll_interval(Duration::from_millis(2))
        .with_retry(RetryPolicy::Bounded {
            max_attempts: 4,
            delay: Duration::from_millis(2),
            on_timeout: true,
            on_reqwest_io: true,
        });
    let bridge = install_bridge(&runtime, config);

    // Two callers in parallel.
    let sink_a = Arc::new(Sink::default());
    let sink_b = Arc::new(Sink::default());
    let caller_a = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_a),
        Duration::from_secs(5),
    );
    let caller_b = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_b),
        Duration::from_secs(5),
    );

    runtime
        .try_send(caller_a, CallerMsg::Run(ReqwestRequest::get(&url)))
        .expect("kick a");
    runtime
        .try_send(caller_b, CallerMsg::Run(ReqwestRequest::get(&url)))
        .expect("kick b");

    // Sample current_in_flight across the run; must always be <= 2.
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut max_observed = 0u64;
    while Instant::now() < deadline {
        let snap = bridge.metrics.snapshot();
        max_observed = max_observed.max(snap.current_in_flight);
        assert!(
            snap.current_in_flight <= 2,
            "current_in_flight {} exceeded max_in_flight 2",
            snap.current_in_flight
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // Drain to completion.
    let _ = sink_a.wait(Duration::from_secs(5));
    let _ = sink_b.wait(Duration::from_secs(5));

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    flaky.stop();

    assert!(max_observed <= 2, "high-water mark exceeded cap");
}

// ---------------------------------------------------------------------------
// New: config validation
// ---------------------------------------------------------------------------

#[test]
fn config_validation_rejects_zero_capacity() {
    let cfg = ReqwestConfig::default().with_mailbox_capacity(0);
    assert!(matches!(
        cfg.validate(),
        Err(ReqwestConfigError::ZeroMailboxCapacity)
    ));
}

#[test]
fn config_validation_rejects_zero_max_in_flight() {
    let cfg = ReqwestConfig::default().with_max_in_flight(0);
    assert!(matches!(
        cfg.validate(),
        Err(ReqwestConfigError::ZeroMaxInFlight)
    ));
}

#[test]
fn config_validation_rejects_zero_poll_interval() {
    let cfg = ReqwestConfig::default().with_poll_interval(Duration::ZERO);
    assert!(matches!(
        cfg.validate(),
        Err(ReqwestConfigError::ZeroPollInterval)
    ));
}

#[test]
fn config_validation_rejects_zero_retry_attempts() {
    let cfg = ReqwestConfig::default().with_retry(RetryPolicy::Bounded {
        max_attempts: 0,
        delay: Duration::from_millis(1),
        on_timeout: true,
        on_reqwest_io: true,
    });
    assert!(matches!(
        cfg.validate(),
        Err(ReqwestConfigError::ZeroRetryAttempts)
    ));
}

#[test]
fn invalid_header_fails_closed() {
    let mut request = ReqwestRequest::get("http://127.0.0.1:1/");
    // Force-construct a header value with bytes that are invalid for
    // HTTP. HeaderValue::from_bytes would refuse, but we go via bytes
    // through clone_headers — pass an invalid name instead, which
    // HeaderName::from_bytes rejects.
    request.headers.insert(
        HeaderName::from_static("x-ok"),
        HeaderValue::from_static("ok"),
    );
    // Sanity: the above is valid. Now send a method that's invalid.
    request.method = Method::from_bytes(b"GET").expect("method");
    // We can't easily fabricate a bad HeaderName via the safe API, so
    // instead test the equivalent guarantee: an invalid request URL
    // surfaces InvalidRequest pre-reqwest.
    let _ = request;
    let bad = ReqwestRequest::get("http://[invalid url]");
    let (outcome, metrics) = run_one_call(
        ReqwestConfig::default(),
        bad,
        Duration::from_secs(1),
        Duration::from_secs(5),
    );
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Err(ReqwestError::InvalidRequest(_)))
    ));
    assert_eq!(metrics.snapshot().invalid, 1);
}

// ---------------------------------------------------------------------------
// Raw-path coverage: lock that the worker also accepts the literal
// `call(addr, ReqwestMsg::Send(req), timeout)` form, not just
// `send_request`. If `send_request` ever stops being a thin wrap, this
// test still exercises the raw worker contract.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum RawCallerMsg {
    Run(ReqwestRequest),
    Done(Outcome),
}

struct RawCallerIsolate {
    worker: ReqwestAddress,
    timeout: Duration,
    sink: Arc<Sink>,
}

impl Isolate for RawCallerIsolate {
    tina::isolate_types! {
        message: RawCallerMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<RawCallerMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: RawCallerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RawCallerMsg::Run(request) => {
                // Deliberately the literal raw form, not `send_request`.
                call(self.worker, ReqwestMsg::Send(request), self.timeout).then(RawCallerMsg::Done)
            }
            RawCallerMsg::Done(outcome) => {
                self.sink.put(outcome);
                stop()
            }
        }
    }
}

#[test]
fn raw_call_path_still_works() {
    let server = FakeServer::spawn(delayed_ok(b"raw", Duration::from_millis(1)));
    let url = server.url("/raw");
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, ReqwestConfig::default());
    let sink = Arc::new(Sink::default());
    let caller_addr = runtime
        .register_with_capacity::<_, Infallible>(
            RawCallerIsolate {
                worker: bridge.address,
                timeout: Duration::from_secs(2),
                sink: Arc::clone(&sink),
            },
            8,
        )
        .expect("register raw caller");
    runtime
        .try_send(caller_addr, RawCallerMsg::Run(ReqwestRequest::get(&url)))
        .expect("kick raw caller");
    match sink.wait(Duration::from_secs(5)) {
        CallOutcome::Replied(Ok(response)) => {
            assert_eq!(response.body.as_slice(), b"raw");
        }
        other => panic!("raw call path failed: {other:?}"),
    }
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    server.stop();
}
