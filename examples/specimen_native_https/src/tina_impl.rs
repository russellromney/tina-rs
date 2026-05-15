//! Tina HTTPS server. The host gates startup on the typed call-shaped
//! `Start` reply, then drives the scripted client once `Ready` lands.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_http::{
    HttpRequest, HttpResponse, HttpsListener, HttpsListenerMsg, HttpsServerConfig, StatefulRouter,
    TlsServerIdentity,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime};

use crate::{Report, scripted_client, tls_identity};

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
    let identity_bundle = tls_identity::generate();
    let identity = TlsServerIdentity::from_der(
        identity_bundle.cert_chain_der.clone(),
        identity_bundle.private_key_der.clone(),
    );

    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let counter = runtime
        .register_with_capacity::<_, Infallible>(Counter::default(), 16)
        .map_err(|e| anyhow::anyhow!("register counter: {e:?}"))?;
    let listener = runtime
        .register_with_capacity::<_, _>(
            HttpsListener::<SingleShard>::new(
                "127.0.0.1:0".parse()?,
                counter,
                HttpsServerConfig::dev(identity),
            ),
            8,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;

    let outcome = runtime
        .call_blocking(
            listener,
            HttpsListenerMsg::Start,
            Duration::from_secs(5),
        )
        .map_err(|e| anyhow::anyhow!("https startup call failed: {e:?}"))?;
    let ready = match outcome {
        CallOutcome::Replied(Ok(ready)) => ready,
        CallOutcome::Replied(Err(error)) => {
            anyhow::bail!("https startup failed: {error:?}")
        }
        CallOutcome::Full => anyhow::bail!("https startup call back-pressured (mailbox full)"),
        CallOutcome::Closed => anyhow::bail!("https listener closed before reply"),
        CallOutcome::Timeout => anyhow::bail!("https startup timed out before listener replied"),
        CallOutcome::Rejected(reason) => anyhow::bail!("https startup rejected: {reason:?}"),
    };

    let report = scripted_client(ready.local_addr, identity_bundle.cert_der);

    runtime
        .try_send(listener, HttpsListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("send Stop: {e:?}"))?;
    runtime
        .shutdown()
        .map_err(|e| anyhow::anyhow!("runtime shutdown: {e:?}"))?;

    Ok(report)
}
