//! Tina: `tina_http::HttpsListener` + `Counter` isolate.
//!
//! Startup is **call-shaped**: a tiny `Driver` isolate calls
//! `HttpsListener` with `Start` and the typed
//! `Result<HttpsReady, HttpsStartupError>` reply pops out via an
//! mpsc channel. Once `Ready` arrives, the example drives the
//! shared scripted rustls client and shuts down.

use std::convert::Infallible;
use std::sync::mpsc;
use std::time::Duration;

use http::{Method, StatusCode};
use tina::prelude::*;
use tina_http::{
    HttpRequest, HttpResponse, HttpsListener, HttpsListenerMsg, HttpsReady, HttpsServerConfig,
    HttpsStartupError, StatefulRouter, TlsServerIdentity,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, call,
};

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
    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        let router = StatefulRouter::<Counter>::new()
            .get("/counter", get_counter)
            .post("/counter", post_counter)
            .method_not_allowed();
        // Ignore the dispatcher's body for unknown paths so we still
        // produce a 404 with no body — the example contract.
        let response = match (request.method.clone(), request.path.as_str()) {
            (Method::GET | Method::POST, "/counter") => router.dispatch(self, &request),
            _ => HttpResponse::with_status(StatusCode::NOT_FOUND),
        };
        reply(response)
    }
}

#[derive(Debug, Clone)]
enum DriverMsg {
    Start {
        listener: Address<HttpsListenerMsg, Result<HttpsReady, HttpsStartupError>>,
        timeout: Duration,
    },
    Returned(CallOutcome<Result<HttpsReady, HttpsStartupError>>),
}

struct Driver {
    sender: mpsc::Sender<Result<HttpsReady, HttpsStartupError>>,
}

#[tina::isolate(message = DriverMsg, call = RuntimeCall<DriverMsg>)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Start { listener, timeout } => {
                call(listener, HttpsListenerMsg::Start, timeout).reply(DriverMsg::Returned)
            }
            DriverMsg::Returned(CallOutcome::Replied(inner)) => {
                let _ = self.sender.send(inner);
                stop()
            }
            DriverMsg::Returned(_) => {
                let _ = self
                    .sender
                    .send(Err(HttpsStartupError::Bind {
                        source: tina_runtime::CallError::Timeout,
                    }));
                stop()
            }
        }
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

    let (tx, rx) = mpsc::channel();
    let driver = runtime
        .register_with_capacity::<_, Infallible>(Driver { sender: tx }, 8)
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;
    runtime
        .try_send(
            driver,
            DriverMsg::Start {
                listener,
                timeout: Duration::from_secs(5),
            },
        )
        .map_err(|e| anyhow::anyhow!("send Start: {e:?}"))?;

    let ready = rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| anyhow::anyhow!("https startup never replied"))?
        .map_err(|e| anyhow::anyhow!("https startup failed: {e:?}"))?;

    let report = scripted_client(ready.local_addr, identity_bundle.cert_der);

    runtime
        .try_send(listener, HttpsListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("send Stop: {e:?}"))?;
    let _ = runtime.shutdown();

    Ok(report)
}
