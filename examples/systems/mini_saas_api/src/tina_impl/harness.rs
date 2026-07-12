use std::convert::Infallible;
use std::time::{Duration, Instant};

use http::StatusCode;
use tina::capacity::{CapacityMode, CapacitySurfaceReport};
use tina::pool::{CloseMode, PoolConfig};
use tina::prelude::*;
use tina_http::{
    BodyMetrics, BodyPressureReport, HttpClientConfig, HttpListener, HttpListenerMsg, HttpTarget,
    KeepalivePoolDrainOutcome, build_keepalive_pool, shutdown_keepalive_pool,
};
use tina_runtime::lifecycle::{
    CloseAdmission, Health, Lifecycle, ResourceCloseReport, ResourceKind, ShutdownChoreography,
    ShutdownStep, StepOutcome,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeEvent, RuntimeEventKind, ThreadedRuntime,
    ThreadedRuntimeConfig,
};
use tina_sim::dst::{
    LiveReplayCapture, LiveReplayFact, LiveReplayReport, ReplayCase as DstReplayCase, ReplayConfig,
    ReplayReport, check_captured_replay,
};
use tina_sqlite_bridge::{SqliteConfig, SqlitePressureReport, SqliteWorker};

use crate::budget::BODY_CAP_BYTES;
use crate::{RunMode, RunReport, UserObservation, get, post, put};

