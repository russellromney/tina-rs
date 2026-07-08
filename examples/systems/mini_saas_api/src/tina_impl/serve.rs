use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use http::StatusCode;
use tina::pool::{CloseMode, PoolConfig};
use tina::prelude::*;
use tina_http::{
    BodyMetrics, HttpClientConfig, HttpListener, HttpListenerMsg, HttpReady, HttpResponse,
    HttpStartupError, HttpTarget, KeepalivePoolDrainOutcome, KeepalivePoolHandles,
    build_keepalive_pool, shutdown_keepalive_pool,
};
use tina_runtime::lifecycle::{
    CloseAdmission, ResourceCloseReport, ResourceKind, ServiceShutdownReport, ShutdownChoreography,
    ShutdownStep, StepOutcome,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, SignalWaitReply, ThreadedRuntime, signal_wait,
};
use tina_sqlite_bridge::{InstalledSqliteBridge, SqliteConfig, SqliteWorker};

use super::controller::{Controller, ControllerMsg, NotifyMsg, NotifySink};
use super::harness::wait_for_capacity;
use super::shutdown::pool_shutdown_to_close_report;
use super::{
    REQUEST_TIMEOUT, ScopeSetMetrics, StartupSummary, build_startup_summary, listener_config,
    response_body_text, seed_db,
};

const SERVE_SHARD_COUNT: usize = 1;
/// Idle window for one `signal_wait` arm. A real SIGINT/SIGTERM fires
/// immediately; the watcher re-arms on timeout so the wait is effectively
/// permanent for the life of the service.
const SIGNAL_REARM_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// Upper bound on how long the graceful drain waits for already-admitted
/// in-flight requests to finish on their own before force-cancelling the
/// stragglers. Real requests complete well inside this; the bound only
/// caps a wedged request.
const SERVE_DRAIN_DEADLINE: Duration = Duration::from_secs(5);

type ServiceRuntime = ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>;
type ControllerAddr = Address<ControllerMsg, HttpResponse>;
type ListenerAddr = Address<HttpListenerMsg, Result<HttpReady, HttpStartupError>>;

/// One-shot shutdown switch shared between the in-runtime [`SignalWatcher`]
/// and the blocking `serve` main thread. Holds the signal name that fired.
#[derive(Default)]
struct ShutdownTrip {
    state: Mutex<Option<&'static str>>,
    signalled: Condvar,
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

    /// Whether a signal has already tripped the switch.
    fn is_tripped(&self) -> bool {
        self.state
            .lock()
            .expect("shutdown trip mutex poisoned")
            .is_some()
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

/// Waits on SIGINT / SIGTERM through the runtime's `signal_wait` rail. The
/// first delivery trips [`ShutdownTrip`] to start the graceful drain; a
/// second delivery force-exits (double Ctrl-C). Re-arms on timeout and after
/// the first signal; a re-armed wait outstanding at teardown is cancelled
/// with the runtime.
struct SignalWatcher {
    trip: Arc<ShutdownTrip>,
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
                if self.trip.is_tripped() {
                    // Second signal while the first drain is already running on
                    // the main thread (double Ctrl-C): operators expect this to
                    // stop now, so force-exit rather than finish the bounded
                    // drain. 130 = 128 + SIGINT, the conventional interrupted
                    // exit code.
                    eprintln!("serve: second signal ({name}); forcing immediate exit");
                    std::process::exit(130);
                }
                self.trip.trip(name);
                // Re-arm this signal so a *repeat* of it (the first delivery of
                // the other signal is already parked) reaches the force-exit
                // branch above.
                signal_wait(name, SIGNAL_REARM_TIMEOUT)
                    .then(move |reply| SignalMsg::Received(name, reply))
            }
            // Timeout re-arm. `signal-hook` latches a pending signal in a
            // persistent `AtomicBool`, so a signal that arrives during the
            // gap between completion and re-arm is not lost: the flag stays
            // set and is delivered to the freshly re-armed waiter on the next
            // driver poll. A teardown-cancel re-arm is a no-op once the
            // runtime is stopping.
            SignalMsg::Received(name, Err(_)) => signal_wait(name, SIGNAL_REARM_TIMEOUT)
                .then(move |reply| SignalMsg::Received(name, reply)),
        }
    }
}

/// A fully assembled, bound service: the same controller + SQLite bridge +
/// notify service + outbound keepalive pool the scripted modes build, plus
/// the handles the graceful drain needs. Both [`serve`] and the in-flight
/// drain proof build one of these so they share the exact drain path.
pub(crate) struct ServiceInstance {
    runtime: ServiceRuntime,
    controller: ControllerAddr,
    sqlite: InstalledSqliteBridge<SingleShard>,
    outbound: KeepalivePoolHandles,
    notify_listener: ListenerAddr,
    main_listener: ListenerAddr,
    /// Live notify-scope counters. The drain waits on `in_use` reaching zero
    /// so in-flight notify requests complete and release their pool lease.
    scope_metrics: ScopeSetMetrics,
    pub(crate) addr: SocketAddr,
    pub(crate) startup: StartupSummary,
    /// Keeps the temp SQLite file alive for the service's lifetime.
    _dir: tempfile::TempDir,
}

