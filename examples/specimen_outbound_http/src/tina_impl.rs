//! Tina: native `tina_http::HttpListener` server + native
//! `tina_http::build_keepalive_pool` outbound client. The host runs the
//! same scripted sequence as the Tokio side, but every request still
//! travels through Tina's bounded pool and keepalive connection isolates.

use std::convert::Infallible;
use std::time::Duration;

use http::StatusCode;
use tina::pool::{AcquireOutcome, CloseMode, PoolConfig, ReleaseDisposition, ReleaseOutcome};
use tina::prelude::*;
use tina_http::{
    HttpClientConfig, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, HttpServerConfig,
    HttpTarget, KeepaliveConnAddr, KeepaliveConnectionMsg, KeepaliveOutcome,
    KeepalivePoolDrainOutcome, StatefulRouter, build_keepalive_pool, shutdown_keepalive_pool,
};
use tina_runtime::pool::{WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallKind, CallOutcome, DefaultThreadedMailboxFactory, RuntimeEventKind, ThreadedRuntime,
};

use crate::Report;

type Runtime = ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>;
type PoolAddr = Address<WorkerPoolMsg<KeepaliveConnAddr>, WorkerPoolReply<KeepaliveConnAddr>>;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

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

// -------------------------------------------------------------------
// Host-side script.
// -------------------------------------------------------------------

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)?;

    // Server: counter service + keepalive-enabled listener.
    let counter = runtime
        .register_with_capacity::<_, Infallible>(Counter::default(), 16)
        .map_err(|e| anyhow::anyhow!("register counter: {e:?}"))?;

    let mut server_config = HttpServerConfig::dev();
    server_config.limits.keepalive_idle_timeout = Some(Duration::from_secs(30));
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

    // Client: a one-slot keepalive pool. The pool owns the lease
    // vocabulary; the connection isolate owns the TCP transport and
    // reuses it for every request below.
    let handles = build_keepalive_pool(
        &runtime,
        HttpTarget::http_with_host(server_addr, "x"),
        HttpClientConfig::dev(),
        PoolConfig::new(1, 4),
        16,
        16,
    )
    .map_err(|e| anyhow::anyhow!("build keepalive pool: {e:?}"))?;

    let lease = acquire_connection(&runtime, handles.pool)?;
    let conn = *lease.handle();
    let mut report = Report {
        exit_clean: true,
        ..Report::default()
    };

    let response = send_request(&runtime, conn, HttpRequest::get("/counter").build())?;
    record_get(&mut report, &response, Some("0"))?;

    for _ in 0..3 {
        let response = send_request(&runtime, conn, HttpRequest::post("/counter").build())?;
        if response.status == StatusCode::OK {
            report.successful_post += 1;
        }
    }

    let response = send_request(&runtime, conn, HttpRequest::get("/counter").build())?;
    record_get(&mut report, &response, None)?;
    report.final_counter_value = body_text(&response).trim().parse().unwrap_or(0);

    let response = send_request(&runtime, conn, HttpRequest::get("/missing").build())?;
    if response.status == StatusCode::NOT_FOUND {
        report.got_404_for_missing = true;
    }

    release_connection(&runtime, handles.pool, lease)?;
    let shutdown = shutdown_keepalive_pool(&runtime, &handles, CloseMode::Drain, REQUEST_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("shutdown keepalive pool: {e:?}"))?;
    anyhow::ensure!(
        matches!(shutdown.drain, KeepalivePoolDrainOutcome::Drained)
            && shutdown.requested == shutdown.stopped
            && shutdown.timed_out == 0
            && shutdown.rejected == 0
            && shutdown.already_closed == 0
            && shutdown.connection_failures.is_empty(),
        "keepalive shutdown was not clean: {shutdown:?}"
    );

    runtime
        .try_send(listener_addr, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("send Stop: {e:?}"))?;
    let trace = runtime
        .shutdown()
        .map_err(|e| anyhow::anyhow!("runtime shutdown: {e:?}"))?;
    let accepts = trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::TcpAccept,
                    ..
                }
            )
        })
        .count();
    anyhow::ensure!(
        accepts == 1,
        "keepalive specimen expected one server TCP accept across the script, got {accepts}"
    );

    Ok(report)
}

fn acquire_connection(
    runtime: &Runtime,
    pool: PoolAddr,
) -> anyhow::Result<tina::pool::PoolLease<KeepaliveConnAddr>> {
    match runtime.call_blocking(pool, WorkerPoolMsg::Acquire, REQUEST_TIMEOUT)? {
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => Ok(lease),
        other => anyhow::bail!("expected keepalive pool acquire, got {other:?}"),
    }
}

fn send_request(
    runtime: &Runtime,
    conn: KeepaliveConnAddr,
    request: HttpRequest,
) -> anyhow::Result<HttpResponse> {
    match runtime.call_blocking(
        conn,
        KeepaliveConnectionMsg::request(request, REQUEST_TIMEOUT),
        REQUEST_TIMEOUT + Duration::from_secs(1),
    )? {
        CallOutcome::Replied(KeepaliveOutcome::Request {
            result: Ok(response),
            ..
        }) => Ok(response),
        other => anyhow::bail!("expected keepalive response, got {other:?}"),
    }
}

fn release_connection(
    runtime: &Runtime,
    pool: PoolAddr,
    lease: tina::pool::PoolLease<KeepaliveConnAddr>,
) -> anyhow::Result<()> {
    match runtime.call_blocking(
        pool,
        WorkerPoolMsg::Release {
            lease,
            disposition: ReleaseDisposition::Reuse,
        },
        REQUEST_TIMEOUT,
    )? {
        CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Released)) => Ok(()),
        other => anyhow::bail!("expected keepalive pool release, got {other:?}"),
    }
}

fn record_get(report: &mut Report, response: &HttpResponse, expected_body: Option<&str>) -> anyhow::Result<()> {
    if response.status == StatusCode::OK {
        report.successful_get += 1;
    }
    if let Some(expected) = expected_body {
        anyhow::ensure!(
            body_text(response).trim() == expected,
            "GET body mismatch: expected {expected:?}, got {:?}; response={response:?}",
            body_text(response),
        );
    }
    Ok(())
}

fn body_text(response: &HttpResponse) -> String {
    String::from_utf8_lossy(response.body.as_buffered().unwrap_or(&[])).to_string()
}
