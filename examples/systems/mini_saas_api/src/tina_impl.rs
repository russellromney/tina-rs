use std::collections::HashMap;
use std::convert::Infallible;
use std::path::Path;
use std::time::{Duration, Instant};

use http::StatusCode;
use rusqlite::Connection;
use tina::capacity::{CapacityMode, CapacitySurfaceReport};
use tina::pool::{
    AcquireOutcome, CloseMode, PoolConfig, PoolLease, PoolPressureReport, ReleaseDisposition,
    ReleaseOutcome,
};
use tina::prelude::*;
use tina::{CallContext, RequestContext, reply_to_request};
use tina_http::{
    HttpClientConfig, HttpListener, HttpListenerMsg, HttpRequest, HttpRequestBody, HttpResponse,
    HttpServerConfig, HttpTarget, KeepaliveConnAddr, KeepaliveConnectionMsg, KeepaliveOutcome,
    KeepalivePoolDrainOutcome, build_keepalive_pool, shutdown_keepalive_pool,
};
use tina_runtime::pool::{WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, RuntimeEvent, RuntimeEventKind,
    ThreadedRuntime, call, sleep,
};
use tina_sim::dst::{
    LiveReplayCapture, LiveReplayFact, LiveReplayReport, ReplayCase as DstReplayCase, ReplayConfig,
    ReplayReport, check_captured_replay,
};
use tina_sqlite_bridge::{
    SqliteAddress, SqliteConfig, SqliteError, SqliteMetricsHandle, SqlitePressureReport,
    SqliteRequest, SqliteResponse, SqliteResult, SqliteValue, SqliteWorker, send_request,
};

use crate::{RunMode, RunReport, get, post, put};

type PoolAddr = Address<WorkerPoolMsg<KeepaliveConnAddr>, WorkerPoolReply<KeepaliveConnAddr>>;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const BODY_CAP_BYTES: usize = 32;
const CONTROLLER_MAILBOX_CAPACITY: usize = 2;

