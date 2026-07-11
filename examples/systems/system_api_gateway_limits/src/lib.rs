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

use std::convert::Infallible;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, CapacitySummary, ConcurrencyGuardedInsertError, ConcurrencyPendingReplies,
    DefaultThreadedMailboxFactory, SharedCapacityReservation, SharedCapacityScope, SleepReply,
    SplitServiceHandle, ThreadedRuntime, format_assertion_failure, format_discovery_line, sleep,
};

#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub gateway_mailbox: usize,
    pub pending_capacity: usize,
    /// Shared in-flight-request budget (weighted by route).
    pub shared_cap: usize,
    /// Shared body-bytes budget across both routes.
    pub body_cap: usize,
    pub upload_weight: usize,
    pub list_weight: usize,
    /// Per-request body size charged against the body-bytes budget.
    pub upload_body: usize,
    pub list_body: usize,
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
            body_cap: 4_096,
            upload_weight: 2,
            list_weight: 1,
            upload_body: 1_024,
            list_body: 128,
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
    /// Body-bytes shared budget facts (second weighted dimension).
    pub body_high_water: usize,
    pub body_full_count: u64,
    pub body_admitted: u64,
    pub body_released: u64,
    pub body_current_at_drain: usize,
    pub discovery_lines: Vec<String>,
    pub summary_line: String,
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
pub enum GatewayEvent {
    HoldDone {
        qid: u64,
        route: Route,
        result: SleepReply,
    },
}

#[derive(Debug)]
pub enum GatewayRequest {
    Request { route: Route, hold: Duration },
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
    body_scope: SharedCapacityScope,
    upload_weight: usize,
    list_weight: usize,
    upload_body: usize,
    list_body: usize,
    pending: ConcurrencyPendingReplies<u64, GatewayReply, SharedCapacityReservation>,
    next_qid: u64,
}

#[tina_runtime::isolate(event = GatewayEvent, request = GatewayRequest, reply = GatewayReply)]
impl Gateway {
    fn handle_event(
        &mut self,
        event: GatewayEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            GatewayEvent::HoldDone { qid, route, result } => self.hold_done(qid, route, result),
        }
    }

    fn handle_request(
        &mut self,
        request: GatewayRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            GatewayRequest::Request { route, hold } => self.dispatch(route, hold, call),
        }
    }
}

impl Gateway {
    #[allow(clippy::too_many_arguments)]
    fn new(
        scope: SharedCapacityScope,
        body_scope: SharedCapacityScope,
        upload_weight: usize,
        list_weight: usize,
        upload_body: usize,
        list_body: usize,
        pending_capacity: usize,
    ) -> Self {
        Self {
            scope,
            body_scope,
            upload_weight,
            list_weight,
            upload_body,
            list_body,
            pending: ConcurrencyPendingReplies::with_capacity(
                "system_api_gateway_limits.pending",
                pending_capacity,
            ),
            next_qid: 1,
        }
    }

    fn weight_for(&self, route: Route) -> usize {
        match route {
            Route::Upload => self.upload_weight,
            Route::List => self.list_weight,
        }
    }

    fn body_for(&self, route: Route) -> usize {
        match route {
            Route::Upload => self.upload_body,
            Route::List => self.list_body,
        }
    }

    fn dispatch(
        &mut self,
        route: Route,
        hold: Duration,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        let weight = self.weight_for(route);
        let body_bytes = self.body_for(route);
        let charge = match SharedCapacityReservation::try_reserve([
            self.scope.charge(weight),
            self.body_scope.charge(body_bytes),
        ]) {
            Ok(charge) => charge,
            Err(full) => {
                return call.reply(GatewayReply::Full {
                    filled: full.scope,
                    requested: full.requested,
                    current: full.current,
                    max: full.max,
                });
            }
        };

        let qid = self.next_qid;
        call.capture(|request| {
            match self
                .pending
                .insert_deferred_guarded(qid, request.into_deferred(), charge)
            {
                Ok(_ticket) => {
                    self.next_qid = qid + 1;
                    sleep(hold).then(move |result| {
                        tina::ServiceMessage::Event(GatewayEvent::HoldDone { qid, route, result })
                    })
                }
                Err(ConcurrencyGuardedInsertError::Admission {
                    reply, failure, ..
                }) => {
                    let report = failure.report();
                    reply_to::<Self>(
                        reply,
                        GatewayReply::Full {
                            filled: "gateway.pending".into(),
                            requested: 1,
                            current: report.current,
                            max: report.capacity,
                        },
                    )
                }
                Err(ConcurrencyGuardedInsertError::PendingFull { reply, .. }) => {
                    let report = self.pending.report();
                    reply_to::<Self>(
                        reply,
                        GatewayReply::Full {
                            filled: "gateway.pending_mismatch".into(),
                            requested: 1,
                            current: report.parked,
                            max: report.admission.capacity,
                        },
                    )
                }
                Err(ConcurrencyGuardedInsertError::DuplicateKey { reply, .. }) => {
                    reply_to::<Self>(
                        reply,
                        GatewayReply::Full {
                            filled: "gateway.duplicate".into(),
                            requested: 1,
                            current: 0,
                            max: 0,
                        },
                    )
                }
            }
        })
    }

