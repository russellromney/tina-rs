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
    BodyMetrics, BodyPressureReport, HttpClientConfig, HttpListener, HttpListenerMsg, HttpRequest,
    HttpRequestBody, HttpResponse, HttpServerConfig, HttpTarget, KeepaliveConnAddr,
    KeepaliveConnectionMsg, KeepaliveOutcome, KeepalivePoolDrainOutcome, build_keepalive_pool,
    shutdown_keepalive_pool,
};
use tina_runtime::lifecycle::{
    CloseAdmission, CloseOutcome, Health, Lifecycle, Readiness, ReadinessReason,
    ResourceCloseReport, ResourceKind, ServiceTopology, ShutdownChoreography, ShutdownStep,
    StepOutcome, TopologyComponent,
};
use tina_runtime::pool::{WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, DrainStage, DrainState, RuntimeCall, RuntimeEvent,
    RuntimeEventKind, ThreadedRuntime, ThreadedRuntimeConfig, call, sleep,
};
use tina_sim::dst::{
    LiveReplayCapture, LiveReplayFact, LiveReplayReport, ReplayCase as DstReplayCase, ReplayConfig,
    ReplayReport, check_captured_replay,
};
use tina_sqlite_bridge::{
    SqliteAddress, SqliteConfig, SqliteError, SqliteMetricsHandle, SqlitePressureReport,
    SqliteRequest, SqliteResponse, SqliteResult, SqliteValue, SqliteWorker, send_request,
};

use crate::{RunMode, RunReport, UserObservation, get, post, put};

type PoolAddr = Address<WorkerPoolMsg<KeepaliveConnAddr>, WorkerPoolReply<KeepaliveConnAddr>>;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
// Caps are declared once in `crate::budget`. Install sites read the
// operative caps back from the validated manifest (`ServiceCaps`); the
// in-isolate body check, display formatting, and the replay fact use
// these two consts, which the manifest is built from and a test ties to
// the manifest rows.
use crate::budget::{BODY_CAP_BYTES, CONTROLLER_MAILBOX_CAPACITY};