pub fn run(mode: RunMode) -> anyhow::Result<RunReport> {
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("mini-saas.sqlite");
    seed_db(&db_path)?;

    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let sqlite = SqliteWorker::<SingleShard>::install(
        &runtime,
        SqliteConfig::path(&db_path)
            .with_default_timeout(Duration::from_secs(2))
            .with_busy_timeout(Duration::from_millis(250))
            .with_poll_interval(Duration::from_millis(1))
            .with_mailbox_capacity(2),
    )
    .map_err(|e| anyhow::anyhow!("install sqlite bridge: {e}"))?;

    let notify_service = runtime
        .register_with_capacity::<_, Infallible>(NotifySink::default(), 8)
        .map_err(|e| anyhow::anyhow!("register notify sink: {e:?}"))?;
    let notify_listener_config = listener_config(1024);
    let notify_listener = runtime
        .register_with_capacity::<_, Infallible>(
            HttpListener::<SingleShard, NotifyMsg>::with_config(
                "127.0.0.1:0".parse()?,
                notify_service,
                notify_listener_config,
            ),
            notify_listener_config.listener_mailbox_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register notify listener: {e:?}"))?;
    let notify_bound = runtime.observe_next_bound();
    runtime
        .try_send(notify_listener, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start notify listener: {e:?}"))?;
    let notify_addr = notify_bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("bind notify listener: {e:?}"))?;

    let outbound = build_keepalive_pool(
        &runtime,
        HttpTarget::http_with_host(notify_addr, "notify.local"),
        HttpClientConfig::pressure(),
        PoolConfig::new(1, 0),
        8,
        8,
    )
    .map_err(|e| anyhow::anyhow!("build outbound keepalive pool: {e:?}"))?;

    let controller = runtime
        .register_with_capacity::<_, Infallible>(
            Controller::new(sqlite.address, sqlite.metrics.clone(), outbound.pool),
            CONTROLLER_MAILBOX_CAPACITY,
        )
        .map_err(|e| anyhow::anyhow!("register controller: {e:?}"))?;

    let main_listener_config = listener_config(BODY_CAP_BYTES);
    let main_listener = runtime
        .register_with_capacity::<_, Infallible>(
            HttpListener::<SingleShard, ControllerMsg>::with_config(
                "127.0.0.1:0".parse()?,
                controller,
                main_listener_config,
            ),
            main_listener_config.listener_mailbox_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register main listener: {e:?}"))?;
    let main_bound = runtime.observe_next_bound();
    runtime
        .try_send(main_listener, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start main listener: {e:?}"))?;
    let addr = main_bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("bind main listener: {e:?}"))?;

    let mut report = drive_script(addr, mode)?;

    match runtime.call_blocking(controller, ControllerMsg::CloseIngress, REQUEST_TIMEOUT)? {
        CallOutcome::Replied(response) if response.status == StatusCode::OK => {}
        other => anyhow::bail!("close ingress control call failed: {other:?}"),
    }
    let during_shutdown = get(addr, "/ready")?;
    report.ready_during_shutdown_503 =
        during_shutdown.status == 503 && during_shutdown.body.contains("ingress_stopped");
    let rejected_after_close = post(addr, "/items", "name=after-close")?;
    report.ingress_rejects_after_close =
        rejected_after_close.status == 503 && rejected_after_close.body.contains("ingress_stopped");

    sqlite.closer.close();
    let after_db_close = get(addr, "/ready")?;
    report.ready_after_db_close_503 =
        after_db_close.status == 503 && after_db_close.body.contains("db_closed");

    let db_pressure = sqlite.metrics.pressure_report();
    let outbound_shutdown = shutdown_keepalive_pool(
        &runtime,
        &outbound,
        CloseMode::Drain,
        Duration::from_secs(2),
    )
    .map_err(|e| anyhow::anyhow!("shutdown keepalive pool: {e:?}"))?;
    runtime
        .try_send(notify_listener, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("stop notify listener: {e:?}"))?;
    runtime
        .try_send(main_listener, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("stop main listener: {e:?}"))?;
    let trace = runtime.shutdown().unwrap_or_default();
    let pressure = tina_runtime::pressure::PressureSummary::from_events(&trace);
    let deferred_replies = trace
        .iter()
        .filter(|event| matches!(event.kind(), RuntimeEventKind::DeferredReplySent { .. }))
        .count();

    report.shutdown_clean = matches!(outbound_shutdown.drain, KeepalivePoolDrainOutcome::Drained);
    report.multi_turn_notify = report.notified_item && deferred_replies >= 3;
    report.capacity_line = format!(
        "capacity {} db.capacity={} db.closed={} outbound.drain={:?} trace_pressure={}",
        report.capacity_line.trim(),
        db_pressure.capacity,
        db_pressure.closed_count,
        outbound_shutdown.drain,
        pressure
    );

    Ok(report)
}

fn drive_script(addr: std::net::SocketAddr, mode: RunMode) -> anyhow::Result<RunReport> {
    let mut report = RunReport::default();

    let health = get(addr, "/health")?;
    report.health_ok = health.status == 200 && health.body.contains("alive");

    let ready = get(addr, "/ready")?;
    report.ready_ok = ready.status == 200 && ready.body.contains("ready");

    let created = post(addr, "/items", "name=alpha")?;
    report.created_item = created.status == 201 && created.body.contains("id=1");

    let got = get(addr, "/items/1")?;
    report.read_item = got.status == 200 && got.body.contains("alpha");

    let notified = post(addr, "/items/1/notify", "")?;
    report.notified_item = notified.status == 200 && notified.body.contains("notified");

    report.missing_404 = get(addr, "/items/999")?.status == 404;
    report.method_405 = put(addr, "/items/1")?.status == 405;
    report.bad_request_400 = post(addr, "/items", "bad")?.status == 400;
    let body_cap_body = "name=abcdefghijklmnopqrstuvwxyz0123456789";
    let body_cap = post(addr, "/items", body_cap_body)?;
    report.body_cap_413 = body_cap.status == 413;
    report.live_replay_fact = live_replay_fact(ReplayCase {
        name: "mini_saas_body_full",
        method: "post",
        path: "/items",
        request_body_bytes: body_cap_body.len(),
        cap: BODY_CAP_BYTES,
        status: body_cap.status,
    })?;
    report.db_constraint_409 = post(addr, "/items", "name=alpha")?.status == 409;

    if matches!(mode, RunMode::Pressure) {
        let a = addr;
        let b = addr;
        let t1 = std::thread::spawn(move || post(a, "/items/1/notify", "slow"));
        wait_for_capacity(addr, "outbound.in_flight=1", Duration::from_secs(2))?;
        let t2 = std::thread::spawn(move || post(b, "/items/1/notify", ""));
        let r1 = t1
            .join()
            .map_err(|_| anyhow::anyhow!("first pressure request panicked"))??;
        let r2 = t2
            .join()
            .map_err(|_| anyhow::anyhow!("second pressure request panicked"))??;
        report.outbound_pressure_503 = [r1.status, r2.status].contains(&503);
    }

    let capacity = get(addr, "/debug/capacity")?;
    report.capacity_line = capacity.body;
    Ok(report)
}

struct ReplayCase {
    name: &'static str,
    method: &'static str,
    path: &'static str,
    request_body_bytes: usize,
    cap: usize,
    status: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReplayOp {
    Post {
        path: &'static str,
        body_bytes: usize,
        status: u16,
    },
}

fn live_replay_fact(case: ReplayCase) -> anyhow::Result<String> {
    anyhow::ensure!(
        case.request_body_bytes > case.cap,
        "replay case {} does not exceed body cap: body={} cap={}",
        case.name,
        case.request_body_bytes,
        case.cap
    );
    anyhow::ensure!(
        case.status == 413,
        "replay case {} expected status_413, got status_{}",
        case.name,
        case.status
    );
    let op = ReplayOp::Post {
        path: case.path,
        body_bytes: case.request_body_bytes,
        status: case.status,
    };
    let replay_case = DstReplayCase::new(
        case.name,
        83,
        ReplayConfig::default(),
        "mini-saas body cap request",
        vec![op],
        "oversize POST returns typed body-cap status",
    );
    let capacity_fact = LiveReplayFact::capacity_surface(&CapacitySurfaceReport::weighted(
        "mini_saas.http.body",
        CapacityMode::Fixed,
        case.cap,
        0,
        case.request_body_bytes,
        1,
        "bytes",
    ));
    check_live_replay_case(&replay_case, capacity_fact)
        .map_err(|e| anyhow::anyhow!("live replay capture mismatch: {e}"))?;
    Ok(format!(
        "case={} ops=[{}:{}:{}bytes] fact=status_413 cap={}",
        case.name, case.method, case.path, case.request_body_bytes, case.cap
    ))
}

fn check_live_replay_case(
    case: &DstReplayCase<ReplayOp>,
    capacity_fact: LiveReplayFact,
) -> Result<(), Box<tina_sim::dst::CapturedReplayMismatch<ReplayOp>>> {
    let runner = |case: &DstReplayCase<ReplayOp>| {
        let Some(ReplayOp::Post {
            body_bytes, status, ..
        }) = case.history.operations().first()
        else {
            return Err(tina_sim::dst::TraceProjectionError {
                event_id: tina_runtime::EventId::new(0),
                kind: None,
                reason: "missing materialized replay op".to_owned(),
            });
        };
        let report = ReplayReport::from_case_and_events(case, &[] as &[RuntimeEvent], *status);
        let fact = LiveReplayFact::capacity_surface(&CapacitySurfaceReport::weighted(
            "mini_saas.http.body",
            CapacityMode::Fixed,
            BODY_CAP_BYTES,
            0,
            *body_bytes,
            u64::from(*status == 413),
            "bytes",
        ));
        Ok(LiveReplayReport::exact(report).with_live_fact(fact))
    };
    let report = runner(case).expect("local live replay runner uses exact projection");
    let capture =
        LiveReplayCapture::from_case_and_report(case, "mini_saas_api_live", &report.replay)
            .with_live_fact(capacity_fact);
    check_captured_replay(&capture, &capture.to_replay_case(), runner).map(|_| ())
}

fn wait_for_capacity(
    addr: std::net::SocketAddr,
    needle: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        match get(addr, "/debug/capacity") {
            Ok(response) => {
                last = response.body;
                if last.contains(needle) {
                    return Ok(());
                }
            }
            Err(error) => last = format!("capacity probe failed: {error}"),
        }
        std::thread::yield_now();
    }
    anyhow::bail!("timed out waiting for capacity {needle}; last={last:?}")
}

fn listener_config(max_body_bytes: usize) -> HttpServerConfig {
    let mut config = HttpServerConfig::pressure();
    config.limits.max_body_bytes = max_body_bytes;
    config.limits.keepalive_idle_timeout = Some(Duration::from_millis(500));
    config.service_call_timeout = Duration::from_secs(3);
    config
}

fn seed_db(path: &Path) -> anyhow::Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE items (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        );",
    )?;
    Ok(())
}

