//! Tina: `tina_http::HttpListener` + `Counter` isolate. Bound
//! address comes back via `runtime.observe_next_bound()`. The
//! shared `scripted_client` exercises the wire.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_http::{
    HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, HttpServerConfig, StatefulRouter,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};

use crate::{Report, scripted_client};

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
    fn response_for(&mut self, request: &HttpRequest) -> HttpResponse {
        let router = StatefulRouter::<Counter>::new()
            .get("/counter", get_counter)
            .post("/counter", post_counter)
            .method_not_allowed();
        router.dispatch(self, request)
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.response_for(&request))
    }

    fn handle_call(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.response_for(&request))
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)?;

    let counter = runtime
        .register_with_capacity::<_, Infallible>(Counter::default(), 16)
        .map_err(|e| anyhow::anyhow!("register counter: {e:?}"))?;

    let server_config = HttpServerConfig::dev();
    let listener = runtime
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
        .try_send(listener, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("send Start: {e:?}"))?;
    let server_addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("listener bind: {e:?}"))?;

    let report = scripted_client(server_addr);

    runtime
        .try_send(listener, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("send Stop: {e:?}"))?;
    runtime
        .shutdown()
        .map_err(|e| anyhow::anyhow!("runtime shutdown: {e:?}"))?;

    Ok(report)
}
