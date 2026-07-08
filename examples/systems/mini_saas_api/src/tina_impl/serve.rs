use std::convert::Infallible;
use std::time::{Duration, Instant};

use http::StatusCode;
use tina::pool::{
    CloseMode, PoolConfig,
};
use tina::prelude::*;
use tina_http::{
    BodyMetrics, HttpClientConfig, HttpListener, HttpListenerMsg, HttpTarget, KeepalivePoolDrainOutcome,
    build_keepalive_pool, shutdown_keepalive_pool,
};
use tina_runtime::lifecycle::{
    CloseAdmission,
    ResourceCloseReport, ResourceKind, ShutdownChoreography, ShutdownStep,
    StepOutcome,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, SignalWaitReply,
    ThreadedRuntime, signal_wait,
};
use tina_sqlite_bridge::{
    SqliteConfig, SqliteWorker,
};


use super::controller::{Controller, ControllerMsg, NotifyMsg, NotifySink};
use super::shutdown::pool_shutdown_to_close_report;
use super::{REQUEST_TIMEOUT, ScopeSetMetrics, build_startup_summary, listener_config, response_body_text, seed_db};

const SERVE_SHARD_COUNT: usize = 1;
/// Idle window for one `signal_wait` arm. A real SIGINT/SIGTERM fires
/// immediately; the watcher re-arms on timeout so the wait is effectively
/// permanent for the life of the service.
const SIGNAL_REARM_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// One-shot shutdown switch shared between the in-runtime [`SignalWatcher`]
/// and the blocking `serve` main thread. Holds the signal name that fired.
#[derive(Default)]
struct ShutdownTrip {
    state: std::sync::Mutex<Option<&'static str>>,
    signalled: std::sync::Condvar,
}

impl ShutdownTrip {
    /// Record the first signal and wake the parked main thread. Idempotent:
    /// a later signal does not overwrite the one that tripped first.
    fn trip(&self, signal: &'static str) {
        let mut state = self.state.lock().expect("shutdown trip mutex poisoned");
        if state.is_none() {
            *state = Some(signal);
        }
        self.signalled.notify_all();
    }

    /// Park until a signal trips the switch; returns the signal name.
    fn wait(&self) -> &'static str {
        let mut state = self.state.lock().expect("shutdown trip mutex poisoned");
        while state.is_none() {
            state = self
                .signalled
                .wait(state)
                .expect("shutdown trip mutex poisoned");
        }
        state.expect("state is Some once the wait loop exits")
    }
}

enum SignalMsg {
    /// Begin (or re-arm) the wait for one named OS signal.
    Arm(&'static str),
    Received(&'static str, SignalWaitReply),
}

/// Waits on SIGINT / SIGTERM through the runtime's `signal_wait` rail and
/// trips [`ShutdownTrip`] on the first delivery. Re-arms on timeout; a
/// re-armed wait outstanding at teardown is cancelled with the runtime.
struct SignalWatcher {
    trip: std::sync::Arc<ShutdownTrip>,
}

#[tina_runtime::isolate(message = SignalMsg)]
impl SignalWatcher {
    fn handle(
        &mut self,
        msg: SignalMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SignalMsg::Arm(name) => signal_wait(name, SIGNAL_REARM_TIMEOUT)
                .then(move |reply| SignalMsg::Received(name, reply)),
            SignalMsg::Received(name, Ok(_)) => {
                self.trip.trip(name);
                noop()
            }
            // Timeout or teardown cancel: re-arm so a slow-arriving signal is
            // still caught. The re-arm is a no-op once the runtime is stopping.
            SignalMsg::Received(name, Err(_)) => signal_wait(name, SIGNAL_REARM_TIMEOUT)
                .then(move |reply| SignalMsg::Received(name, reply)),
        }
    }
}

/// Bind the service and run until SIGINT/SIGTERM, then drain gracefully and
/// return. This is the copyable run-forever entrypoint: it assembles the
/// same controller + SQLite bridge + notify service + outbound keepalive
/// pool as [`run`], reuses the health/readiness/capacity routes and the
/// shutdown choreography, and adds only the signal wait and the blocking
/// park. No scripted traffic, no assertions.
pub fn serve(addr: std::net::SocketAddr) -> anyhow::Result<()> {
    // Validate the budget manifest and read install caps back from it,
    // exactly as `run` does — a bad cap fails here, not at first traffic.
    let manifest = crate::budget::manifest();
    manifest
        .validate()
        .map_err(|errors| anyhow::anyhow!("invalid budget manifest: {errors:?}"))?;
    let caps = crate::budget::ServiceCaps::from_manifest(&manifest)
        .map_err(|missing| anyhow::anyhow!("manifest missing install caps: {missing:?}"))?;

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
                addr,
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
    let bound_addr = main_bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("bind main listener: {e:?}"))?;