use super::controller::{Controller, ControllerMsg, NotifyMsg, NotifySink};
use super::shutdown::pool_shutdown_to_close_report;
use super::{
    REQUEST_TIMEOUT, ScopeSetMetrics, build_startup_summary, listener_config, response_body_text,
    seed_db,
};

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

    let runtime = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)?;
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
    let notify_bound = runtime.observe_next_bound()?;
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
    let scope_metrics = ScopeSetMetrics::with_capacity(caps.request_scope_set);
    let controller = runtime
        .register_with_capacity::<_, Infallible>(
            Controller::new(
                sqlite.address,
                sqlite.metrics.clone(),
                outbound.pool,
                public_body_metrics.clone(),
                caps.body,
                caps.controller_mailbox,
                caps.request_scope_set,
                caps.request_scope_child_cap,
                scope_metrics.clone(),
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
    let main_bound = runtime.observe_next_bound()?;
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

    // Owner-stop scope sweep. The in-flight notify above has drained to
    // completion, so its scope was retired; this sweep finds the set empty
    // and reports zero unreleased capacity. A scope still active here would
    // have its pending child rail cancelled (proven by
    // `prove_drain_cancels_active_scope`).
    report.scopes_drain_line =
        match runtime.call_blocking(controller, ControllerMsg::DrainScopes, REQUEST_TIMEOUT)? {
            CallOutcome::Replied(response) => response_body_text(&response).trim().to_owned(),
            other => anyhow::bail!("drain scopes control call failed: {other:?}"),
        };
    report.scopes_drain_unreleased_zero = report.scopes_drain_line.contains("unreleased=0");

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
    let terminal = runtime.shutdown_report();
    terminal.ensure_clean()?;
    choreo.record(
        ShutdownStep::StopOwner,
        "shutdown_runtime",
        t_runtime.elapsed(),
        StepOutcome::Clean,
    );
    let pressure = tina_runtime::pressure::PressureSummary::from_events(terminal.trace());
    let deferred_replies = terminal
        .trace()
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
        &scope_metrics,
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

/// Owner-stop sweep proof: a notify request is held mid-outbound so its
/// scope has a still-pending child rail, then the scope set is drained.
/// The drain cancels that child (`OwnerStopped`); the stranded caller is
/// answered with an error, never `notified`. This is the non-zero
/// counterpart to the zero-unreleased sweep in [`run`].
pub fn prove_drain_cancels_active_scope() -> anyhow::Result<crate::DrainActiveReport> {
    let manifest = crate::budget::manifest();
    manifest
        .validate()
        .map_err(|errors| anyhow::anyhow!("invalid budget manifest: {errors:?}"))?;
    let caps = crate::budget::ServiceCaps::from_manifest(&manifest)
        .map_err(|missing| anyhow::anyhow!("manifest missing install caps: {missing:?}"))?;

    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("mini-saas.sqlite");
    seed_db(&db_path)?;

    let runtime = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)?;
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
    let notify_bound = runtime.observe_next_bound()?;
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

    let scope_metrics = ScopeSetMetrics::with_capacity(caps.request_scope_set);
    let controller = runtime
        .register_with_capacity::<_, Infallible>(
            Controller::new(
                sqlite.address,
                sqlite.metrics.clone(),
                outbound.pool,
                BodyMetrics::default(),
                caps.body,
                caps.controller_mailbox,
                caps.request_scope_set,
                caps.request_scope_child_cap,
                scope_metrics,
            ),
            caps.controller_mailbox,
        )
        .map_err(|e| anyhow::anyhow!("register controller: {e:?}"))?;
    let mut main_listener_config = listener_config(caps.body);
    main_listener_config.listener_mailbox_capacity = caps.main_listener_mailbox;
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
    let main_bound = runtime.observe_next_bound()?;
    runtime
        .try_send(main_listener, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start main listener: {e:?}"))?;
    let addr = main_bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("bind main listener: {e:?}"))?;

    // Seed one item so the notify path reaches the outbound call.
    let created = crate::post(addr, "/items", "name=alpha")?;
    anyhow::ensure!(
        created.status == 201,
        "seed create expected 201, got {} ({})",
        created.status,
        created.body,
    );

    // Hold a notify mid-outbound: the sink's "slow" path defers ~250ms, so
    // the controller's outbound request call is parked and its scope has a
    // pending child for that window.
    let slow_addr = addr;
    let slow = std::thread::spawn(move || crate::post(slow_addr, "/items/1/notify", "slow"));
    wait_for_capacity(addr, "outbound.in_flight=1", Duration::from_secs(2))?;

    // Sweep while the child is pending.
    let drain_line =
        match runtime.call_blocking(controller, ControllerMsg::DrainScopes, REQUEST_TIMEOUT)? {
            CallOutcome::Replied(response) => response_body_text(&response).trim().to_owned(),
            other => anyhow::bail!("drain scopes control call failed: {other:?}"),
        };
    let scopes_cancelled = parse_drain_field(&drain_line, "scopes_cancelled").unwrap_or(0);
    let children_cancelled = parse_drain_field(&drain_line, "children_cancelled").unwrap_or(0);
    let unreleased = parse_drain_field(&drain_line, "unreleased").unwrap_or(usize::MAX);

    // Tear down; the stranded slow notify gets an error/non-200, never
    // `notified`, because its outbound wait was closed by the sweep.
    runtime
        .try_send(notify_listener, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("stop notify listener: {e:?}"))?;
    runtime
        .try_send(main_listener, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("stop main listener: {e:?}"))?;
    sqlite.closer.close();
    let outbound_shutdown = shutdown_keepalive_pool(
        &runtime,
        &outbound,
        CloseMode::Force,
        Duration::from_secs(2),
    )
    .map_err(|error| anyhow::anyhow!("shutdown keepalive pool: {error:?}"));
    let runtime_shutdown = runtime.shutdown_report().ensure_clean();

    let slow_aborted = match slow.join() {
        Ok(Ok(parts)) => !(parts.status == 200 && parts.body.contains("notified")),
        Ok(Err(_)) | Err(_) => true,
    };
    outbound_shutdown?;
    runtime_shutdown?;

    Ok(crate::DrainActiveReport {
        scopes_cancelled,
        children_cancelled,
        unreleased,
        slow_notify_aborted: slow_aborted,
        drain_line,
    })
}

fn parse_drain_field(line: &str, key: &str) -> Option<usize> {
    line.split_whitespace()
        .find_map(|field| {
            field
                .strip_prefix(key)
                .and_then(|rest| rest.strip_prefix('='))
        })
        .and_then(|value| value.parse().ok())
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

pub(crate) fn wait_for_capacity(
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
pub fn run_soak(config: crate::SoakConfig) -> anyhow::Result<crate::SoakReport> {
    use tina_proof_harness::load::{self, LoadObservation, LoadRun, LoadStop, OpOutcome};

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
    let runtime = ThreadedRuntime::try_with_config_and_trace_observer(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
        live_trace.observer(),
    )?;
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
    let notify_bound = runtime.observe_next_bound()?;
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
    let scope_metrics = ScopeSetMetrics::with_capacity(caps.request_scope_set);
    let controller = runtime
        .register_with_capacity::<_, Infallible>(
            Controller::new(
                sqlite.address,
                sqlite.metrics.clone(),
                outbound.pool,
                public_body_metrics.clone(),
                caps.body,
                caps.controller_mailbox,
                caps.request_scope_set,
                caps.request_scope_child_cap,
                scope_metrics.clone(),
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
    let main_bound = runtime.observe_next_bound()?;
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
    // The third lane proves the bridge/pool path: without it the keepalive
    // outbound pool is never exercised and the soak only proves HTTP+DB.
    let op_addr = addr;
    let timeout = config.connect_timeout;
    let observe_addr = addr;
    let observe_trace = live_trace.clone();
    let load_report = load::run_with_observation(
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
        Some(move || match crate::get(observe_addr, "/debug/capacity") {
            Ok(capacity) => {
                let mut observation = load_observation_from_capacity_body(capacity.body.trim_end());
                let pressure = observe_trace.pressure_summary();
                observation.late = pressure.reply_rejected_no_pending_call;
                observation
            }
            Err(_) => LoadObservation {
                leak_clean: false,
                unavailable_surfaces: vec![tina_proof_harness::UnavailableSurface {
                    name: "mini_saas_api.capacity".to_string(),
                    kind: "http_probe",
                    reason: "GET /debug/capacity failed after load".to_string(),
                }],
                ..LoadObservation::default()
            },
        }),
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
    let terminal = runtime.shutdown_report();
    terminal.ensure_clean()?;
    let pressure = tina_runtime::pressure::PressureSummary::from_events(terminal.trace());
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

fn load_observation_from_capacity_body(body: &str) -> tina_proof_harness::LoadObservation {
    use tina_proof_harness::{LoadObservation, SurfacePlateau};

    let http_cap = parse_usize_field(body, "http.body_cap");
    let http_current = parse_usize_field(body, "http.request_body_current").unwrap_or(0);
    let http_high = parse_usize_field(body, "http.request_body_high_water").unwrap_or(0);
    let http_full = parse_u64_field(body, "http.body_full").unwrap_or(0);

    let db_cap = parse_usize_field(body, "db.capacity");
    let db_current = parse_usize_field(body, "db.in_flight").unwrap_or(0);
    let db_high = parse_usize_field(body, "db.high_water").unwrap_or(0);
    let db_full = parse_u64_field(body, "db.full").unwrap_or(0);

    let outbound_cap = parse_usize_field(body, "outbound.max_waiters");
    let outbound_current = parse_usize_field(body, "outbound.waiters").unwrap_or(0);
    let outbound_high = parse_usize_field(body, "outbound.high_water_waiters").unwrap_or(0);
    let outbound_full = parse_u64_field(body, "outbound.full").unwrap_or(0);

    let surfaces = vec![
        SurfacePlateau {
            name: "http.request_body".to_string(),
            kind: "body_bytes",
            capacity: http_cap,
            high_water: http_high,
            final_current: http_current,
            full: http_full,
            max_messages: None,
            current_messages: 0,
            high_water_messages: 0,
            max_weight: http_cap,
            current_weight: Some(http_current),
            high_water_weight: Some(http_high),
            shared_max_weight: None,
            shared_current_weight: None,
            shared_high_water_weight: None,
            leak_clean: http_current == 0,
        },
        SurfacePlateau {
            name: "db.in_flight".to_string(),
            kind: "bridge",
            capacity: db_cap,
            high_water: db_high,
            final_current: db_current,
            full: db_full,
            max_messages: db_cap,
            current_messages: db_current,
            high_water_messages: db_high,
            max_weight: None,
            current_weight: None,
            high_water_weight: None,
            shared_max_weight: None,
            shared_current_weight: None,
            shared_high_water_weight: None,
            leak_clean: db_current == 0,
        },
        SurfacePlateau {
            name: "outbound.pool_waiters".to_string(),
            kind: "pool_waiters",
            capacity: outbound_cap,
            high_water: outbound_high,
            final_current: outbound_current,
            full: outbound_full,
            max_messages: outbound_cap,
            current_messages: outbound_current,
            high_water_messages: outbound_high,
            max_weight: None,
            current_weight: None,
            high_water_weight: None,
            shared_max_weight: None,
            shared_current_weight: None,
            shared_high_water_weight: None,
            leak_clean: outbound_current == 0,
        },
    ];

    LoadObservation {
        leak_checked: true,
        leak_clean: surfaces.iter().all(|surface| surface.leak_clean),
        surface_plateaus: surfaces,
        unavailable_surfaces: Vec::new(),
        trace_hash: None,
        late: 0,
    }
}

fn parse_usize_field(line: &str, key: &str) -> Option<usize> {
    parse_field(line, key).and_then(|value| value.parse().ok())
}

fn parse_u64_field(line: &str, key: &str) -> Option<u64> {
    parse_field(line, key).and_then(|value| value.parse().ok())
}

fn parse_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&prefix))
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
    scope_metrics: &ScopeSetMetrics,
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
    // Request-scope set joined with live counters: cap from the manifest,
    // in-use / high-water / full from the controller's shared metrics.
    let (scope_cap, scope_in_use, scope_high_water, scope_full) = scope_metrics.snapshot();
    report.add_measured(
        "request_scope",
        CapacitySurfaceReport::count(
            "request.scope_set",
            CapacityMode::Fixed,
            scope_cap,
            scope_in_use,
            scope_high_water,
            scope_full,
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
        (
            "request.scope_child_cap",
            "per-request structural child cap; not an aggregate live counter",
        ),
    ] {
        report.add_unavailable(name, "mailbox", reason);
    }
    report
}
