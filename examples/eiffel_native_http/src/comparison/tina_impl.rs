use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use http::{Method, StatusCode};
use tina::prelude::*;
use tina_http::{HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

use super::{SideReport, scripted_client};

#[derive(Debug, Default)]
struct Counter {
    value: u32,
}

#[tina::isolate(message = HttpRequest, reply = HttpResponse)]
impl Counter {
    fn handle(&mut self, request: HttpRequest, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
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

    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");

    let listener = runtime
        .register_with_capacity::<HttpListener<SingleShard>, _>(
            HttpListener::<SingleShard>::new(
                bind_addr,
                counter,
                HttpLimits::default(),
                Duration::from_secs(2),
                16,
            ),
            8,
        )
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .expect("send Start");

    let server_addr = bound
        .wait(Duration::from_secs(2))
        .expect("listener publishes bound address");

    let report = scripted_client(server_addr);

    runtime
        .try_send(listener, HttpListenerMsg::Stop)
        .expect("send Stop");
    let _ = runtime.shutdown().expect("runtime shutdown");

    report
}