#[derive(Default)]
struct NotifySink {
    accepted: u64,
}

enum NotifyMsg {
    Request(HttpRequest),
    Delayed(RequestContext<HttpResponse>),
}

impl From<HttpRequest> for NotifyMsg {
    fn from(request: HttpRequest) -> Self {
        Self::Request(request)
    }
}

impl Isolate for NotifySink {
    tina::isolate_types! {
        message: NotifyMsg,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<NotifyMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            NotifyMsg::Request(_) => noop(),
            NotifyMsg::Delayed(req) => {
                self.accepted += 1;
                reply_to_request(req, text(StatusCode::OK, "accepted\n"))
            }
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            NotifyMsg::Request(request) => {
                if request.method != http::Method::POST || request.path != "/notify" {
                    return call.reply(text(StatusCode::NOT_FOUND, "missing\n"));
                }
                if body_text(&request).contains("fail") {
                    return call
                        .reply(text(StatusCode::INTERNAL_SERVER_ERROR, "upstream_failed\n"));
                }
                if body_text(&request).contains("slow") {
                    return call
                        .defer(sleep(Duration::from_millis(250)))
                        .reply(|req, _| NotifyMsg::Delayed(req));
                }
                self.accepted += 1;
                call.reply(text(StatusCode::OK, "accepted\n"))
            }
            NotifyMsg::Delayed(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

struct Controller {
    db: SqliteAddress,
    db_metrics: SqliteMetricsHandle,
    outbound_pool: PoolAddr,
    next_id: i64,
    ingress_open: bool,
    live_items: HashMap<i64, String>,
}

impl Controller {
    fn new(db: SqliteAddress, db_metrics: SqliteMetricsHandle, outbound_pool: PoolAddr) -> Self {
        Self {
            db,
            db_metrics,
            outbound_pool,
            next_id: 1,
            ingress_open: true,
            live_items: HashMap::new(),
        }
    }
}

enum ControllerMsg {
    Http(HttpRequest),
    CloseIngress,
    ReadyDb(
        RequestContext<HttpResponse>,
        bool,
        CallOutcome<SqliteResult>,
    ),
    ReadyPool(
        RequestContext<HttpResponse>,
        bool,
        CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>,
    ),
    Created(
        RequestContext<HttpResponse>,
        i64,
        String,
        CallOutcome<SqliteResult>,
    ),
    Loaded(RequestContext<HttpResponse>, i64, CallOutcome<SqliteResult>),
    NotifyLoaded(
        RequestContext<HttpResponse>,
        i64,
        bool,
        CallOutcome<SqliteResult>,
    ),
    NotifyAcquired(
        RequestContext<HttpResponse>,
        i64,
        String,
        bool,
        CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>,
    ),
    NotifySent(
        RequestContext<HttpResponse>,
        PoolLease<KeepaliveConnAddr>,
        CallOutcome<KeepaliveOutcome>,
    ),
    NotifyReleased(
        RequestContext<HttpResponse>,
        bool,
        CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>,
    ),
    CapacityPool(
        RequestContext<HttpResponse>,
        CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>,
    ),
}

impl From<HttpRequest> for ControllerMsg {
    fn from(request: HttpRequest) -> Self {
        Self::Http(request)
    }
}

impl Isolate for Controller {
    tina::isolate_types! {
        message: ControllerMsg,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<ControllerMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ControllerMsg::Http(_) => noop(),
            ControllerMsg::CloseIngress => {
                self.ingress_open = false;
                noop()
            }
            ControllerMsg::ReadyDb(req, ingress_stopped, outcome) => match outcome {
                CallOutcome::Replied(Ok(_)) => call(
                    self.outbound_pool,
                    WorkerPoolMsg::PressureReport,
                    REQUEST_TIMEOUT,
                )
                .then_with_request(req, move |req, outcome| {
                    ControllerMsg::ReadyPool(req, ingress_stopped, outcome)
                }),
                other => reply_to_request(
                    req,
                    readiness(
                        false,
                        &ready_reasons(ingress_stopped, Some(db_reason(&other))),
                    ),
                ),
            },
            ControllerMsg::ReadyPool(req, ingress_stopped, outcome) => match outcome {
                CallOutcome::Replied(WorkerPoolReply::Pressure(report))
                    if report.available > 0 && !ingress_stopped =>
                {
                    reply_to_request(req, readiness(true, &[]))
                }
                CallOutcome::Replied(WorkerPoolReply::Pressure(report)) if report.available > 0 => {
                    reply_to_request(req, readiness(false, &ready_reasons(ingress_stopped, None)))
                }
                CallOutcome::Replied(WorkerPoolReply::Pressure(_)) => reply_to_request(
                    req,
                    readiness(
                        false,
                        &ready_reasons(ingress_stopped, Some("outbound_full")),
                    ),
                ),
                _ => reply_to_request(
                    req,
                    readiness(
                        false,
                        &ready_reasons(ingress_stopped, Some("outbound_closed")),
                    ),
                ),
            },
            ControllerMsg::Created(req, id, name, outcome) => match outcome {
                CallOutcome::Replied(Ok(SqliteResponse::Executed { .. })) => {
                    self.live_items.insert(id, name);
                    reply_to_request(req, text(StatusCode::CREATED, format!("id={id}\n")))
                }
                CallOutcome::Replied(Err(SqliteError::Constraint(_))) => {
                    reply_to_request(req, text(StatusCode::CONFLICT, "db_constraint\n"))
                }
                other => reply_to_request(req, db_error_response(other)),
            },
            ControllerMsg::Loaded(req, id, outcome) => {
                reply_to_request(req, item_response(id, outcome))
            }
            ControllerMsg::NotifyLoaded(req, id, slow, outcome) => {
                match item_from_rows(id, outcome) {
                    Ok(Some(name)) => {
                        call(self.outbound_pool, WorkerPoolMsg::Acquire, REQUEST_TIMEOUT)
                            .then_with_request(req, move |req, outcome| {
                                ControllerMsg::NotifyAcquired(req, id, name, slow, outcome)
                            })
                    }
                    Ok(None) => reply_to_request(req, text(StatusCode::NOT_FOUND, "not_found\n")),
                    Err(response) => reply_to_request(req, *response),
                }
            }
            ControllerMsg::NotifyAcquired(req, id, name, slow, outcome) => match outcome {
                CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => {
                    let body = if slow {
                        format!("id={id}&name={name}&slow=true")
                    } else {
                        format!("id={id}&name={name}")
                    };
                    let request = HttpRequest::post("/notify").text_body(body).build();
                    call(
                        *lease.handle(),
                        KeepaliveConnectionMsg::request(request, REQUEST_TIMEOUT),
                        REQUEST_TIMEOUT + Duration::from_secs(1),
                    )
                    .then_with_request(req, move |req, outcome| {
                        ControllerMsg::NotifySent(req, lease, outcome)
                    })
                }
                other => reply_to_request(req, pool_acquire_error_response(other)),
            },
            ControllerMsg::NotifySent(req, lease, outcome) => {
                let (ok, disposition) = match &outcome {
                    CallOutcome::Replied(KeepaliveOutcome::Request {
                        result: Ok(response),
                        must_retire,
                    }) => (
                        response.status.is_success(),
                        if *must_retire {
                            ReleaseDisposition::Retire
                        } else {
                            ReleaseDisposition::Reuse
                        },
                    ),
                    _ => (false, ReleaseDisposition::Retire),
                };
                call(
                    self.outbound_pool,
                    WorkerPoolMsg::Release { lease, disposition },
                    REQUEST_TIMEOUT,
                )
                .then_with_request(req, move |req, release| {
                    ControllerMsg::NotifyReleased(req, ok, release)
                })
            }
            ControllerMsg::NotifyReleased(req, ok, release) => match release {
                CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Released)) if ok => {
                    reply_to_request(req, text(StatusCode::OK, "notified\n"))
                }
                CallOutcome::Replied(WorkerPoolReply::Release(_)) if ok => reply_to_request(
                    req,
                    text(StatusCode::SERVICE_UNAVAILABLE, "outbound_release\n"),
                ),
                _ => reply_to_request(req, text(StatusCode::BAD_GATEWAY, "notify_failed\n")),
            },
            ControllerMsg::CapacityPool(req, outcome) => {
                let db = self.db_metrics.pressure_report();
                let outbound = match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Pressure(report)) => report,
                    _ => PoolPressureReport::default(),
                };
                reply_to_request(req, text(StatusCode::OK, capacity_body(db, outbound)))
            }
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ControllerMsg::Http(request) => self.route(request, call),
            ControllerMsg::CloseIngress => {
                self.ingress_open = false;
                call.reply(text(StatusCode::OK, "ingress_closed\n"))
            }
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl Controller {
    fn route(&mut self, request: HttpRequest, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        let method = request.method.clone();
        let path = request.path.clone();
        match (method, path.as_str()) {
            (http::Method::GET, "/health") => call_ctx.reply(text(StatusCode::OK, "alive\n")),
            (http::Method::GET, "/ready") => {
                let ingress_stopped = !self.ingress_open;
                call_ctx
                    .defer(send_request(
                        self.db,
                        SqliteRequest::query_rows("SELECT 1", 1),
                        REQUEST_TIMEOUT,
                    ))
                    .reply(move |req, outcome| ControllerMsg::ReadyDb(req, ingress_stopped, outcome))
            }
            (http::Method::GET, "/debug/capacity") => {
                call_ctx
                    .defer(call(
                        self.outbound_pool,
                        WorkerPoolMsg::PressureReport,
                        REQUEST_TIMEOUT,
                    ))
                    .reply(ControllerMsg::CapacityPool)
            }
            _ if !self.ingress_open => {
                call_ctx.reply(text(StatusCode::SERVICE_UNAVAILABLE, "ingress_stopped\n"))
            }
            (http::Method::POST, "/items") => self.create_item(request, call_ctx),
            _ if path.starts_with("/items/") => self.item_route(request, call_ctx),
            _ => call_ctx.reply(text(StatusCode::NOT_FOUND, "not_found\n")),
        }
    }

    fn create_item(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        let Some(body) = request.body.as_buffered() else {
            return call.reply(text(
                StatusCode::BAD_REQUEST,
                "streaming_body_unsupported\n",
            ));
        };
        if body.len() > BODY_CAP_BYTES {
            return call.reply(text(StatusCode::PAYLOAD_TOO_LARGE, "body_full\n"));
        }
        let body = String::from_utf8_lossy(body);
        let Some(name) = body.strip_prefix("name=").filter(|s| !s.is_empty()) else {
            return call.reply(text(StatusCode::BAD_REQUEST, "bad_request\n"));
        };
        let id = self.next_id;
        self.next_id += 1;
        let name = name.to_owned();
        call.defer(send_request(
            self.db,
            SqliteRequest::execute("INSERT INTO items (id, name) VALUES (?, ?)").params(vec![
                SqliteValue::Integer(id),
                SqliteValue::Text(name.clone()),
            ]),
            REQUEST_TIMEOUT,
        ))
        .reply(move |req, outcome| ControllerMsg::Created(req, id, name, outcome))
    }

    fn item_route(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        let rest = request.path.trim_start_matches("/items/");
        let (id_text, notify) = match rest.strip_suffix("/notify") {
            Some(id) => (id, true),
            None => (rest, false),
        };
        let Ok(id) = id_text.parse::<i64>() else {
            return call.reply(text(StatusCode::BAD_REQUEST, "bad_id\n"));
        };
        match (&request.method, notify) {
            (&http::Method::GET, false) => {
                call.defer(send_request(
                    self.db,
                    SqliteRequest::query_rows("SELECT name FROM items WHERE id = ?", 1)
                        .params(vec![SqliteValue::Integer(id)]),
                    REQUEST_TIMEOUT,
                ))
                .reply(move |req, outcome| ControllerMsg::Loaded(req, id, outcome))
            }
            (&http::Method::POST, true) => {
                let slow = body_text(&request).contains("slow");
                call.defer(send_request(
                    self.db,
                    SqliteRequest::query_rows("SELECT name FROM items WHERE id = ?", 1)
                        .params(vec![SqliteValue::Integer(id)]),
                    REQUEST_TIMEOUT,
                ))
                .reply(move |req, outcome| ControllerMsg::NotifyLoaded(req, id, slow, outcome))
            }
            _ => call.reply(text(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed\n")),
        }
    }
}

fn item_response(id: i64, outcome: CallOutcome<SqliteResult>) -> HttpResponse {
    match item_from_rows(id, outcome) {
        Ok(Some(name)) => text(StatusCode::OK, format!("id={id} name={name}\n")),
        Ok(None) => text(StatusCode::NOT_FOUND, "not_found\n"),
        Err(response) => *response,
    }
}

fn item_from_rows(
    id: i64,
    outcome: CallOutcome<SqliteResult>,
) -> Result<Option<String>, Box<HttpResponse>> {
    match outcome {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { rows, .. })) => {
            let Some(row) = rows.first() else {
                return Ok(None);
            };
            let Some(name) = row.first().and_then(SqliteValue::as_text) else {
                return Err(Box::new(text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "decode_error\n",
                )));
            };
            let _ = id;
            Ok(Some(name.to_owned()))
        }
        other => Err(Box::new(db_error_response(other))),
    }
}

