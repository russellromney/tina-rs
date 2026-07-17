//! All-Tina HTTPS counter specimen: a `tina_http::HttpsListener` server **and**
//! a `tina_http::HttpClient` HTTPS client share one runtime, on one shard. The
//! client scripts `GET /counter → POST × 3 → GET /counter → GET /missing`
//! against the server. Before the TLS listener/client split this was impossible — the single TLS
//! worker deadlocked both sides of one handshake — so the client lived in a
//! separate stdlib-rustls process. Now TLS rides the TCP rail and both ends are
//! Tina.

use std::convert::Infallible;
use std::time::Duration;

use http::{Method, StatusCode, Version};
use tina::prelude::*;
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientMsg, HttpHostPolicy, HttpRequest, HttpRequestBody,
    HttpResponse, HttpResponseBody, HttpTarget, HttpsListener, HttpsListenerMsg, HttpsServerConfig,
    StatefulRouter, TlsServerIdentity, TlsTrustRoots,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem};

use crate::{Report, tls_identity};

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

fn request(method: Method, path: &str) -> HttpRequest {
    HttpRequest {
        method,
        path: path.to_string(),
        version: Version::HTTP_11,
        headers: http::HeaderMap::new(),
        body: HttpRequestBody::default(),
    }
}

fn body_text(response: &HttpResponse) -> String {
    match &response.body {
        HttpResponseBody::Buffered(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => String::new(),
    }
}

pub fn run() -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), run_application)?)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
) -> anyhow::Result<Report> {
    let identity_bundle = tls_identity::generate();
    let identity = TlsServerIdentity::from_der(
        identity_bundle.cert_chain_der.clone(),
        identity_bundle.private_key_der.clone(),
    );

    let counter = app
        .register_root::<_, Infallible>(Counter::default(), 16)
        .map_err(|e| anyhow::anyhow!("register counter: {e:?}"))?;
    let listener = app
        .register_root::<_, _>(
            HttpsListener::<SingleShard>::new(
                "127.0.0.1:0".parse()?,
                counter,
                HttpsServerConfig::dev(identity),
            ),
            8,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;
    // The HTTPS client shares the runtime — and the shard, and the TLS lane —
    // with the server it talks to.
    let client = app
        .register_root::<HttpClient<SingleShard>, Infallible>(
            HttpClient::<SingleShard>::new(HttpClientConfig::dev()),
            16,
        )
        .map_err(|e| anyhow::anyhow!("register client: {e:?}"))?;

    let ready = match app
        .call_blocking(listener, HttpsListenerMsg::Start, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("https startup call failed: {e:?}"))?
    {
        CallOutcome::Replied(Ok(ready)) => ready,
        CallOutcome::Replied(Err(error)) => anyhow::bail!("https startup failed: {error:?}"),
        CallOutcome::Full => anyhow::bail!("https startup call back-pressured (mailbox full)"),
        CallOutcome::Closed => anyhow::bail!("https listener closed before reply"),
        CallOutcome::Timeout => anyhow::bail!("https startup timed out before listener replied"),
        CallOutcome::Rejected(reason) => anyhow::bail!("https startup rejected: {reason:?}"),
    };

    let trust = TlsTrustRoots::from_der(vec![identity_bundle.cert_der.clone()]);
    let target = || HttpTarget::Https {
        addr: ready.local_addr,
        server_name: "localhost".to_string(),
        trust_roots: trust.clone(),
        host: HttpHostPolicy::Explicit("localhost".to_string()),
    };

    let mut report = Report {
        exit_clean: true,
        ..Report::default()
    };

    // The same scripted flow the stdlib client runs against the tokio side, now
    // driven by Tina's own HTTPS client against Tina's own HTTPS server.
    let fetch = |req: HttpRequest| -> anyhow::Result<HttpResponse> {
        match app
            .call_blocking(
                client,
                HttpClientMsg::call(target(), req),
                Duration::from_secs(5),
            )
            .map_err(|e| anyhow::anyhow!("client call failed: {e:?}"))?
        {
            CallOutcome::Replied(Ok(response)) => Ok(response),
            CallOutcome::Replied(Err(error)) => Err(anyhow::anyhow!("client error: {error:?}")),
            other => Err(anyhow::anyhow!("client call did not reply: {other:?}")),
        }
    };

    let first = fetch(request(Method::GET, "/counter"))?;
    if first.status == StatusCode::OK {
        report.successful_get += 1;
    }
    anyhow::ensure!(
        body_text(&first).trim() == "0",
        "first GET should report counter=0"
    );

    for _ in 0..3 {
        let posted = fetch(request(Method::POST, "/counter"))?;
        if posted.status == StatusCode::OK {
            report.successful_post += 1;
        }
    }

    let second = fetch(request(Method::GET, "/counter"))?;
    if second.status == StatusCode::OK {
        report.successful_get += 1;
    }
    report.final_counter_value = body_text(&second).trim().parse().unwrap_or(0);

    let missing = fetch(request(Method::GET, "/missing"))?;
    if missing.status == StatusCode::NOT_FOUND {
        report.got_404_for_missing = true;
    }

    app.try_send(listener, HttpsListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("send Stop: {e:?}"))?;

    Ok(report)
}
