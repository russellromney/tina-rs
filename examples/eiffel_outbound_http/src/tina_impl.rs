//! Tina: native `tina_http::HttpListener` server + native
//! `tina_http::HttpClient` outbound client. Each scripted request
//! is bridged from the host thread into the runtime via a tiny
//! `Driver` isolate that does `call(client, ..., timeout)` and
//! forwards the outcome through `std::sync::mpsc`.

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
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, call,
};

use crate::Report;

// -------------------------------------------------------------------
// Counter service.
// -------------------------------------------------------------------

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

// -------------------------------------------------------------------
// Driver: a one-shot isolate that takes one HTTP call, awaits its
// `CallOutcome`, and forwards the result to the host thread via
// std mpsc. The pattern keeps `run_request` simple: each call is a
// fresh isolate that lives for exactly one request.
// -------------------------------------------------------------------

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

    fn handle(&mut self, msg: DriverMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
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
) -> anyhow::Result<Result<HttpResponse, HttpClientError>> {
    let (tx, rx) = mpsc::channel();
    let driver = runtime
        .register_with_capacity::<_, Infallible>(Driver { sender: tx }, 16)
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;
    runtime
        .try_send(
            driver,
            DriverMsg::Begin {
                client,
                target,
                request,
            },
        )
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;
    Ok(rx.recv_timeout(Duration::from_secs(5))?)
}

// -------------------------------------------------------------------
// Run
// -------------------------------------------------------------------

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    // Server: counter service + listener.
    let counter = runtime
        .register_with_capacity::<_, Infallible>(Counter::default(), 16)
        .map_err(|e| anyhow::anyhow!("register counter: {e:?}"))?;
    let server_config = HttpServerConfig::dev();
    let listener_addr = runtime
        .register_with_capacity::<_, _>(
            HttpListener::<SingleShard>::with_config(
                "127.0.0.1:0".parse()?,
                counter,
                server_config,
            ),
            server_config.listener_mailbox_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener_addr, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("send Start: {e:?}"))?;
    let server_addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("listener bind: {e:?}"))?;

    // Client: long-lived service-shaped HttpClient.
    let client = runtime
        .register_with_capacity::<_, Infallible>(
            HttpClient::<SingleShard>::new(HttpClientConfig::dev()),
            16,
        )
        .map_err(|e| anyhow::anyhow!("register client: {e:?}"))?;

    let mut report = Report {
        exit_clean: true,
        ..Report::default()
    };

    // Initial GET — counter=0.
    let r0 = run_request(
        &runtime,
        client,
        server_addr,
        HttpRequest::get("/counter").header("Host", "x").build(),
    )?
    .expect("initial GET");
    if r0.status == StatusCode::OK {
        report.successful_get += 1;
    }
    let body0 = r0.body.as_buffered().unwrap_or(&[]);
    assert_eq!(
        std::str::from_utf8(body0).unwrap_or("").trim(),
        "0",
        "initial GET should report counter=0",
    );

    // Three POSTs.
    for _ in 0..3 {
        let r = run_request(
            &runtime,
            client,
            server_addr,
            HttpRequest::post("/counter").header("Host", "x").build(),
        )?
        .expect("POST");
        if r.status == StatusCode::OK {
            report.successful_post += 1;
        }
    }

    // Final GET — should be 3.
    let r4 = run_request(
        &runtime,
        client,
        server_addr,
        HttpRequest::get("/counter").header("Host", "x").build(),
    )?
    .expect("final GET");
    if r4.status == StatusCode::OK {
        report.successful_get += 1;
    }
    let body4 = r4.body.as_buffered().unwrap_or(&[]);
    report.final_counter_value = std::str::from_utf8(body4)
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0);

    // 404 for missing path.
    let r5 = run_request(
        &runtime,
        client,
        server_addr,
        HttpRequest::get("/missing").header("Host", "x").build(),
    )?
    .expect("404 GET");
    if r5.status == StatusCode::NOT_FOUND {
        report.got_404_for_missing = true;
    }

    runtime
        .try_send(listener_addr, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("send Stop: {e:?}"))?;
    let _ = runtime.shutdown();

    Ok(report)
}
