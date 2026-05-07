//! Tina: native `tina_http::HttpListener` server + native
//! `tina_http::HttpClient` outbound client. The whole scripted
//! sequence (GET → 3× POST → GET → GET /missing) runs inside a
//! single `Driver` isolate that walks the steps via
//! `call(client, ..., timeout).reply(...)` and ends with
//! `stop_with(report)`. The host reads the report through
//! `runtime.observe_result::<Report>` — no `mpsc::channel`, no
//! per-request bridge.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use http::StatusCode;
use tina::prelude::*;
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientError, HttpClientMsg, HttpListener, HttpListenerMsg,
    HttpRequest, HttpResponse, HttpServerConfig, StatefulRouter,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, call};

use crate::Report;

// -------------------------------------------------------------------
// Counter service.
// -------------------------------------------------------------------

#[derive(Debug, Default)]
struct Counter {
    value: u32,
}

fn get_counter(state: &mut Counter, _: &HttpRequest) -> HttpResponse {
    HttpResponse::text(state.value.to_string())
}

fn post_counter(state: &mut Counter, _: &HttpRequest) -> HttpResponse {
    state.value += 1;
    HttpResponse::text(state.value.to_string())
}

#[tina::isolate(message = HttpRequest, reply = HttpResponse)]
impl Counter {
    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        let router = StatefulRouter::<Counter>::new()
            .get("/counter", get_counter)
            .post("/counter", post_counter)
            .method_not_allowed();
        reply(router.dispatch(self, &request))
    }
}

// -------------------------------------------------------------------
// Driver: walks the scripted sequence, accumulates the Report, and
// `stop_with`s it for the host.
// -------------------------------------------------------------------

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
type ResponseResult = Result<HttpResponse, HttpClientError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    InitialGet,
    Post(u8),
    FinalGet,
    MissingGet,
    Done,
}

impl Step {
    fn next(self) -> Step {
        match self {
            Step::InitialGet => Step::Post(0),
            Step::Post(n) if n + 1 < 3 => Step::Post(n + 1),
            Step::Post(_) => Step::FinalGet,
            Step::FinalGet => Step::MissingGet,
            Step::MissingGet | Step::Done => Step::Done,
        }
    }
}

#[derive(Debug)]
enum DriverMsg {
    Begin,
    Returned(CallOutcome<ResponseResult>),
}

struct Driver {
    client: Address<HttpClientMsg, ResponseResult>,
    target: SocketAddr,
    step: Step,
    report: Report,
}

#[tina_runtime::isolate(message = DriverMsg)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin => self.dispatch(),
            DriverMsg::Returned(outcome) => {
                self.absorb(outcome);
                if self.step == Step::Done {
                    stop_with(self.report)
                } else {
                    self.dispatch()
                }
            }
        }
    }
}

impl Driver {
    fn dispatch(&mut self) -> Effect<Self> {
        let request = match self.step {
            Step::InitialGet | Step::FinalGet => {
                HttpRequest::get("/counter").header("Host", "x").build()
            }
            Step::Post(_) => HttpRequest::post("/counter").header("Host", "x").build(),
            Step::MissingGet => HttpRequest::get("/missing").header("Host", "x").build(),
            Step::Done => return stop_with(self.report),
        };
        call(
            self.client,
            HttpClientMsg::call(self.target, request),
            REQUEST_TIMEOUT,
        )
        .reply(DriverMsg::Returned)
    }

    fn absorb(&mut self, outcome: CallOutcome<ResponseResult>) {
        let response = match outcome {
            CallOutcome::Replied(Ok(resp)) => resp,
            _ => {
                // Any failure halts the script. The smoke test will
                // assert the partial Report against expected counts.
                self.step = Step::Done;
                return;
            }
        };
        match self.step {
            Step::InitialGet => {
                if response.status == StatusCode::OK {
                    self.report.successful_get += 1;
                }
                let body = response.body.as_buffered().unwrap_or(&[]);
                assert_eq!(
                    std::str::from_utf8(body).unwrap_or("").trim(),
                    "0",
                    "initial GET should report counter=0",
                );
            }
            Step::Post(_) => {
                if response.status == StatusCode::OK {
                    self.report.successful_post += 1;
                }
            }
            Step::FinalGet => {
                if response.status == StatusCode::OK {
                    self.report.successful_get += 1;
                }
                let body = response.body.as_buffered().unwrap_or(&[]);
                self.report.final_counter_value = std::str::from_utf8(body)
                    .unwrap_or("0")
                    .trim()
                    .parse()
                    .unwrap_or(0);
            }
            Step::MissingGet => {
                if response.status == StatusCode::NOT_FOUND {
                    self.report.got_404_for_missing = true;
                }
            }
            Step::Done => {}
        }
        self.step = self.step.next();
    }
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

    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                client,
                target: server_addr,
                step: Step::InitialGet,
                report: Report {
                    exit_clean: true,
                    ..Report::default()
                },
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = runtime
        .observe_result::<Report, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    runtime
        .try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    let report = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("driver did not finish: {e:?}"))?;

    runtime
        .try_send(listener_addr, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("send Stop: {e:?}"))?;
    let _ = runtime.shutdown();

    Ok(report)
}