fn db_error_response(outcome: CallOutcome<SqliteResult>) -> HttpResponse {
    match outcome {
        CallOutcome::Replied(Err(SqliteError::Full)) | CallOutcome::Full => {
            text(StatusCode::SERVICE_UNAVAILABLE, "db_full\n")
        }
        CallOutcome::Replied(Err(SqliteError::Closed)) | CallOutcome::Closed => {
            text(StatusCode::SERVICE_UNAVAILABLE, "db_closed\n")
        }
        CallOutcome::Replied(Err(SqliteError::Timeout)) | CallOutcome::Timeout => {
            text(StatusCode::GATEWAY_TIMEOUT, "db_timeout\n")
        }
        CallOutcome::Replied(Err(SqliteError::Constraint(_))) => {
            text(StatusCode::CONFLICT, "db_constraint\n")
        }
        CallOutcome::Replied(Err(_)) | CallOutcome::Rejected(_) => {
            text(StatusCode::INTERNAL_SERVER_ERROR, "db_error\n")
        }
        CallOutcome::Replied(Ok(_)) => text(StatusCode::INTERNAL_SERVER_ERROR, "db_shape\n"),
    }
}

fn db_reason(outcome: &CallOutcome<SqliteResult>) -> &'static str {
    match outcome {
        CallOutcome::Replied(Err(SqliteError::Closed)) | CallOutcome::Closed => "db_closed",
        CallOutcome::Replied(Err(SqliteError::Full)) | CallOutcome::Full => "db_full",
        CallOutcome::Replied(Err(SqliteError::Timeout)) | CallOutcome::Timeout => "db_timeout",
        _ => "db_error",
    }
}

