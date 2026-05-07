//! Integration test: a service that dispatches via [`Router`] handles
//! real TCP requests through the native server.
//!
//! Demonstrates the intended use pattern: declare routes once with
//! stateless fn handlers, and call `router.dispatch(&request)` from
//! the service's `handle`.

mod common;

use std::convert::Infallible;
use std::time::Duration;

use http::{Method, StatusCode};
use tina::prelude::*;
use tina_http::{HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, Router};
use tina_runtime::{
    DefaultThreadedMailboxFactory, RuntimeEvent, ThreadedRuntime, ThreadedRuntimeConfig,
};

use common::{TestShard, assert_status_and_body, assert_status_starts_with, scripted_request};

fn say_hi(_: &HttpRequest) -> HttpResponse {
    HttpResponse::text("hi")
}

fn echo_body(req: &HttpRequest) -> HttpResponse {
    let bytes = req.body.as_buffered().unwrap_or(&[]).to_vec();
    HttpResponse::with_body(StatusCode::OK, bytes)
}

struct Service {
    router: Router,
}

impl Isolate for Service {
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
        reply(self.router.dispatch(&request))
    }
}

/// Spins up an HttpListener whose service uses a Router under the hood.
fn start_router_harness() -> (
    std::net::SocketAddr,
    ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    Address<HttpListenerMsg>,
) {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let router =
        Router::new()
            .route(Method::GET, "/hi", say_hi)
            .route(Method::POST, "/echo", echo_body);
    let service = runtime
        .register_with_capacity::<Service, Infallible>(Service { router }, 16)
        .expect("register service");

    let bind: std::net::SocketAddr = "127.0.0.1:0".parse().expect("loopback");
    let listener_isolate = HttpListener::<TestShard>::new(
        bind,
        service,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    );
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

    (addr, runtime, listener)
}

fn shutdown(
    runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    listener: Address<HttpListenerMsg>,
) -> Vec<RuntimeEvent> {
    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    runtime.shutdown().unwrap_or_default()
}

#[test]
fn router_serves_matching_routes() {
    let (addr, runtime, listener) = start_router_harness();

    let r_hi = scripted_request(addr, b"GET /hi HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status_and_body(&r_hi, "200", "hi");

    let r_echo = scripted_request(
        addr,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert_status_and_body(&r_echo, "200", "hello");

    shutdown(runtime, listener);
}

#[test]
fn router_returns_404_on_miss() {
    let (addr, runtime, listener) = start_router_harness();

    let r = scripted_request(addr, b"GET /nope HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status_starts_with(&r, "404");

    let r = scripted_request(addr, b"PATCH /hi HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status_starts_with(&r, "404");

    shutdown(runtime, listener);
}