pub fn run(mode: RunMode) -> anyhow::Result<RunReport> {
    // Validate the budget manifest before binding anything. A bad cap
    // fails here with typed errors, not at first traffic. The operative
    // install caps are then read back from the manifest object, so the
    // listener/bridge/pool below are configured *from the manifest*.
    let manifest = crate::budget::manifest();
    manifest
        .validate()
        .map_err(|errors| anyhow::anyhow!("invalid budget manifest: {errors:?}"))?;
    let caps = crate::budget::ServiceCaps::from_manifest(&manifest)
        .map_err(|missing| anyhow::anyhow!("manifest missing install caps: {missing:?}"))?;

    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("mini-saas.sqlite");
    seed_db(&db_path)?;

    // Typed lifecycle witness. The host records every state the service
    // passes through so the plan's "Starting → Ready → Draining →
    // Stopped" assertion is one Vec<Lifecycle> instead of being implied
    // across the topology/health/shutdown fields.
    let mut lifecycle_transitions: Vec<Lifecycle> = vec![Lifecycle::Starting];

    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let sqlite = SqliteWorker::<SingleShard>::install(
        &runtime,
        SqliteConfig::path(&db_path)
            .with_default_timeout(Duration::from_secs(2))
            .with_busy_timeout(Duration::from_millis(250))
            .with_poll_interval(Duration::from_millis(1))
            .with_mailbox_capacity(caps.sqlite_mailbox),
    )
    .map_err(|e| anyhow::anyhow!("install sqlite bridge: {e}"))?;

    let notify_service = runtime
        .register_with_capacity::<_, Infallible>(NotifySink::default(), caps.notify_mailbox)
        .map_err(|e| anyhow::anyhow!("register notify sink: {e:?}"))?;
    let notify_listener_config = listener_config(caps.notify_body);
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
        PoolConfig::new(caps.outbound_pool, 0),
        caps.outbound_connection_mailbox,
        caps.outbound_pool_mailbox,
    )
    .map_err(|e| anyhow::anyhow!("build outbound keepalive pool: {e:?}"))?;

    let public_body_metrics = BodyMetrics::default();
    let controller = runtime
        .register_with_capacity::<_, Infallible>(
            Controller::new(
                sqlite.address,
                sqlite.metrics.clone(),
                outbound.pool,
                public_body_metrics.clone(),
                caps.body,
                caps.controller_mailbox,
            ),
            caps.controller_mailbox,
        )
        .map_err(|e| anyhow::anyhow!("register controller: {e:?}"))?;

    let mut main_listener_config = listener_config(caps.body);
    // Accept-queue depth is installed from the manifest too.
    main_listener_config.listener_mailbox_capacity = caps.main_listener_mailbox;
    let main_listener = runtime
        .register_with_capacity::<_, Infallible>(
            HttpListener::<SingleShard, ControllerMsg>::with_config(
                "127.0.0.1:0".parse()?,
                controller,
                main_listener_config,
            )
            // Listener takes a clone so the shutdown budget report can
            // still snapshot live body high-water/full from this handle.
            .with_metrics(public_body_metrics.clone()),
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

    // Startup summary: one compact line naming every bounded surface
    // we declared so far, plus surfaces we *know* exist but cannot
    // measure yet from this scope (sqlite live pressure is sampled
    // later via call). The line is grep-friendly and matches the
    // shape used by ServicePressureReport::summary_line. The same
    // call builds the typed `ServiceTopology` that downstream observers
    // can pattern-match instead of grepping.
    let startup = build_startup_summary(addr, notify_addr);

    // Both listeners bound, controller registered, bridges installed.
    // The service is now in Ready.
    lifecycle_transitions.push(Lifecycle::Ready);

    let mut report = drive_script(addr, mode)?;
    report.startup_summary_line = startup.summary_line;
    report.startup_discovery_lines = startup.discovery_lines;
    report.topology = Some(startup.topology);

    let in_flight_addr = addr;
    let in_flight = std::thread::spawn(move || post(in_flight_addr, "/items/1/notify", "slow"));
    wait_for_capacity(addr, "outbound.in_flight=1", Duration::from_secs(2))?;

    // Shutdown choreography: every step the host drives is recorded with
    // its kind, label, elapsed, and outcome. The terminal report carries
    // the same facts as the `terminal_line` string but in typed form so
    // tests and dashboards can pattern-match instead of parsing.
    let mut choreo = ShutdownChoreography::new("mini_saas_api");

    let t_close = Instant::now();
    match runtime.call_blocking(controller, ControllerMsg::CloseIngress, REQUEST_TIMEOUT)? {
        CallOutcome::Replied(response) if response.status == StatusCode::OK => {
            // Ingress closed; the controller's DrainState is now Draining.
            lifecycle_transitions.push(Lifecycle::Draining);
            choreo.record(
                ShutdownStep::StopIngress,
                "close_ingress",
                t_close.elapsed(),
                StepOutcome::Clean,
            );
        }
        other => {
            choreo.record(
                ShutdownStep::StopIngress,
                "close_ingress",
                t_close.elapsed(),
                StepOutcome::Failed {
                    reason: format!("close ingress control call failed: {other:?}"),
                },
            );
            anyhow::bail!("close ingress control call failed: {other:?}");
        }
    }
    let t_drain = Instant::now();
    let in_flight_response = in_flight
        .join()
        .map_err(|_| anyhow::anyhow!("shutdown in-flight request panicked"))??;
    let in_flight_clean =
        in_flight_response.status == 200 && in_flight_response.body.contains("notified");
    choreo.record(
        ShutdownStep::DrainInFlight,
        "drain_in_flight_notify",
        t_drain.elapsed(),
        if in_flight_clean {
            StepOutcome::Clean
        } else {
            StepOutcome::Failed {
                reason: format!(
                    "in-flight drain returned status={} body={:?}",
                    in_flight_response.status, in_flight_response.body,
                ),
            }
        },
    );
    report.shutdown_in_flight_typed = in_flight_clean;
    report.observations.push(observation(
        "shutdown_in_flight_notify",
        in_flight_response.status,
        &in_flight_response.body,
    ));
    let during_shutdown = get(addr, "/ready")?;
    report.ready_during_shutdown_503 =
        during_shutdown.status == 503 && during_shutdown.body.contains("ingress_stopped");
    report.observations.push(observation(
        "ready_during_shutdown",
        during_shutdown.status,
        &during_shutdown.body,
    ));
    let capacity_during_shutdown = get(addr, "/debug/capacity")?;
    report.capacity_during_shutdown_line =
        format!("capacity_during_shutdown {}", capacity_during_shutdown.body);
    let rejected_after_close = post(addr, "/items", "name=after-close")?;
    report.ingress_rejects_after_close =
        rejected_after_close.status == 503 && rejected_after_close.body.contains("ingress_stopped");
    report.observations.push(observation(
        "post_after_ingress_close",
        rejected_after_close.status,
        &rejected_after_close.body,
    ));

    let t_db = Instant::now();
    sqlite.closer.close();
    let db_close_report = ResourceCloseReport::clean(
        "db.bridge",
        ResourceKind::Bridge,
        CloseAdmission::Drain,
        t_db.elapsed(),
    );
    choreo.record_close(&db_close_report, "close_sqlite_bridge");
    let after_db_close = get(addr, "/ready")?;
    report.ready_after_db_close_503 =
        after_db_close.status == 503 && after_db_close.body.contains("db_closed");
    report.observations.push(observation(
        "ready_after_db_close",
        after_db_close.status,
        &after_db_close.body,
    ));

    let db_pressure = sqlite.metrics.pressure_report();
    let t_pool = Instant::now();
    let outbound_shutdown = shutdown_keepalive_pool(
        &runtime,
        &outbound,
        CloseMode::Drain,
        Duration::from_secs(2),
    )
    .map_err(|e| anyhow::anyhow!("shutdown keepalive pool: {e:?}"))?;
    let outbound_close_report =
        pool_shutdown_to_close_report("outbound.pool", &outbound_shutdown, t_pool.elapsed());
    choreo.record_close(&outbound_close_report, "close_outbound_pool");
    let t_notify_listener = Instant::now();
    runtime
        .try_send(notify_listener, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("stop notify listener: {e:?}"))?;
    choreo.record_close(
        &ResourceCloseReport::clean(
            "notify.listener",
            ResourceKind::Listener,
            CloseAdmission::Drain,
            t_notify_listener.elapsed(),
        ),
        "stop_notify_listener",
    );
    let t_main_listener = Instant::now();
    runtime
        .try_send(main_listener, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("stop main listener: {e:?}"))?;
    choreo.record_close(
        &ResourceCloseReport::clean(
            "main.listener",
            ResourceKind::Listener,
            CloseAdmission::Drain,
            t_main_listener.elapsed(),
        ),
        "stop_main_listener",
    );
    let t_runtime = Instant::now();
    let trace = runtime
        .shutdown()
        .map_err(|e| anyhow::anyhow!("runtime shutdown: {e:?}"))?;
    choreo.record(
        ShutdownStep::StopOwner,
        "shutdown_runtime",
        t_runtime.elapsed(),
        StepOutcome::Clean,
    );
    let pressure = tina_runtime::pressure::PressureSummary::from_events(&trace);
    let deferred_replies = trace
        .iter()
        .filter(|event| matches!(event.kind(), RuntimeEventKind::DeferredReplySent { .. }))
        .count();

    let shutdown_report = choreo.finish();
    report.shutdown_clean = matches!(outbound_shutdown.drain, KeepalivePoolDrainOutcome::Drained)
        && outbound_shutdown.requested == outbound_shutdown.stopped
        && outbound_shutdown.timed_out == 0
        && outbound_shutdown.rejected == 0
        && outbound_shutdown.already_closed == 0
        && outbound_shutdown.connection_failures.is_empty()
        && shutdown_report.clean;
    report.shutdown_report = Some(shutdown_report);
    report.health_pre_shutdown = Some(
        Health::new("mini_saas_api", Lifecycle::Stopped)
            .with_pressure(build_pressure_snapshot(&db_pressure)),
    );
    lifecycle_transitions.push(Lifecycle::Stopped);
    report.lifecycle_transitions = lifecycle_transitions;
    report.multi_turn_notify = report.notified_item && deferred_replies >= 3;

    // Join the declared manifest with what the run actually observed:
    // body high-water/full from BodyMetrics, db in-flight from the
    // bridge report. Caps come from the manifest; live numbers come
    // from the reports, never the other way around.
    let live_budget = live_budget_pressure(
        &public_body_metrics.snapshot(),
        main_listener_config.limits.max_body_bytes,
        &db_pressure,
    );
    let budget_report = manifest.report(&live_budget);
    report.budget_report_line = budget_report.summary_line();
    report.budget_replay_line = manifest.replay_export().summary_line();
    report.budget_consistent = budget_report.consistency.is_consistent();
    report.budget_report = Some(budget_report);

    report.terminal_line = format!(
        "terminal db.capacity={} db.closed={} outbound.drain={:?} outbound.stop_requested={} \
         outbound.stop_stopped={} outbound.stop_timed_out={} outbound.stop_rejected={} \
         outbound.stop_already_closed={} outbound.stop_failures={} trace_pressure={}",
        db_pressure.capacity,
        db_pressure.closed_count,
        outbound_shutdown.drain,
        outbound_shutdown.requested,
        outbound_shutdown.stopped,
        outbound_shutdown.timed_out,
        outbound_shutdown.rejected,
        outbound_shutdown.already_closed,
        outbound_shutdown.connection_failures.len(),
        pressure
    );

    Ok(report)
}

