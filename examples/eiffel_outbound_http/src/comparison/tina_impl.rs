//! Tina side of the outbound-HTTP comparison.
//!
//! Hosts a native [`HttpListener`] with a counter service, then drives
//! it via the service-shaped [`HttpClient`] as the outbound client.
//! Walks the same scripted endpoint sequence the Tokio side runs.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::Duration;

use http::{Method, StatusCode};
use tina::prelude::*;
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientError, HttpClientMsg, HttpListener, HttpListenerMsg,
    HttpRequest, HttpResponse, HttpServerConfig,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

use super::SideReport;

#[derive(Debug, Default)]
struct Counter {
    value: u32,
}

#[tina::isolate(message = HttpRequest, reply = HttpResponse)]
impl Counter {
    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, SingleShard>,
    ) -> Effect<Self> {
        let response = match (request.method.clone(), request.path.as_str()) {
            (Method::GET, "/counter") => HttpResponse::text(self.value.to_string()),
            (Method::POST, "/counter") => {
                self.value += 1;
                HttpResponse::text(self.value.to_string())
            }
            _ => HttpResponse::with_status(StatusCode::NOT_FOUND),
        };
        reply(response)
    }
}

#[derive(Debug, Clone)]
enum DriverMsg {
    Begin {
        client: Address<HttpClientMsg, Result<HttpResponse, HttpClientError>>,
        target: SocketAddr,
        request: HttpRequest,
    },
    Returned(CallOutcome<Result<HttpResponse, HttpClientError>>),
}

struct Driver {
    sender: mpsc::Sender<Result<HttpResponse, HttpClientError>>,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DriverMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin {
                client,
                target,
                request,
            } => call(
                client,
                HttpClientMsg::call(target, request),
                Duration::from_secs(2),
            )
            .reply(DriverMsg::Returned),
            DriverMsg::Returned(outcome) => {
                let result = match outcome {
                    CallOutcome::Replied(inner) => inner,
                    CallOutcome::Full => Err(HttpClientError::Busy),
                    CallOutcome::Closed => Err(HttpClientError::Closed),
                    CallOutcome::Timeout => Err(HttpClientError::Timeout),
                };
                let _ = self.sender.send(result);
                stop()
            }
        }
    }
}

fn run_request(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    client: Address<HttpClientMsg, Result<HttpResponse, HttpClientError>>,
    target: SocketAddr,
    request: HttpRequest,
) -> Result<HttpResponse, HttpClientError> {
    let (tx, rx) = mpsc::channel();
    let driver = runtime
        .register_with_capacity::<Driver, Infallible>(Driver { sender: tx }, 16)
        .expect("register driver");
    runtime
        .try_send(
            driver,
            DriverMsg::Begin {
                client,
                target,
                request,
            },
        )
        .expect("send Begin");
    rx.recv_timeout(Duration::from_secs(5))
        .expect("driver result")
}

pub(crate) fn run() -> SideReport {
    let runtime = ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    // Server side: counter service + listener.
    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
    let server_config = HttpServerConfig::dev();
    let listener_addr = runtime
        .register_with_capacity::<HttpListener<SingleShard>, _>(
            HttpListener::<SingleShard>::with_config(bind_addr, counter, server_config),
            server_config.listener_mailbox_capacity,
        )
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener_addr, HttpListenerMsg::Start)
        .expect("send Start");
    let server_addr = bound
        .wait(Duration::from_secs(2))
        .expect("listener publishes bound address");

    // Client side: long-lived service-shaped HttpClient.
    let client = runtime
        .register_with_capacity::<HttpClient<SingleShard>, Infallible>(
            HttpClient::<SingleShard>::new(HttpClientConfig::dev()),
            16,
        )
        .expect("register client");

    let mut report = SideReport {
        successful_get: 0,
        successful_post: 0,
        final_counter_value: 0,
        got_404_for_missing: false,
        exit_clean: true,
    };

    let r0 = run_request(
        &runtime,
        client,
        server_addr,
        HttpRequest::get("/counter").header("Host", "x").build(),
    )
    .expect("initial GET");
    if r0.status == StatusCode::OK {
        report.successful_get += 1;
    }
    assert_eq!(
        std::str::from_utf8(&r0.body).unwrap_or("").trim(),
        "0",
        "initial GET should report counter=0"
    );

    for _ in 0..3 {
        let r = run_request(
            &runtime,
            client,
            server_addr,
            HttpRequest::post("/counter").header("Host", "x").build(),
        )
        .expect("POST");
        if r.status == StatusCode::OK {
            report.successful_post += 1;
        }
    }

    let r4 = run_request(
        &runtime,
        client,
        server_addr,
        HttpRequest::get("/counter").header("Host", "x").build(),
    )
    .expect("final GET");
    if r4.status == StatusCode::OK {
        report.successful_get += 1;
    }
    report.final_counter_value = std::str::from_utf8(&r4.body)
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0);

    let r5 = run_request(
        &runtime,
        client,
        server_addr,
        HttpRequest::get("/missing").header("Host", "x").build(),
    )
    .expect("404 GET");
    if r5.status == StatusCode::NOT_FOUND {
        report.got_404_for_missing = true;
    }

    runtime
        .try_send(listener_addr, HttpListenerMsg::Stop)
        .expect("send Stop");
    let _ = runtime.shutdown().expect("runtime shutdown");

    report
}