/// Typed outcome of [`ServiceInstance::graceful_drain`].
pub(crate) struct DrainReport {
    pub(crate) terminal_line: String,
    pub(crate) shutdown_report: ServiceShutdownReport,
    pub(crate) shutdown_clean: bool,
    pub(crate) scopes_drain_line: String,
}

impl ServiceInstance {
    /// Assemble and bind the service on `addr` (use `127.0.0.1:0` for an
    /// ephemeral port). Validates the budget manifest first, then installs
    /// every resource from the manifest caps, exactly as [`super::run`] does.
    pub(crate) fn build(addr: SocketAddr) -> anyhow::Result<Self> {
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
                    scope_metrics.clone(),
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

        let startup = build_startup_summary(bound_addr, notify_addr);

        Ok(Self {
            runtime,
            controller,
            sqlite,
            outbound,
            notify_listener,
            main_listener,
            scope_metrics,
            addr: bound_addr,
            startup,
            _dir: dir,
        })
    }

    /// Drain the service and stop the runtime. Truly graceful: after closing
    /// ingress it waits (bounded by `drain_deadline`) for already-admitted
    /// in-flight notify requests to finish on their own — they return their
    /// real responses and release their outbound pool leases naturally. Only
    /// a request still parked at the deadline is force-cancelled by the scope
    /// sweep; that path leaves a stranded lease, so the pool drain reports
    /// `TimedOut` and `shutdown_clean` is `false`.
    pub(crate) fn graceful_drain(self, drain_deadline: Duration) -> anyhow::Result<DrainReport> {
        let mut choreo = ShutdownChoreography::new("mini_saas_api");

        let t_close = Instant::now();
        match self.runtime.call_blocking(
            self.controller,
            ControllerMsg::CloseIngress,
            REQUEST_TIMEOUT,
        )? {
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

        // Bounded drain-wait: let already-admitted work finish on its own
        // before tearing anything down. Poll, do not cancel — cancelling here
        // is what stranded the pool lease.
        //
        // Two in-flight shapes matter:
        //   * A notify request holds a cross-turn *resource* — an outbound
        //     keepalive lease returned only when its flow reaches `Released`.
        //     Its scope stays in the set for that whole window, so
        //     `notify.in_use` is the signal that it (and its lease) is done.
        //   * A plain GET/POST/`/ready` holds no cross-turn resource; it only
        //     has a SQLite query in flight. `db.leased` (0/1 on the serial
        //     worker) is the signal that query is still executing.
        // Waiting both to zero means the bridge is idle and no lease is out,
        // so the teardown below closes nothing out from under live work.
        //
        // Residual (bounded, clean): a request whose SQLite op is still queued
        // in the bridge mailbox — not yet `leased` — when the deadline fires
        // sheds with a typed `503 db_closed`, never a 500 or a silent drop.
        // The runtime shutdown reclaims any such pending call without leaking.
        let t_drain = Instant::now();
        let deadline = Instant::now() + drain_deadline;
        loop {
            let (_, in_use, _, _) = self.scope_metrics.snapshot();
            let db_leased = self.sqlite.metrics.pressure_report().leased;
            if in_use == 0 && db_leased == 0 {
                choreo.record(
                    ShutdownStep::DrainInFlight,
                    "drain_in_flight",
                    t_drain.elapsed(),
                    StepOutcome::Clean,
                );
                break;
            }
            if Instant::now() >= deadline {
                choreo.record(
                    ShutdownStep::DrainInFlight,
                    "drain_in_flight",
                    t_drain.elapsed(),
                    StepOutcome::Failed {
                        reason: format!(
                            "still in flight at deadline: notify_scopes={in_use} db_leased={db_leased}"
                        ),
                    },
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        // Owner-stop sweep: force-cancel any straggler still parked past the
        // deadline. After a clean drain-wait this finds the set empty.
        let scopes_drain_line = match self.runtime.call_blocking(
            self.controller,
            ControllerMsg::DrainScopes,
            REQUEST_TIMEOUT,
        )? {
            CallOutcome::Replied(response) => response_body_text(&response).trim().to_owned(),
            other => anyhow::bail!("drain scopes control call failed: {other:?}"),
        };

        let t_db = Instant::now();
        self.sqlite.closer.close();
        choreo.record_close(
            &ResourceCloseReport::clean(
                "db.bridge",
                ResourceKind::Bridge,
                CloseAdmission::Drain,
                t_db.elapsed(),
            ),
            "close_sqlite_bridge",
        );
        let db_pressure = self.sqlite.metrics.pressure_report();

        let t_pool = Instant::now();
        let outbound_shutdown = shutdown_keepalive_pool(
            &self.runtime,
            &self.outbound,
            CloseMode::Drain,
            Duration::from_secs(2),
        )
        .map_err(|e| anyhow::anyhow!("shutdown keepalive pool: {e:?}"))?;
        choreo.record_close(
            &pool_shutdown_to_close_report("outbound.pool", &outbound_shutdown, t_pool.elapsed()),
            "close_outbound_pool",
        );

        let t_notify_listener = Instant::now();
        self.runtime
            .try_send(self.notify_listener, HttpListenerMsg::Stop)
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
        self.runtime
            .try_send(self.main_listener, HttpListenerMsg::Stop)
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
        let trace = self
            .runtime
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
        let shutdown_clean = matches!(
            outbound_shutdown.drain,
            KeepalivePoolDrainOutcome::Drained
        ) && outbound_shutdown.requested == outbound_shutdown.stopped
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

        Ok(DrainReport {
            terminal_line,
            shutdown_report,
            shutdown_clean,
            scopes_drain_line,
        })
    }
}

/// Bind the service and run until SIGINT/SIGTERM, then drain gracefully and
/// return. The copyable run-forever entrypoint: it assembles the same
/// service the scripted modes build, reuses the health/readiness/capacity
/// routes and the shutdown choreography, and adds only the signal wait, the
/// blocking park, and a bounded drain-wait. Exits non-zero if the drain does
/// not complete cleanly so an orchestrator sees the unclean stop.
pub fn serve(addr: SocketAddr) -> anyhow::Result<()> {
    let instance = ServiceInstance::build(addr)?;

    // One startup line: bind addr + shard count + the same capacity summary
    // the scripted modes print.
    println!(
        "serve listening addr={} shards={SERVE_SHARD_COUNT} {}",
        instance.addr, instance.startup.summary_line,
    );

    // Arm the signal watcher, then park the main thread until a signal trips.
    let trip = Arc::new(ShutdownTrip::default());
    let watcher = instance
        .runtime
        .register_with_capacity::<_, Infallible>(
            SignalWatcher {
                trip: Arc::clone(&trip),
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register signal watcher: {e:?}"))?;
    instance
        .runtime
        .try_send(watcher, SignalMsg::Arm("sigint"))
        .map_err(|e| anyhow::anyhow!("arm sigint watcher: {e:?}"))?;
    instance
        .runtime
        .try_send(watcher, SignalMsg::Arm("sigterm"))
        .map_err(|e| anyhow::anyhow!("arm sigterm watcher: {e:?}"))?;

    let signal = trip.wait();
    println!("serve signal={signal}; draining");

    let report = instance.graceful_drain(SERVE_DRAIN_DEADLINE)?;
    println!("serve {}", report.scopes_drain_line);
    println!("{}", report.terminal_line);
    println!(
        "serve shutdown_clean={} {}",
        report.shutdown_clean,
        report.shutdown_report.summary_line(),
    );
    if !report.shutdown_clean {
        // Non-zero exit: a straggler outlived the drain deadline and its
        // resources did not close cleanly. The orchestrator should see this.
        anyhow::bail!(
            "graceful drain did not complete cleanly within {SERVE_DRAIN_DEADLINE:?}"
        );
    }
    Ok(())
}

/// Proof for the graceful drain: build the service, hold one notify request
/// mid-outbound, then run the same drain [`serve`] runs with `drain_deadline`.
/// With a deadline longer than the in-flight work the request completes with
/// its real response and the pool lease is released (clean); with a deadline
/// shorter than the work the straggler is force-cancelled and the lease is
/// stranded (unclean). This is the regression witness for the drain fix.
pub fn prove_graceful_drain_completes_in_flight(
    drain_deadline: Duration,
) -> anyhow::Result<crate::GracefulDrainProof> {
    let instance = ServiceInstance::build("127.0.0.1:0".parse()?)?;
    let addr = instance.addr;

    let created = crate::post(addr, "/items", "name=alpha")?;
    anyhow::ensure!(
        created.status == 201,
        "seed create expected 201, got {} ({})",
        created.status,
        created.body,
    );

    // Hold a notify mid-outbound: the sink's "slow" path defers ~250ms, so the
    // controller's outbound request call is parked and its scope is in flight.
    let slow_addr = addr;
    let slow = std::thread::spawn(move || crate::post(slow_addr, "/items/1/notify", "slow"));
    wait_for_capacity(addr, "outbound.in_flight=1", Duration::from_secs(2))?;

    // Drain while the notify is still in flight.
    let report = instance.graceful_drain(drain_deadline)?;

    let in_flight = slow
        .join()
        .map_err(|_| anyhow::anyhow!("in-flight notify thread panicked"))?;
    let (in_flight_status, in_flight_body) = match in_flight {
        Ok(parts) => (parts.status, parts.body.trim().to_owned()),
        Err(error) => (0, format!("transport error: {error}")),
    };

    Ok(crate::GracefulDrainProof {
        in_flight_notified: in_flight_status == 200 && in_flight_body.contains("notified"),
        in_flight_status,
        in_flight_body,
        shutdown_clean: report.shutdown_clean,
        terminal_line: report.terminal_line,
        scopes_drain_line: report.scopes_drain_line,
    })
}