fn drive_script(addr: std::net::SocketAddr, mode: RunMode) -> anyhow::Result<RunReport> {
    let mut report = RunReport::default();

    let health = get(addr, "/health")?;
    report.health_ok = health.status == 200 && health.body.contains("alive");
    report
        .observations
        .push(observation("health", health.status, &health.body));

    let ready = get(addr, "/ready")?;
    report.ready_ok = ready.status == 200 && ready.body.contains("ready");
    report
        .observations
        .push(observation("ready", ready.status, &ready.body));

    let created = post(addr, "/items", "name=alpha")?;
    report.created_item = created.status == 201 && created.body.contains("id=1");
    report
        .observations
        .push(observation("create_item", created.status, &created.body));

    let got = get(addr, "/items/1")?;
    report.read_item = got.status == 200 && got.body.contains("alpha");
    report
        .observations
        .push(observation("read_item", got.status, &got.body));

    let notified = post(addr, "/items/1/notify", "")?;
    report.notified_item = notified.status == 200 && notified.body.contains("notified");
    report
        .observations
        .push(observation("notify_item", notified.status, &notified.body));

    let peer_close = post(addr, "/items/1/notify", "close")?;
    report.observations.push(observation(
        "notify_peer_close",
        peer_close.status,
        &peer_close.body,
    ));
    let after_peer_close = post(addr, "/items/1/notify", "")?;
    report.notify_after_peer_close = peer_close.status == 200
        && after_peer_close.status == 200
        && after_peer_close.body.contains("notified");
    report.observations.push(observation(
        "notify_after_peer_close",
        after_peer_close.status,
        &after_peer_close.body,
    ));

    let missing = get(addr, "/items/999")?;
    report.missing_404 = missing.status == 404;
    report
        .observations
        .push(observation("missing_item", missing.status, &missing.body));
    let method = put(addr, "/items/1")?;
    report.method_405 = method.status == 405;
    report.observations.push(observation(
        "method_not_allowed",
        method.status,
        &method.body,
    ));
    let bad_request = post(addr, "/items", "bad")?;
    report.bad_request_400 = bad_request.status == 400;
    report.observations.push(observation(
        "bad_create_body",
        bad_request.status,
        &bad_request.body,
    ));
    let body_cap_body = "name=abcdefghijklmnopqrstuvwxyz0123456789";
    let body_cap = post(addr, "/items", body_cap_body)?;
    report.body_cap_413 = body_cap.status == 413;
    report.observations.push(observation(
        "parser_body_cap",
        body_cap.status,
        &body_cap.body,
    ));
    report.live_replay_fact = live_replay_fact(ReplayCase {
        name: "mini_saas_body_full",
        method: "post",
        path: "/items",
        request_body_bytes: body_cap_body.len(),
        cap: BODY_CAP_BYTES,
        status: body_cap.status,
    })?;
    let duplicate = post(addr, "/items", "name=alpha")?;
    report.db_constraint_409 = duplicate.status == 409;
    report.observations.push(observation(
        "duplicate_create",
        duplicate.status,
        &duplicate.body,
    ));

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
        report.outbound_pressure_503 = [(&r1, "pressure_first"), (&r2, "pressure_second")]
            .into_iter()
            .any(|(response, _)| response.status == 503 && response.body.contains("outbound_full"));
        report
            .observations
            .push(observation("pressure_first", r1.status, &r1.body));
        report
            .observations
            .push(observation("pressure_second", r2.status, &r2.body));
    }

    let capacity = get(addr, "/debug/capacity")?;
    report.capacity_before_shutdown_line = format!("capacity_before_shutdown {}", capacity.body);
    Ok(report)
}