fn pool_acquire_error_response(
    outcome: CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>,
) -> HttpResponse {
    match outcome {
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Full))
        | CallOutcome::Full => text(StatusCode::SERVICE_UNAVAILABLE, "outbound_full\n"),
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Closed))
        | CallOutcome::Closed => text(StatusCode::SERVICE_UNAVAILABLE, "outbound_closed\n"),
        CallOutcome::Timeout => text(StatusCode::GATEWAY_TIMEOUT, "outbound_timeout\n"),
        _ => text(StatusCode::SERVICE_UNAVAILABLE, "outbound_unavailable\n"),
    }
}

fn readiness(ok: bool, reasons: &[&str]) -> HttpResponse {
    if ok {
        text(StatusCode::OK, "ready\n")
    } else {
        text(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("not_ready reasons={}\n", reasons.join(",")),
        )
    }
}

fn ready_reasons(ingress_stopped: bool, reason: Option<&'static str>) -> Vec<&'static str> {
    let mut reasons = Vec::with_capacity(2);
    if ingress_stopped {
        reasons.push("ingress_stopped");
    }
    if let Some(reason) = reason {
        reasons.push(reason);
    }
    reasons
}

fn capacity_body(db: SqlitePressureReport, outbound: PoolPressureReport) -> String {
    format!(
        "http.body_bytes={BODY_CAP_BYTES} controller.mailbox={CONTROLLER_MAILBOX_CAPACITY} \
         db.waiters={} db.in_flight={} db.high_water={} db.full={} outbound.waiters={} \
         outbound.in_flight={} outbound.high_water_waiters={} outbound.full={}\n",
        db.waiters,
        db.leased,
        db.high_water,
        db.full_count,
        outbound.waiters,
        outbound.leased,
        outbound.high_water_waiters,
        outbound.full_count,
    )
}

fn body_text(request: &HttpRequest) -> String {
    match &request.body {
        HttpRequestBody::Buffered(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        HttpRequestBody::Stream(_) => String::new(),
    }
}

fn text(status: StatusCode, body: impl Into<String>) -> HttpResponse {
    let mut response = HttpResponse::with_body(status, body.into().into_bytes());
    response.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain"),
    );
    response
}
