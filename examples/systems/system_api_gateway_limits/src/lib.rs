//! Tiny "API gateway" specimen that proves
//! [`SharedCapacityScope`](tina_runtime::SharedCapacityScope).
//!
//! Two routes ("upload", "list") share one shard-local in-flight cap.
//! Callers race; one route can drain the shared scope; the other
//! sees `Full { filled=gateway.in_flight, ... }` because the cap is
//! shared.
//!
//! The specimen returns its discovery lines and a one-line summary
//! so the smoke test can be copied into CI without modification.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use tina::{CallContext, prelude::*};
use tina_runtime::lifecycle::{
    Health, Lifecycle, Readiness, ReadinessReason, ServiceTopology, TopologyComponent,
};
use tina_runtime::service_pressure::ServicePressureBuilder;
use tina_runtime::service_report::{ServiceReplayStatus, ServiceReport, ServiceReportBuilder};
use tina_runtime::{
    CallOutcome, CapacitySummary, DefaultThreadedMailboxFactory, PendingReplies, ServiceHandle,
    SharedCapacityScope, SharedLease, SharedScopeFull, SleepReply, ThreadedRuntime,
    format_assertion_failure, format_discovery_line, sleep,
};

#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub gateway_mailbox: usize,
    pub pending_capacity: usize,
    pub shared_cap: usize,
    pub upload_weight: usize,
    pub list_weight: usize,
    pub upload_hold_ms: u64,
    pub list_hold_ms: u64,
    pub upload_callers: usize,
    pub list_callers: usize,
    pub call_timeout_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            gateway_mailbox: 64,
            pending_capacity: 32,
            shared_cap: 4,
            upload_weight: 2,
            list_weight: 1,
            upload_hold_ms: 80,
            list_hold_ms: 40,
            upload_callers: 4,
            list_callers: 6,
            call_timeout_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub upload_admitted: usize,
    pub upload_full: usize,
    pub upload_timeout: usize,
    pub list_admitted: usize,
    pub list_full: usize,
    pub list_timeout: usize,
    pub scope_high_water: usize,
    pub scope_full_count: u64,
    pub scope_admitted: u64,
    pub scope_released: u64,
    /// Scope `current` *after* the runtime has been shut down. Owner
    /// stop must release every charge by the time this is read, so
    /// any non-zero value here is a lease leak.
    pub scope_current_at_drain: usize,
    pub scope_high_water_at_drain: usize,
    pub discovery_lines: Vec<String>,
    pub summary_line: String,
    /// Phase 111 service product surface. Threads the gateway's shared
    /// capacity scope into a typed [`ServiceReport`] with lifecycle
    /// `Stopped` (the run is one-shot), explicit pressure surfaces, and
    /// an explicit `NotCaptured` replay status.
    pub service_report: ServiceReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Upload,
    List,
}

impl Route {
    fn label(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::List => "list",
        }
    }
}

