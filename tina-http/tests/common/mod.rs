//! Shared test harness for the native HTTP server integration tests.
//!
//! Each integration test file (`tests/server_smoke.rs`,
//! `tests/server_bad_input.rs`, etc.) writes its own `mod common;` and
//! uses [`TestHarness::start()`] to spin up a real `ThreadedRuntime` with
//! one shard, a `Counter` service isolate, and an `HttpListener` bound
//! to loopback. The harness observes the runtime's bound-address fact so
//! test clients can dial it.

#![allow(dead_code)]

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use http::{Method, StatusCode};
use tina::CallContext;
use tina::prelude::*;
use tina_http::{
    BodyMetrics, HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse,
};
use tina_runtime::{
    DefaultThreadedMailboxFactory, RuntimeEvent, RuntimeEventKind, ThreadedRuntime,
    ThreadedRuntimeConfig,
};

/// Single source of truth for the integration-test shard id. Each
/// integration test binary runs in its own process so collisions between
/// test files are not a real concern, but using a const removes the
/// "magic number per file" smell flagged in the hostile review.
pub const TEST_SHARD_ID: u32 = 101;

#[derive(Debug, Default)]
pub struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(TEST_SHARD_ID)
    }
}

#[derive(Debug, Default)]
pub struct Counter {
    pub value: u64,
}

impl Isolate for Counter {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.response_for(request))
    }

    fn handle_call(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.response_for(request))
    }
}

impl Counter {
    fn response_for(&mut self, request: HttpRequest) -> HttpResponse {
        match (request.method.clone(), request.path.as_str()) {
            (Method::GET, "/counter") => HttpResponse::text(self.value.to_string()),
            (Method::POST, "/counter") => {
                self.value += 1;
                HttpResponse::text(self.value.to_string())
            }
            (Method::POST, "/echo") => {
                let body_bytes = match request.body {
                    tina_http::HttpRequestBody::Buffered(b) => b,
                    tina_http::HttpRequestBody::Stream(_) => Vec::new(),
                };
                HttpResponse::with_body(StatusCode::OK, body_bytes)
            }
            _ => HttpResponse::with_status(StatusCode::NOT_FOUND),
        }
    }
}

/// One configurable knob: the service isolate's mailbox capacity. Tests
/// that exercise overload set this to 1; everyone else uses a generous
/// default.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    pub service_mailbox_capacity: usize,
    pub connection_mailbox_capacity: usize,
    pub service_call_timeout: Duration,
    pub limits: HttpLimits,
    pub body_metrics: Option<BodyMetrics>,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            service_mailbox_capacity: 16,
            connection_mailbox_capacity: 16,
            service_call_timeout: Duration::from_secs(2),
            limits: HttpLimits::default(),
            body_metrics: None,
        }
    }
}

/// One running native HTTP server, bound to a loopback ephemeral port,
/// ready to accept real TCP requests. Drop the harness or call
/// [`TestHarness::shutdown`] to stop the runtime cleanly.
pub struct TestHarness {
    pub addr: SocketAddr,
    runtime: Option<ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>>,
    listener: Address<HttpListenerMsg>,
}

impl TestHarness {
    pub fn start() -> Self {
        Self::start_with_config(HarnessConfig::default())
    }

    pub fn start_with_config(config: HarnessConfig) -> Self {
        let runtime = ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig {
                command_capacity: 64,
                idle_wait: Duration::from_millis(1),
                ..Default::default()
            },
        );

        let counter = runtime
            .register_with_capacity::<Counter, Infallible>(
                Counter::default(),
                config.service_mailbox_capacity,
            )
            .expect("register counter");

        let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");

        let mut listener_isolate = HttpListener::<TestShard>::new(
            bind_addr,
            counter,
            config.limits,
            config.service_call_timeout,
            config.connection_mailbox_capacity,
        );
        if let Some(metrics) = config.body_metrics.clone() {
            listener_isolate = listener_isolate.with_metrics(metrics);
        }
        let listener = runtime
            .register_with_capacity::<HttpListener<TestShard>, _>(listener_isolate, 8)
            .expect("register listener");

        let bound = runtime.observe_next_bound();
        runtime
            .try_send(listener, HttpListenerMsg::Start)
            .expect("send Start");

        let addr = bound
            .wait(Duration::from_secs(2))
            .expect("listener publishes bound address");

        Self {
            addr,
            runtime: Some(runtime),
            listener,
        }
    }

    pub fn shutdown(mut self) -> Vec<RuntimeEvent> {
        if let Some(runtime) = self.runtime.take() {
            let _ = runtime.try_send(self.listener, HttpListenerMsg::Stop);
            runtime.shutdown().unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Snapshots the runtime trace without shutting down. Used by tests
    /// that want to inspect events mid-run; tests that only care about
    /// final state should call `shutdown()` and read its return value.
    pub fn snapshot_trace(&self) -> Vec<RuntimeEvent> {
        self.runtime
            .as_ref()
            .map(|rt| rt.complete_trace().unwrap_or_default())
            .unwrap_or_default()
    }

    /// Borrows the underlying runtime so tests can register their own
    /// auxiliary isolates (e.g. an outbound `HttpClient` driver) on the
    /// same runtime that's hosting the server.
    pub fn runtime_handle(&self) -> &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> {
        self.runtime
            .as_ref()
            .expect("runtime present until shutdown")
    }

    /// Returns the listener isolate address for tests that need to
    /// send extra `HttpListenerMsg`s to it (e.g. idempotency
    /// regression tests).
    pub fn listener_address(&self) -> Address<HttpListenerMsg> {
        self.listener
    }
}

/// Counts runtime events whose kind is `CallCompleted` for a TCP-read
/// completion. Tests use this to prove a request actually exercised the
/// multi-read accumulation path rather than landing in one chunk.
pub fn count_tcp_read_completions(trace: &[RuntimeEvent]) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted { call_kind, .. }
                    if matches!(call_kind, tina_runtime::CallKind::TcpRead)
            )
        })
        .count()
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            let _ = runtime.try_send(self.listener, HttpListenerMsg::Stop);
            let _ = runtime.shutdown();
        }
    }
}

pub fn scripted_request(addr: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream.write_all(request).expect("write request");
    stream.flush().expect("flush");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    response
}

pub fn assert_status_and_body(response: &[u8], expected_status: &str, expected_body: &str) {
    let text = std::str::from_utf8(response).expect("utf8 response");
    assert!(
        text.starts_with(&format!("HTTP/1.1 {expected_status}")),
        "expected status {expected_status} in response: {text:?}"
    );
    assert!(
        text.ends_with(expected_body),
        "expected body {expected_body:?} at end of response: {text:?}"
    );
}

pub fn assert_status_starts_with(response: &[u8], expected_status: &str) {
    let text = std::str::from_utf8(response).expect("utf8 response");
    assert!(
        text.starts_with(&format!("HTTP/1.1 {expected_status}")),
        "expected status {expected_status} in response: {text:?}"
    );
}