    fn hold_done(&mut self, qid: u64, route: Route, _result: SleepReply) -> Effect<Self> {
        let Some(effect) = self.pending.reply_by_key::<Self>(
            &qid,
            GatewayReply::Ok {
                route: route.label(),
            },
        ) else {
            return noop();
        };
        effect
    }
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let scope = SharedCapacityScope::new("gateway.in_flight", "weight", config.shared_cap);
    let body_scope = SharedCapacityScope::new("gateway.body_bytes", "bytes", config.body_cap);
    let gateway: SplitServiceHandle<GatewayEvent, GatewayRequest, GatewayReply> = runtime
        .register_split_service::<Gateway, GatewayEvent, GatewayRequest, Infallible>(
            Gateway::new(
                scope.clone(),
                body_scope.clone(),
                config.upload_weight,
                config.list_weight,
                config.upload_body,
                config.list_body,
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
        let addr = gateway.requests;
        threads.push(thread::spawn(move || {
            gate.wait();
            let r = rt.call_blocking_request(
                addr,
                GatewayRequest::Request {
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
        let addr = gateway.requests;
        threads.push(thread::spawn(move || {
            gate.wait();
            let r = rt.call_blocking_request(
                addr,
                GatewayRequest::Request {
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
    let body_pre = body_scope.snapshot();
    let scope_line = scope.discovery_line();
    let body_line = body_scope.discovery_line();
    let surface_line =
        format_discovery_line(&scope.surface_report(tina::capacity::CapacityMode::Fixed));
    let mut capacity_summary = CapacitySummary::new();
    capacity_summary
        .push(scope.surface_report(tina::capacity::CapacityMode::Fixed))
        .map_err(|e| anyhow::anyhow!("push surface: {e:?}"))?;
    capacity_summary
        .push(body_scope.surface_report(tina::capacity::CapacityMode::Fixed))
        .map_err(|e| anyhow::anyhow!("push body surface: {e:?}"))?;
    if let Err(errors) = capacity_summary.assert_no_full() {
        // Failing is expected for this specimen; surface the
        // copyable FAIL line for CI consumers but do not error.
        for err in &errors {
            eprintln!("{}", format_assertion_failure(err));
        }
    }

    // Owner stop must release every held charge. Shutdown drops the
    // gateway isolate, which drops every parked capacity reservation.
    // The post-shutdown snapshot is the load-bearing
    // proof: `current` must be 0 even if callers were still timing
    // out at shutdown time.
    shutdown(runtime);
    let snap = scope.snapshot();
    let body_snap = body_scope.snapshot();

    let summary_line = format!(
        "system=system_api_gateway_limits upload_admitted={} upload_full={} upload_timeout={} list_admitted={} list_full={} list_timeout={} scope_high_water={} scope_full_count={} scope_current_at_drain={} body_high_water={} body_full_count={} body_current_at_drain={}",
        upload_admitted,
        upload_full,
        upload_timeout,
        list_admitted,
        list_full,
        list_timeout,
        snap.high_water,
        snap.full_count,
        snap.current,
        body_snap.high_water,
        body_snap.full_count,
        body_snap.current,
    );

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
        body_high_water: body_pre.high_water,
        body_full_count: body_snap.full_count,
        body_admitted: body_snap.admitted,
        body_released: body_snap.released,
        body_current_at_drain: body_snap.current,
        discovery_lines: vec![scope_line, body_line, surface_line],
        summary_line,
    })
}

fn shutdown(runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>) {
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