#[derive(Debug)]
pub enum GatewayMsg {
    Request {
        route: Route,
        hold: Duration,
    },
    HoldDone {
        qid: u64,
        route: Route,
        result: SleepReply,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayReply {
    Ok {
        route: &'static str,
    },
    Full {
        filled: String,
        requested: usize,
        current: usize,
        max: usize,
    },
}

struct Gateway {
    scope: SharedCapacityScope,
    upload_weight: usize,
    list_weight: usize,
    pending: PendingReplies<u64, GatewayReply>,
    leases: HashMap<u64, SharedLease>,
    next_qid: u64,
}

#[tina_runtime::isolate(message = GatewayMsg, reply = GatewayReply)]
impl Gateway {
    fn handle(
        &mut self,
        msg: GatewayMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            GatewayMsg::Request { .. } => noop(),
            GatewayMsg::HoldDone { qid, route, result } => self.hold_done(qid, route, result),
        }
    }

    fn handle_call(&mut self, msg: GatewayMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            GatewayMsg::Request { route, hold } => self.dispatch(route, hold, call),
            GatewayMsg::HoldDone { .. } => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

impl Gateway {
    fn new(
        scope: SharedCapacityScope,
        upload_weight: usize,
        list_weight: usize,
        pending_capacity: usize,
    ) -> Self {
        Self {
            scope,
            upload_weight,
            list_weight,
            pending: PendingReplies::with_capacity(pending_capacity)
                .named("system_api_gateway_limits.pending"),
            leases: HashMap::new(),
            next_qid: 1,
        }
    }

    fn weight_for(&self, route: Route) -> usize {
        match route {
            Route::Upload => self.upload_weight,
            Route::List => self.list_weight,
        }
    }

    fn dispatch(
        &mut self,
        route: Route,
        hold: Duration,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        let weight = self.weight_for(route);
        match self.scope.try_admit(weight) {
            Ok(lease) => {
                let qid = self.next_qid;
                self.next_qid += 1;
                let slot = call.into_request_context().into_deferred();
                if let Err(error) = self.pending.try_insert(qid, slot) {
                    // PendingReplies refused. Release the lease so
                    // the scope does not leak, and reply Full pointing
                    // at pending (not the shared scope).
                    drop(lease);
                    return match error {
                        tina_runtime::PendingRepliesInsertError::Full(_, slot) => reply_to(
                            slot,
                            GatewayReply::Full {
                                filled: "gateway.pending".into(),
                                requested: 1,
                                current: self.pending.capacity(),
                                max: self.pending.capacity(),
                            },
                        ),
                        tina_runtime::PendingRepliesInsertError::DuplicateKey(_, slot) => reply_to(
                            slot,
                            GatewayReply::Full {
                                filled: "gateway.duplicate".into(),
                                requested: 1,
                                current: 0,
                                max: 0,
                            },
                        ),
                    };
                }
                self.leases.insert(qid, lease);
                sleep(hold).then(move |result| GatewayMsg::HoldDone { qid, route, result })
            }
            Err(SharedScopeFull {
                scope,
                requested,
                current,
                max,
            }) => call.reply(GatewayReply::Full {
                filled: scope,
                requested,
                current,
                max,
            }),
        }
    }

    fn hold_done(&mut self, qid: u64, route: Route, _result: SleepReply) -> Effect<Self> {
        // Drop the lease first so the scope reflects "free" before
        // the caller wakes up. The lease may be missing if the
        // request was somehow cancelled — release returns silently.
        self.leases.remove(&qid);
        let Some(slot) = self.pending.take(&qid) else {
            return noop();
        };
        reply_to(
            slot,
            GatewayReply::Ok {
                route: route.label(),
            },
        )
    }
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let scope = SharedCapacityScope::new("gateway.in_flight", "weight", config.shared_cap);
    let gateway: ServiceHandle<GatewayMsg, GatewayReply> = runtime
        .register_service::<_, Infallible>(
            Gateway::new(
                scope.clone(),
                config.upload_weight,
                config.list_weight,
                config.pending_capacity,
            ),
            config.gateway_mailbox,
        )
        .map_err(|e| anyhow::anyhow!("register gateway: {e:?}"))?;

    let timeout = Duration::from_millis(config.call_timeout_ms);
    let outcomes = Arc::new(Mutex::new(Vec::with_capacity(
        config.upload_callers + config.list_callers,
    )));
    let barrier = Arc::new(Barrier::new(
        config.upload_callers + config.list_callers + 1,
    ));
    let mut threads = Vec::new();

    for _ in 0..config.upload_callers {
        let rt = Arc::clone(&runtime);
        let gate = Arc::clone(&barrier);
        let out = Arc::clone(&outcomes);
        let hold = Duration::from_millis(config.upload_hold_ms);
        let addr = gateway.call;
        threads.push(thread::spawn(move || {
            gate.wait();
            let r = rt.call_blocking_typed(
                addr,
                GatewayMsg::Request {
                    route: Route::Upload,
                    hold,
                },
                timeout,
            );
            out.lock().expect("outcomes").push(("upload", r));
        }));
    }
    for _ in 0..config.list_callers {
        let rt = Arc::clone(&runtime);
        let gate = Arc::clone(&barrier);
        let out = Arc::clone(&outcomes);
        let hold = Duration::from_millis(config.list_hold_ms);
        let addr = gateway.call;
        threads.push(thread::spawn(move || {
            gate.wait();
            let r = rt.call_blocking_typed(
                addr,
                GatewayMsg::Request {
                    route: Route::List,
                    hold,
                },
                timeout,
            );
            out.lock().expect("outcomes").push(("list", r));
        }));
    }
    barrier.wait();
    for t in threads {
        t.join().expect("caller thread panicked");
    }

    let mut upload_admitted = 0usize;
    let mut upload_full = 0usize;
    let mut upload_timeout = 0usize;
    let mut list_admitted = 0usize;
    let mut list_full = 0usize;
    let mut list_timeout = 0usize;
    for (route, outcome) in outcomes.lock().expect("outcomes").iter() {
        match (route, outcome) {
            (&"upload", Ok(CallOutcome::Replied(GatewayReply::Ok { .. }))) => upload_admitted += 1,
            (&"upload", Ok(CallOutcome::Replied(GatewayReply::Full { .. }))) => upload_full += 1,
            (&"upload", Ok(CallOutcome::Timeout)) => upload_timeout += 1,
            (&"list", Ok(CallOutcome::Replied(GatewayReply::Ok { .. }))) => list_admitted += 1,
            (&"list", Ok(CallOutcome::Replied(GatewayReply::Full { .. }))) => list_full += 1,
            (&"list", Ok(CallOutcome::Timeout)) => list_timeout += 1,
            other => anyhow::bail!("unexpected outcome: {other:?}"),
        }
    }

    // Pre-shutdown snapshot for discovery output / capacity summary.
    let snap_pre = scope.snapshot();
    let scope_line = scope.discovery_line();
    let surface_line =
        format_discovery_line(&scope.surface_report(tina::capacity::CapacityMode::Fixed));
    let mut capacity_summary = CapacitySummary::new();
    capacity_summary
        .push(scope.surface_report(tina::capacity::CapacityMode::Fixed))
        .map_err(|e| anyhow::anyhow!("push surface: {e:?}"))?;
    if let Err(errors) = capacity_summary.assert_no_full() {
        // Failing is expected for this specimen; surface the
        // copyable FAIL line for CI consumers but do not error.
        for err in &errors {
            eprintln!("{}", format_assertion_failure(err));
        }
    }

    // Owner stop must release every held charge. Shutdown drops the
    // gateway isolate, which drops its `HashMap<qid, SharedLease>`,
    // which releases. The post-shutdown snapshot is the load-bearing
    // proof: `current` must be 0 even if callers were still timing
    // out at shutdown time.
    shutdown(runtime);
    let snap = scope.snapshot();

    let summary_line = format!(
        "system=system_api_gateway_limits upload_admitted={} upload_full={} upload_timeout={} list_admitted={} list_full={} list_timeout={} scope_high_water={} scope_full_count={} scope_current_at_drain={}",
        upload_admitted,
        upload_full,
        upload_timeout,
        list_admitted,
        list_full,
        list_timeout,
        snap.high_water,
        snap.full_count,
        snap.current,
    );

    // Phase 111 service product surface.
    let pressure = ServicePressureBuilder::new("system_api_gateway_limits")
        .expect("validated service name")
        .surface(
            "scope",
            scope.surface_report(tina::capacity::CapacityMode::Fixed),
        )
        .expect("scope surface")
        .unavailable(
            "gateway.outbound.pool",
            "pool",
            "no outbound pool in this gateway specimen",
        )
        .expect("unavailable surface")
        .finish()
        .expect("builder finish");
    let mut topology = ServiceTopology::new("system_api_gateway_limits", Lifecycle::Stopped);
    topology
        .push_component(TopologyComponent::new(
            "gateway",
            "isolate",
            "",
        ))
        .push_component(TopologyComponent::new(
            "gateway.scope",
            "scope",
            "shard-local",
        ));
    let health = Health::new("system_api_gateway_limits", Lifecycle::Stopped)
        .with_pressure(pressure.clone());
    let service_report = ServiceReportBuilder::new("system_api_gateway_limits")
        .expect("validated service name")
        .lifecycle(Lifecycle::Stopped)
        .readiness(Readiness::not_ready(
            Lifecycle::Stopped,
            vec![ReadinessReason::IngressStopped],
        ))
        .health(health)
        .topology(topology)
        .pressure(pressure)
        .replay(ServiceReplayStatus::not_captured(
            "gateway specimen does not enable live capture",
        ))
        .finish()
        .expect("service report builder finish");

    Ok(RunReport {
        upload_admitted,
        upload_full,
        upload_timeout,
        list_admitted,
        list_full,
        list_timeout,
        scope_high_water: snap_pre.high_water,
        scope_full_count: snap.full_count,
        scope_admitted: snap.admitted,
        scope_released: snap.released,
        scope_current_at_drain: snap.current,
        scope_high_water_at_drain: snap.high_water,
        discovery_lines: vec![scope_line, surface_line],
        summary_line,
        service_report,
    })
}

fn shutdown(runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>) {
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