    // One startup line: bind addr + shard count + the same capacity summary
    // the scripted modes print. Reuses `build_startup_summary` so the
    // topology/pressure shape is identical to the harness output.
    let startup = build_startup_summary(bound_addr, notify_addr);
    println!(
        "serve listening addr={bound_addr} shards={SERVE_SHARD_COUNT} {}",
        startup.summary_line,
    );

    // Arm the signal watcher, then park the main thread until a signal trips.
    let trip = std::sync::Arc::new(ShutdownTrip::default());
    let watcher = runtime
        .register_with_capacity::<_, Infallible>(
            SignalWatcher {
                trip: std::sync::Arc::clone(&trip),
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register signal watcher: {e:?}"))?;
    runtime
        .try_send(watcher, SignalMsg::Arm("sigint"))
        .map_err(|e| anyhow::anyhow!("arm sigint watcher: {e:?}"))?;
    runtime
        .try_send(watcher, SignalMsg::Arm("sigterm"))
        .map_err(|e| anyhow::anyhow!("arm sigterm watcher: {e:?}"))?;

    let signal = trip.wait();
    println!("serve signal={signal}; draining");

    // Graceful drain: same choreography as `run`, minus the scripted probes.
    let mut choreo = ShutdownChoreography::new("mini_saas_api");

    let t_close = Instant::now();
    match runtime.call_blocking(controller, ControllerMsg::CloseIngress, REQUEST_TIMEOUT)? {
        CallOutcome::Replied(response) if response.status == StatusCode::OK => {
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

    // Owner-stop sweep: cancel any still-parked notify child rails.
    let scopes_drain_line =
        match runtime.call_blocking(controller, ControllerMsg::DrainScopes, REQUEST_TIMEOUT)? {
            CallOutcome::Replied(response) => response_body_text(&response).trim().to_owned(),
            other => anyhow::bail!("drain scopes control call failed: {other:?}"),
        };
    println!("serve {scopes_drain_line}");

    let t_db = Instant::now();
    sqlite.closer.close();
    choreo.record_close(
        &ResourceCloseReport::clean(
            "db.bridge",
            ResourceKind::Bridge,
            CloseAdmission::Drain,
            t_db.elapsed(),
        ),
        "close_sqlite_bridge",
    );
    let db_pressure = sqlite.metrics.pressure_report();

    let t_pool = Instant::now();
    let outbound_shutdown = shutdown_keepalive_pool(
        &runtime,
        &outbound,
        CloseMode::Drain,
        Duration::from_secs(2),
    )
    .map_err(|e| anyhow::anyhow!("shutdown keepalive pool: {e:?}"))?;
    choreo.record_close(
        &pool_shutdown_to_close_report("outbound.pool", &outbound_shutdown, t_pool.elapsed()),
        "close_outbound_pool",
    );

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

    let shutdown_report = choreo.finish();
    let clean = matches!(outbound_shutdown.drain, KeepalivePoolDrainOutcome::Drained)
        && outbound_shutdown.requested == outbound_shutdown.stopped
        && outbound_shutdown.timed_out == 0
        && outbound_shutdown.rejected == 0
        && outbound_shutdown.already_closed == 0
        && outbound_shutdown.connection_failures.is_empty()
        && shutdown_report.clean;

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
        pressure,
    );
    println!("{terminal_line}");
    println!(
        "serve shutdown_clean={clean} {}",
        shutdown_report.summary_line(),
    );
    Ok(())
}
