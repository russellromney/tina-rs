//! Tina: native `tina_http::HttpListener` server + an owned
//! `tina_http::InstallKeepalivePool` outbound client installed on the
//! `LocalSystem`. The host runs the
//! same scripted sequence as the Tokio side, but every request still
//! travels through Tina's bounded pool and keepalive connection isolates.

use std::convert::Infallible;
use std::time::Duration;

use http::StatusCode;
use tina::pool::{AcquireOutcome, PoolConfig, ReleaseDisposition, ReleaseOutcome};
use tina::prelude::*;
use tina_http::{
    HttpClientConfig, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, HttpServerConfig,
    HttpTarget, InstallKeepalivePool, KeepaliveCloseAndDrain, KeepaliveConnAddr,
    KeepaliveConnectionMsg, KeepaliveOutcome, KeepalivePoolInstallConfig, StatefulRouter,
};
use tina_runtime::pool::{WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallKind, CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, RuntimeEventKind,
};

use crate::Report;

type App = LocalSystem<SingleShard, DefaultThreadedMailboxFactory>;
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
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), run_application)?)
}

fn run_application(app: &App) -> anyhow::Result<Report> {
    // Server: counter service + keepalive-enabled listener.
    let counter = app
        .register_root::<_, Infallible>(Counter::default(), 16)
        .map_err(|e| anyhow::anyhow!("register counter: {e:?}"))?;

    let mut server_config = HttpServerConfig::dev();
    server_config.limits.keepalive_idle_timeout = Some(Duration::from_secs(30));
    let listener_addr = app
        .register_root::<_, _>(
            HttpListener::<SingleShard>::with_config(
                "127.0.0.1:0".parse()?,
                counter,
                server_config,
            ),
            server_config.listener_mailbox_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;
    let bound = app.observe_next_bound()?;
    app.try_send(listener_addr, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("send Start: {e:?}"))?;
    let server_addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("listener bind: {e:?}"))?;

    // Client: a one-slot keepalive pool installed as an owned resource.
    // The pool owns the lease vocabulary; the connection isolate owns the
    // TCP transport and reuses it for every request below.
    let pool = app
        .install_keepalive_pool(KeepalivePoolInstallConfig::new(
            HttpTarget::http_with_host(server_addr, "x"),
            HttpClientConfig::dev(),
            PoolConfig::new(1, 4),
            16,
            16,
        ))
        .map_err(|e| anyhow::anyhow!("install keepalive pool: {e:?}"))?;

    let lease = acquire_connection(app, pool.pool())?;
    let conn = *lease.handle();
    let mut report = Report {
        exit_clean: true,
        ..Report::default()
    };

    let response = send_request(app, conn, HttpRequest::get("/counter").build())?;
    record_get(&mut report, &response, Some("0"))?;

    for _ in 0..3 {
        let response = send_request(app, conn, HttpRequest::post("/counter").build())?;
        if response.status == StatusCode::OK {
            report.successful_post += 1;
        }
    }

    let response = send_request(app, conn, HttpRequest::get("/counter").build())?;
    // The script posted three increments; a body other than "3" fails the
    // runner instead of fabricating a final value.
    record_get(&mut report, &response, Some("3"))?;
    report.final_counter_value = body_text(&response).trim().parse()?;

    let response = send_request(app, conn, HttpRequest::get("/missing").build())?;
    if response.status == StatusCode::NOT_FOUND {
        report.got_404_for_missing = true;
    }

    release_connection(app, pool.pool(), lease)?;
    let settled = match pool.close_and_drain(REQUEST_TIMEOUT) {
        KeepaliveCloseAndDrain::Drained(settled) => settled,
        other => anyhow::bail!("keepalive close_and_drain was not clean: {other:?}"),
    };
    anyhow::ensure!(
        settled.requested == settled.stopped && settled.already_closed == 0,
        "keepalive shutdown was not clean: {settled:?}"
    );

    app.try_send(listener_addr, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("send Stop: {e:?}"))?;
    let accepts = app
        .trace()
        .events()
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
    app: &App,
    pool: PoolAddr,
) -> anyhow::Result<tina::pool::PoolLease<KeepaliveConnAddr>> {
    match app.call_blocking(pool, WorkerPoolMsg::Acquire, REQUEST_TIMEOUT)? {
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => {
            Ok(lease)
        }
        other => anyhow::bail!("expected keepalive pool acquire, got {other:?}"),
    }
}

fn send_request(
    app: &App,
    conn: KeepaliveConnAddr,
    request: HttpRequest,
) -> anyhow::Result<HttpResponse> {
    match app.call_blocking(
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
    app: &App,
    pool: PoolAddr,
    lease: tina::pool::PoolLease<KeepaliveConnAddr>,
) -> anyhow::Result<()> {
    match app.call_blocking(
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

fn record_get(
    report: &mut Report,
    response: &HttpResponse,
    expected_body: Option<&str>,
) -> anyhow::Result<()> {
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