fn observation(label: &'static str, status: u16, body: &str) -> UserObservation {
    UserObservation {
        label,
        status,
        body: body.trim().to_owned(),
    }
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
    // Pin the budget config the case depends on: the replay-affecting
    // hash changes if the body cap (or any replay-affecting cap)
    // changes, so a saved case never silently rides ambient defaults.
    let replay = crate::budget::manifest().replay_export();
    Ok(format!(
        "case={} ops=[{}:{}:{}bytes] fact=status_413 cap={} budget_schema={} budget_hash={:016x}",
        case.name,
        case.method,
        case.path,
        case.request_body_bytes,
        case.cap,
        replay.schema_version,
        replay.replay_affecting_hash,
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
                if body_text(&request).contains("close") {
                    self.accepted += 1;
                    let mut response = text(StatusCode::OK, "accepted\n");
                    response.headers.insert(
                        http::header::CONNECTION,
                        http::HeaderValue::from_static("close"),
                    );
                    return call.reply(response);
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
    body_metrics: BodyMetrics,
    /// Public body cap, read from the budget manifest at startup. The
    /// listener enforces the same cap; this is the handler-side check
    /// for buffered bodies that reach the isolate.
    body_cap: usize,
    /// Mailbox cap the controller was registered with, read from the
    /// manifest. Surfaced in `/debug/capacity` so the reported value
    /// comes from the manifest object, not a separate const.
    controller_mailbox: usize,
    next_id: i64,
    /// Public-ingress admission state. The controller drives `Open` →
    /// `Draining` on `CloseIngress`; per-request completion lives in the
    /// listener/runtime, so the helper is used purely for the typed stage
    /// label surfaced in `/debug/capacity`. The host owns terminal proof, so
    /// `drain.finish()` is never called from inside the controller.
    drain: DrainState,
    live_items: HashMap<i64, String>,
}

impl Controller {
    fn new(
        db: SqliteAddress,
        db_metrics: SqliteMetricsHandle,
        outbound_pool: PoolAddr,
        body_metrics: BodyMetrics,
        body_cap: usize,
        controller_mailbox: usize,
    ) -> Self {
        Self {
            db,
            db_metrics,
            outbound_pool,
            body_metrics,
            body_cap,
            controller_mailbox,
            next_id: 1,
            drain: DrainState::new(),
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
                self.drain.begin();
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
                    readiness_response(&build_readiness(ingress_stopped, Some(db_reason(&other)))),
                ),
            },
            ControllerMsg::ReadyPool(req, ingress_stopped, outcome) => {
                let dep = match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Pressure(report))
                        if report.available > 0 =>
                    {
                        None
                    }
                    CallOutcome::Replied(WorkerPoolReply::Pressure(_)) => {
                        Some(ReadinessReason::DependencyFull("outbound"))
                    }
                    _ => Some(ReadinessReason::DependencyClosed("outbound")),
                };
                reply_to_request(
                    req,
                    readiness_response(&build_readiness(ingress_stopped, dep)),
                )
            }
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
                        ..
                    }) => (response.status.is_success(), ReleaseDisposition::Reuse),
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
                let body = self.body_metrics.snapshot();
                let db = self.db_metrics.pressure_report();
                let outbound = match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Pressure(report)) => report,
                    _ => PoolPressureReport::default(),
                };
                reply_to_request(
                    req,
                    text(
                        StatusCode::OK,
                        capacity_body(
                            body,
                            db,
                            outbound,
                            self.drain.stage(),
                            self.body_cap,
                            self.controller_mailbox,
                        ),
                    ),
                )
            }
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ControllerMsg::Http(request) => self.route(request, call),
            ControllerMsg::CloseIngress => {
                self.drain.begin();
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
                let ingress_stopped = !self.drain.is_open();
                call_ctx
                    .defer(send_request(
                        self.db,
                        SqliteRequest::query_rows("SELECT 1", 1),
                        REQUEST_TIMEOUT,
                    ))
                    .reply(move |req, outcome| {
                        ControllerMsg::ReadyDb(req, ingress_stopped, outcome)
                    })
            }
            (http::Method::GET, "/debug/capacity") => call_ctx
                .defer(call(
                    self.outbound_pool,
                    WorkerPoolMsg::PressureReport,
                    REQUEST_TIMEOUT,
                ))
                .reply(ControllerMsg::CapacityPool),
            _ if !self.drain.is_open() => {
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
        if body.len() > self.body_cap {
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
            (&http::Method::GET, false) => call
                .defer(send_request(
                    self.db,
                    SqliteRequest::query_rows("SELECT name FROM items WHERE id = ?", 1)
                        .params(vec![SqliteValue::Integer(id)]),
                    REQUEST_TIMEOUT,
                ))
                .reply(move |req, outcome| ControllerMsg::Loaded(req, id, outcome)),
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

fn db_reason(outcome: &CallOutcome<SqliteResult>) -> ReadinessReason {
    match outcome {
        CallOutcome::Replied(Err(SqliteError::Closed)) | CallOutcome::Closed => {
            ReadinessReason::DependencyClosed("db")
        }
        CallOutcome::Replied(Err(SqliteError::Full)) | CallOutcome::Full => {
            ReadinessReason::DependencyFull("db")
        }
        CallOutcome::Replied(Err(SqliteError::Timeout)) | CallOutcome::Timeout => {
            ReadinessReason::DependencyTimeout("db")
        }
        _ => ReadinessReason::DependencyError("db"),
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

fn readiness_response(readiness: &Readiness) -> HttpResponse {
    let status = if readiness.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    text(status, readiness.legacy_body())
}

// Build the typed `Readiness` for the controller's `/ready` route. The
// state picks Draining when ingress is closed, NotReady when a dependency
// is the only issue, and Ready when both are clean.
fn build_readiness(ingress_stopped: bool, dep: Option<ReadinessReason>) -> Readiness {
    let mut reasons = Vec::with_capacity(2);
    if ingress_stopped {
        reasons.push(ReadinessReason::IngressStopped);
    }
    if let Some(reason) = dep {
        reasons.push(reason);
    }
    if reasons.is_empty() {
        Readiness::ready()
    } else {
        let state = if ingress_stopped {
            Lifecycle::Draining
        } else {
            Lifecycle::NotReady
        };
        Readiness::not_ready(state, reasons)
    }
}

fn capacity_body(
    body: BodyPressureReport,
    db: SqlitePressureReport,
    outbound: PoolPressureReport,
    drain_stage: DrainStage,
    body_cap: usize,
    controller_mailbox: usize,
) -> String {
    let stage = drain_stage_label(drain_stage);
    format!(
        "http.body_cap={body_cap} http.request_body_current={} \
         http.request_body_high_water={} http.response_body_current={} \
         http.response_body_high_water={} http.body_full={} http.body_timeout={} \
         http.body_io_error={} \
         controller.mailbox={controller_mailbox} drain.stage={stage} \
         db.capacity={} db.waiters={} db.max_waiters={} db.in_flight={} db.high_water={} \
         db.full={} db.closed={} db.timeout={} outbound.capacity={} outbound.waiters={} \
         outbound.max_waiters={} outbound.in_flight={} outbound.high_water_waiters={} \
         outbound.full={} outbound.closed={} outbound.closed_count={} outbound.cancel={}\n",
        body.request_body_current,
        body.request_body_high_water,
        body.response_body_current,
        body.response_body_high_water,
        body.body_full_count,
        body.body_timeout_count,
        body.body_io_error_count,
        db.capacity,
        db.waiters,
        db.max_waiters,
        db.leased,
        db.high_water,
        db.full_count,
        db.closed_count,
        db.timeout_count,
        outbound.capacity,
        outbound.waiters,
        outbound.max_waiters,
        outbound.leased,
        outbound.high_water_waiters,
        outbound.full_count,
        outbound.closed,
        outbound.closed_count,
        outbound.cancel_count,
    )
}

// `Stopped` is unreachable in this specimen: the host owns the terminal
// report and tears down the controller before `drain.finish()` would run.
// The arm exists so the label match stays exhaustive against `DrainStage`.
fn drain_stage_label(stage: DrainStage) -> &'static str {
    match stage {
        DrainStage::Open => "open",
        DrainStage::Draining => "draining",
        DrainStage::Stopped => "stopped",
    }
}

fn body_text(request: &HttpRequest) -> String {
    match &request.body {
        HttpRequestBody::Buffered(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        HttpRequestBody::Stream(_) | HttpRequestBody::Http2Stream(_) => String::new(),
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

/// Soak driver. Spins up the same service as [`run`] (controller +
/// SQLite + notify + outbound keepalive pool), drives `config.workers`
/// concurrent clients hitting `GET /health` and `GET /items/1`, then
/// captures `/debug/capacity` and runs the same shutdown sequence as
/// `run`. The proof artifact is the typed [`crate::SoakReport`]: load
/// summary, capacity line, terminal line, and `shutdown_clean`.
///
/// This is intentionally narrow: `/health` is the cheapest path, and
/// `GET /items/1` exercises the controller + SQLite bridge + pool
/// shape without needing a write per op (the row is pre-seeded by the
/// initial create). The point is "many requests, real shutdown, no
/// leaks" — not throughput.
pub fn run_soak(config: crate::SoakConfig) -> anyhow::Result<crate::SoakReport> {
    use tina_proof_harness::load::{self, LoadRun, LoadStop, OpOutcome};

    // Same manifest, same install caps as `run`: the soak service is
    // configured from the manifest too.
    let manifest = crate::budget::manifest();
    manifest
        .validate()
        .map_err(|errors| anyhow::anyhow!("invalid budget manifest: {errors:?}"))?;
    let caps = crate::budget::ServiceCaps::from_manifest(&manifest)
        .map_err(|missing| anyhow::anyhow!("manifest missing install caps: {missing:?}"))?;

    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("mini-saas.sqlite");
    seed_db(&db_path)?;

    let live_trace = tina_proof_harness::LiveTrace::new();
    let runtime = ThreadedRuntime::with_config_and_trace_observer(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
        live_trace.observer(),
    );
    let sqlite = SqliteWorker::<SingleShard>::install(
        &runtime,
        SqliteConfig::path(&db_path)
            .with_default_timeout(Duration::from_secs(2))
            .with_busy_timeout(Duration::from_millis(250))
            .with_poll_interval(Duration::from_millis(1))
            .with_mailbox_capacity(caps.sqlite_mailbox),
    )
    .map_err(|e| anyhow::anyhow!("install sqlite bridge: {e}"))?;

    let notify_service = runtime
        .register_with_capacity::<_, Infallible>(NotifySink::default(), caps.notify_mailbox)
        .map_err(|e| anyhow::anyhow!("register notify sink: {e:?}"))?;
    let notify_listener_config = listener_config(caps.notify_body);
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
        PoolConfig::new(caps.outbound_pool, 0),
        caps.outbound_connection_mailbox,
        caps.outbound_pool_mailbox,
    )
    .map_err(|e| anyhow::anyhow!("build outbound keepalive pool: {e:?}"))?;

    let public_body_metrics = BodyMetrics::default();
    let controller = runtime
        .register_with_capacity::<_, Infallible>(
            Controller::new(
                sqlite.address,
                sqlite.metrics.clone(),
                outbound.pool,
                public_body_metrics.clone(),
                caps.body,
                caps.controller_mailbox,
            ),
            caps.controller_mailbox,
        )
        .map_err(|e| anyhow::anyhow!("register controller: {e:?}"))?;

    let mut main_listener_config = listener_config(caps.body);
    // Accept-queue depth is installed from the manifest too.
    main_listener_config.listener_mailbox_capacity = caps.main_listener_mailbox;
    let main_listener = runtime
        .register_with_capacity::<_, Infallible>(
            HttpListener::<SingleShard, ControllerMsg>::with_config(
                "127.0.0.1:0".parse()?,
                controller,
                main_listener_config,
            )
            .with_metrics(public_body_metrics),
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

    // Pre-seed one row so `GET /items/1` is a hit, not a 404. We use the
    // public POST so the body cap is exercised at least once even from
    // the soak path.
    let create = crate::post(addr, "/items", "name=alpha")
        .map_err(|e| anyhow::anyhow!("seed POST /items failed: {e}"))?;
    if create.status != 201 {
        anyhow::bail!(
            "soak seed POST /items must return 201, got {} ({})",
            create.status,
            create.body,
        );
    }

    // Three lanes, deterministic by worker id, so the report can blame
    // each path independently:
    //
    //   worker % 3 == 0 -> GET /health        (cheap; HTTP only)
    //   worker % 3 == 1 -> GET /items/1       (HTTP + SQLite bridge)
    //   worker % 3 == 2 -> POST /items/1/notify (HTTP + bridge + outbound pool)
    //
    // The third lane is what proves Rock 1's "bridge/pool path" line
    // item — without it the keepalive outbound pool is never exercised
    // and the soak only proves the HTTP+DB shape.
    let op_addr = addr;
    let timeout = config.connect_timeout;
    let load_report = load::run(
        LoadRun {
            workers: config.workers,
            stop: LoadStop::ops(config.op_count),
            label: "mini_saas_api_soak",
        },
        move |worker| {
            let result = match worker % 3 {
                0 => crate::one_request_with_timeout(
                    op_addr,
                    b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
                    timeout,
                ),
                1 => crate::one_request_with_timeout(
                    op_addr,
                    b"GET /items/1 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
                    timeout,
                ),
                _ => crate::one_request_with_timeout(
                    op_addr,
                    b"POST /items/1/notify HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    timeout,
                ),
            };
            match result {
                Ok(parts) if parts.status == 200 => OpOutcome::Ok,
                Ok(parts) => {
                    // Map a typed HTTP status into the err_kinds table so
                    // a 503 burst is visible without log scraping.
                    let kind = match parts.status {
                        503 => "http_503",
                        500..=599 => "http_5xx",
                        429 => "http_429",
                        400..=499 => "http_4xx",
                        _ => "http_other",
                    };
                    OpOutcome::Err { kind }
                }
                Err(_) => OpOutcome::Timeout,
            }
        },
        None::<fn() -> bool>,
    );

    // Snapshot capacity while the runtime is still up.
    let capacity = crate::get(addr, "/debug/capacity")?;
    let pressure_after_load = live_trace.pressure_summary();
    let capacity_after_load_line = format!(
        "capacity_after_load {} runtime.send_full={} runtime.completion_full={} runtime.reply_path_full={}",
        capacity.body.trim_end(),
        pressure_after_load.send_rejected_full,
        pressure_after_load.completion_rejected_mailbox_full,
        pressure_after_load.reply_rejected_reply_path_full,
    );

    // Same shutdown sequence as `run`, minus the scripted assertions.
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
    sqlite.closer.close();
    let trace = runtime
        .shutdown()
        .map_err(|e| anyhow::anyhow!("runtime shutdown: {e:?}"))?;
    let pressure = tina_runtime::pressure::PressureSummary::from_events(&trace);
    let shutdown_clean = matches!(outbound_shutdown.drain, KeepalivePoolDrainOutcome::Drained)
        && outbound_shutdown.requested == outbound_shutdown.stopped
        && outbound_shutdown.timed_out == 0
        && outbound_shutdown.rejected == 0
        && outbound_shutdown.already_closed == 0
        && outbound_shutdown.connection_failures.is_empty();
    let terminal_line = format!(
        "terminal db.capacity={} db.closed={} outbound.drain={:?} outbound.stop_requested={} \
         outbound.stop_stopped={} outbound.stop_timed_out={} outbound.stop_rejected={} \
         outbound.stop_already_closed={} outbound.stop_failures={} trace_pressure={}",
        db_pressure.capacity,
        db_pressure.closed_count,
        outbound_shutdown.drain,
        outbound_shutdown.requested,
        outbound_shutdown.stopped,
        outbound_shutdown.timed_out,
        outbound_shutdown.rejected,
        outbound_shutdown.already_closed,
        outbound_shutdown.connection_failures.len(),
        pressure
    );

    Ok(crate::SoakReport {
        load: load_report,
        capacity_after_load_line,
        terminal_line,
        shutdown_clean,
    })
}

struct StartupSummary {
    summary_line: String,
    discovery_lines: Vec<String>,
    topology: ServiceTopology,
}

/// Wrap a `KeepalivePoolShutdownReport` in the typed
/// [`ResourceCloseReport`] vocabulary so the service can record one
/// uniform close-line per resource while keeping the keepalive-specific
/// counters in the `details` string.
fn pool_shutdown_to_close_report(
    name: &'static str,
    pool: &tina_http::KeepalivePoolShutdownReport,
    elapsed: Duration,
) -> ResourceCloseReport {
    let outcome = match pool.drain {
        KeepalivePoolDrainOutcome::Drained
            if pool.requested == pool.stopped
                && pool.timed_out == 0
                && pool.rejected == 0
                && pool.connection_failures.is_empty() =>
        {
            CloseOutcome::Clean
        }
        KeepalivePoolDrainOutcome::TimedOut { .. } => CloseOutcome::TimedOut { waited: elapsed },
        KeepalivePoolDrainOutcome::PoolAlreadyClosed => CloseOutcome::AlreadyClosed,
        _ => CloseOutcome::Failed {
            reason: format!(
                "drain={:?} requested={} stopped={} timed_out={} rejected={} failures={}",
                pool.drain,
                pool.requested,
                pool.stopped,
                pool.timed_out,
                pool.rejected,
                pool.connection_failures.len(),
            ),
        },
    };
    ResourceCloseReport {
        name: name.to_owned(),
        kind: ResourceKind::Pool,
        admission: CloseAdmission::Drain,
        outcome,
        elapsed,
        details: format!(
            "drain={:?} requested={} stopped={} timed_out={} rejected={} already_closed={} failures={}",
            pool.drain,
            pool.requested,
            pool.stopped,
            pool.timed_out,
            pool.rejected,
            pool.already_closed,
            pool.connection_failures.len(),
        ),
    }
}

/// Build a small pressure snapshot for the typed [`Health`] report.
/// The wire format used by `/debug/capacity` and the terminal line stays
/// the source of truth for live numbers; the typed pressure snapshot is
/// the structured copy for callers that want to match on fields.
fn build_pressure_snapshot(db: &SqlitePressureReport) -> tina_runtime::ServicePressureReport {
    use tina::capacity::{CapacityMode, CapacitySurfaceReport};
    use tina_runtime::ServicePressureReport;

    let mut report = ServicePressureReport::new("mini_saas_api");
    report.add_measured(
        "bridge",
        CapacitySurfaceReport::count(
            "db.bridge.capacity",
            CapacityMode::Fixed,
            db.capacity,
            db.leased,
            db.high_water as usize,
            db.full_count,
        ),
    );
    report
}

/// Live pressure report named to match the budget manifest exactly.
///
/// What the resulting consistency check proves, precisely:
/// - **Cap agreement** for the surfaces the runtime actually samples —
///   `http.request_body` and `db.in_flight` — because those are
///   `Measured` with real numbers from runtime reports.
/// - **Presence + no-extra** for every declared surface: each appears
///   here (measured or explicit `Unavailable`), so a missing or
///   undeclared surface fails the check.
///
/// It does *not* re-derive the caps of the `Unavailable` surfaces — the
/// runtime does not sample per-isolate mailbox depth, and the outbound
/// pool is sampled live via `/debug/capacity` rather than re-called from
/// the host during teardown. For those surfaces the manifest is the
/// *install* source (caps flow manifest -> `ServiceCaps` -> the
/// `register_*` / config calls), which is a code-level guarantee, not a
/// runtime-observed one. Nothing is silently dropped.
///
/// `body_cap` is the cap the listener was *actually* installed with
/// (read from the live config object, not a const), so the body-cap
/// consistency check would catch a listener configured off-manifest.
fn live_budget_pressure(
    body: &BodyPressureReport,
    body_cap: usize,
    db: &SqlitePressureReport,
) -> tina_runtime::ServicePressureReport {
    use tina_runtime::ServicePressureReport;

    let mut report = ServicePressureReport::new("mini_saas_api");
    report.add_measured(
        "body",
        CapacitySurfaceReport::weighted(
            "http.request_body",
            CapacityMode::Fixed,
            body_cap,
            body.request_body_current,
            body.request_body_high_water,
            body.body_full_count,
            "bytes",
        ),
    );
    report.add_measured(
        "bridge",
        CapacitySurfaceReport::count(
            "db.in_flight",
            CapacityMode::Fixed,
            db.capacity,
            db.leased,
            db.high_water as usize,
            db.full_count,
        ),
    );
    for (name, reason) in [
        (
            "controller.mailbox",
            "mailbox depth not individually sampled",
        ),
        ("db.mailbox", "mailbox depth not individually sampled"),
        ("notify.mailbox", "mailbox depth not individually sampled"),
        (
            "notify.request_body",
            "internal notify listener body not sampled from this scope",
        ),
        ("outbound.pool", "sampled live via /debug/capacity"),
        ("outbound.in_flight", "sampled live via /debug/capacity"),
        (
            "outbound.connection.mailbox",
            "mailbox depth not individually sampled",
        ),
        (
            "outbound.pool.mailbox",
            "mailbox depth not individually sampled",
        ),
        (
            "http.main_listener.mailbox",
            "accept-queue depth not sampled from this scope",
        ),
    ] {
        report.add_unavailable(name, "mailbox", reason);
    }
    report
}

fn build_startup_summary(
    main_addr: std::net::SocketAddr,
    notify_addr: std::net::SocketAddr,
) -> StartupSummary {
    use tina::capacity::{CapacityMode, CapacitySurfaceReport};
    use tina_runtime::ServicePressureReport;

    // Surfaces declared at startup. We do not sample live counters
    // here — that happens later via `/debug/capacity`. The startup
    // line is a *topology* snapshot: names + caps, plus explicit
    // Unavailable markers for surfaces we know exist but cannot
    // measure from this scope.
    let body_cap = CapacitySurfaceReport::weighted(
        "http.request_body",
        CapacityMode::Fixed,
        BODY_CAP_BYTES,
        0,
        0,
        0,
        "bytes",
    );
    let controller_mailbox = CapacitySurfaceReport::count(
        "controller.mailbox",
        CapacityMode::Fixed,
        CONTROLLER_MAILBOX_CAPACITY,
        0,
        0,
        0,
    );
    let db_pool = CapacitySurfaceReport::count("db.pool", CapacityMode::Fixed, 1, 0, 0, 0);
    let outbound_pool =
        CapacitySurfaceReport::count("outbound.pool", CapacityMode::Fixed, 1, 0, 0, 0);
    // Listener mailbox is bounded by the HTTP listener config; we
    // declare its name here so on-call sees the surface exists even
    // when live depth/accept counters live behind `LiveQueueReport`
    // and aren't sampled from this scope.
    let main_listener = CapacitySurfaceReport::count(
        "http.main_listener.mailbox",
        CapacityMode::Fixed,
        listener_config(BODY_CAP_BYTES).listener_mailbox_capacity,
        0,
        0,
        0,
    );
    let mut report = ServicePressureReport::new("mini_saas_api");
    report.add_measured("body", body_cap);
    report.add_measured("mailbox", controller_mailbox);
    report.add_measured("pool", db_pool);
    report.add_measured("pool", outbound_pool);
    report.add_measured("listener", main_listener);
    // The sqlite bridge measures its own pressure but the bridge is
    // sampled live; at startup the count cap is the only fact we own
    // here. The other live counters are reported via `/debug/capacity`
    // and `terminal_line`. Name them so on-call sees "we plan to
    // measure this".
    report.add_unavailable(
        "db.bridge_in_flight",
        "bridge",
        "sampled live via SqliteMetricsHandle",
    );
    report.add_unavailable(
        "outbound.bridge_in_flight",
        "bridge",
        "sampled live via WorkerPool reports",
    );

    let topology_line =
        format!("topology service=mini_saas_api main_addr={main_addr} notify_addr={notify_addr}");
    let summary_line = format!("startup {} | {}", topology_line, report.summary_line());
    // Reuse `ServicePressureSurface::discovery_line()` instead of
    // duplicating the measured/unavailable format. The previous inline
    // copy used `reason:?` instead of the surface helper's quoting,
    // which would silently drift if the helper's escape rules
    // tightened. The smoke test asserts the resulting `state=unavailable`
    // / `reason="..."` shape.
    let mut discovery_lines = vec![topology_line];
    for surface in &report.surfaces {
        discovery_lines.push(surface.discovery_line());
    }

    // Build the typed `ServiceTopology` covering every started component
    // and the pressure surfaces. The legacy `summary_line` /
    // `discovery_lines` strings stay byte-identical for compatibility;
    // the typed report is the structured proof Phase 106 adds.
    let mut topology = ServiceTopology::new("mini_saas_api", Lifecycle::Ready);
    topology
        .push_component(
            TopologyComponent::new("main.listener", "listener", main_addr.to_string())
                .with_notes("public HTTP ingress"),
        )
        .push_component(
            TopologyComponent::new("notify.listener", "listener", notify_addr.to_string())
                .with_notes("notification HTTP service"),
        )
        .push_component(
            TopologyComponent::new("controller", "isolate", "")
                .with_notes("singleton controller + drain helper"),
        )
        .push_component(
            TopologyComponent::new("db.bridge", "bridge", "sqlite")
                .with_notes("tina-sqlite-bridge worker"),
        )
        .push_component(
            TopologyComponent::new("outbound.pool", "pool", notify_addr.to_string())
                .with_notes("keepalive pool to notify_addr"),
        );
    let topology = topology.with_pressure(report);

    StartupSummary {
        summary_line,
        discovery_lines,
        topology,
    }
}

#[cfg(test)]
mod conversion_tests {
    use super::*;
    use tina_http::{
        KeepaliveConnectionStopFailure, KeepaliveConnectionStopOutcome, KeepalivePoolCloseOutcome,
        KeepalivePoolShutdownReport,
    };

    #[test]
    fn pool_shutdown_clean_drain_becomes_clean_close_outcome() {
        let pool = KeepalivePoolShutdownReport {
            pool_close: KeepalivePoolCloseOutcome::Closed,
            drain: KeepalivePoolDrainOutcome::Drained,
            requested: 1,
            stopped: 1,
            timed_out: 0,
            rejected: 0,
            already_closed: 0,
            connection_failures: Vec::new(),
        };
        let report =
            pool_shutdown_to_close_report("outbound.pool", &pool, Duration::from_millis(2));
        assert_eq!(report.name, "outbound.pool");
        assert_eq!(report.kind, ResourceKind::Pool);
        assert_eq!(report.admission, CloseAdmission::Drain);
        assert!(matches!(report.outcome, CloseOutcome::Clean));
        assert!(report.details.contains("requested=1 stopped=1"));
    }

    #[test]
    fn pool_shutdown_timed_out_drain_propagates_as_timed_out() {
        // Production-shape stuck-child scenario: the keepalive pool's
        // drain deadline fired while leases remained. The conversion
        // must surface this as `CloseOutcome::TimedOut`, which the
        // choreography then records as `StepOutcome::Timeout`.
        let pool = KeepalivePoolShutdownReport {
            pool_close: KeepalivePoolCloseOutcome::Closed,
            drain: KeepalivePoolDrainOutcome::TimedOut { leased: 1 },
            requested: 0,
            stopped: 0,
            timed_out: 0,
            rejected: 0,
            already_closed: 0,
            connection_failures: Vec::new(),
        };
        let elapsed = Duration::from_millis(2_000);
        let report = pool_shutdown_to_close_report("outbound.pool", &pool, elapsed);
        match report.outcome {
            CloseOutcome::TimedOut { waited } => assert_eq!(waited, elapsed),
            other => panic!("expected TimedOut, got {other:?}"),
        }
        assert!(report.details.contains("drain=TimedOut"));
    }

    #[test]
    fn pool_shutdown_with_connection_failures_becomes_failed_close() {
        // The pool admission closed cleanly but one of the connection
        // isolates did not stop on request. The conversion must
        // surface this as `CloseOutcome::Failed` so the choreography
        // flags `clean=false` and the failure reason is searchable.
        let pool = KeepalivePoolShutdownReport {
            pool_close: KeepalivePoolCloseOutcome::Closed,
            drain: KeepalivePoolDrainOutcome::Drained,
            requested: 2,
            stopped: 1,
            timed_out: 0,
            rejected: 1,
            already_closed: 0,
            connection_failures: vec![KeepaliveConnectionStopFailure {
                index: 1,
                outcome: KeepaliveConnectionStopOutcome::UnexpectedReply,
            }],
        };
        let report =
            pool_shutdown_to_close_report("outbound.pool", &pool, Duration::from_millis(1));
        match &report.outcome {
            CloseOutcome::Failed { reason } => {
                assert!(reason.contains("failures=1"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn pool_shutdown_already_closed_becomes_already_closed() {
        let pool = KeepalivePoolShutdownReport {
            pool_close: KeepalivePoolCloseOutcome::AlreadyClosed,
            drain: KeepalivePoolDrainOutcome::PoolAlreadyClosed,
            requested: 0,
            stopped: 0,
            timed_out: 0,
            rejected: 0,
            already_closed: 0,
            connection_failures: Vec::new(),
        };
        let report =
            pool_shutdown_to_close_report("outbound.pool", &pool, Duration::from_millis(0));
        assert!(matches!(report.outcome, CloseOutcome::AlreadyClosed));
    }
}
