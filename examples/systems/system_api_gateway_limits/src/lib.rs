//! Tiny "API gateway" specimen that proves
//! [`SharedCapacityScope`].
//!
//! Two routes ("upload", "list") share one shard-local in-flight cap.
//! Callers race; one route can drain the shared scope; the other
//! sees `Full { filled=gateway.in_flight, ... }` because the cap is
//! shared.
//!
//! The specimen returns its discovery lines and a one-line summary
//! so the smoke test can be copied into CI without modification.

use std::convert::Infallible;
use std::fmt;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, CapacityNameError, CapacitySummary, ConcurrencyGuardedInsertError,
    ConcurrencyPendingReplies, DefaultThreadedMailboxFactory, LocalSystem, ReportedWorkloadError,
    RunToShutdownError, SharedCapacityReservation, SharedCapacityScope, SharedScopeReport,
    SleepReply, SplitServiceHandle, StartupError, ThreadedRuntimeError, format_assertion_failure,
    format_discovery_line, sleep,
};

const MAX_CALLERS: usize = 256;
const MAX_MAILBOX_CAPACITY: usize = 65_536;
const MAX_PENDING_CAPACITY: usize = 65_536;
const MAX_SHARED_CAPACITY: usize = 1_000_000_000;
const MAX_REQUEST_CHARGE: usize = 100_000_000;
const MAX_HOLD_MS: u64 = 60_000;
const MAX_CALL_TIMEOUT_MS: u64 = 60_000;

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

impl RunConfig {
    /// Validates every value that can size an allocation, charge a scope, or
    /// bound a host wait before the runtime or caller threads are created.
    pub fn validate(self) -> Result<Self, RunConfigError> {
        let total_callers = self
            .upload_callers
            .checked_add(self.list_callers)
            .ok_or(RunConfigError::CallerCountOverflow)?;
        bounded("total callers", total_callers, MAX_CALLERS)?;
        bounded(
            "gateway mailbox capacity",
            self.gateway_mailbox,
            MAX_MAILBOX_CAPACITY,
        )?;
        nonzero_bounded(
            "pending capacity",
            self.pending_capacity,
            MAX_PENDING_CAPACITY,
        )?;
        nonzero_bounded("shared capacity", self.shared_cap, MAX_SHARED_CAPACITY)?;
        nonzero_bounded("body capacity", self.body_cap, MAX_SHARED_CAPACITY)?;
        nonzero_bounded("upload weight", self.upload_weight, MAX_REQUEST_CHARGE)?;
        nonzero_bounded("list weight", self.list_weight, MAX_REQUEST_CHARGE)?;
        nonzero_bounded("upload body", self.upload_body, MAX_REQUEST_CHARGE)?;
        nonzero_bounded("list body", self.list_body, MAX_REQUEST_CHARGE)?;
        nonzero_bounded_u64("upload hold", self.upload_hold_ms, MAX_HOLD_MS)?;
        nonzero_bounded_u64("list hold", self.list_hold_ms, MAX_HOLD_MS)?;
        nonzero_bounded_u64("call timeout", self.call_timeout_ms, MAX_CALL_TIMEOUT_MS)?;
        Ok(self)
    }
}

fn bounded(field: &'static str, value: usize, max: usize) -> Result<(), RunConfigError> {
    if value > max {
        return Err(RunConfigError::TooLarge { field, value, max });
    }
    Ok(())
}

fn nonzero_bounded(field: &'static str, value: usize, max: usize) -> Result<(), RunConfigError> {
    if value == 0 {
        return Err(RunConfigError::Zero { field });
    }
    bounded(field, value, max)
}

fn nonzero_bounded_u64(field: &'static str, value: u64, max: u64) -> Result<(), RunConfigError> {
    if value == 0 {
        return Err(RunConfigError::Zero { field });
    }
    if value > max {
        return Err(RunConfigError::DurationTooLarge { field, value, max });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunConfigError {
    Zero {
        field: &'static str,
    },
    TooLarge {
        field: &'static str,
        value: usize,
        max: usize,
    },
    DurationTooLarge {
        field: &'static str,
        value: u64,
        max: u64,
    },
    CallerCountOverflow,
}

impl fmt::Display for RunConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero { field } => write!(f, "{field} must be greater than zero"),
            Self::TooLarge { field, value, max } => {
                write!(f, "{field} {value} exceeds maximum {max}")
            }
            Self::DurationTooLarge { field, value, max } => {
                write!(f, "{field} {value}ms exceeds maximum {max}ms")
            }
            Self::CallerCountOverflow => f.write_str("total caller count overflowed usize"),
        }
    }
}

impl std::error::Error for RunConfigError {}

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
    /// Scope accounting after every concurrent caller returned but before the
    /// refill probe. With no timeouts this proves immediate rollback/release.
    pub scope_after_wave: SharedScopeReport,
    pub body_after_wave: SharedScopeReport,
    /// Exact application replies and timeouts observed for each caller.
    pub caller_outcomes: Vec<ObservedCallerOutcome>,
    /// A successful call after the concurrent wave proves capacity refills.
    pub refill_reply: Option<GatewayReply>,
    pub discovery_lines: Vec<String>,
    pub summary_line: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedCallerOutcome {
    pub route: Route,
    pub caller: usize,
    pub outcome: ObservedCallOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedCallOutcome {
    Replied(GatewayReply),
    Timeout,
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
pub enum GatewayWorkloadError {
    Registration(ThreadedRuntimeError),
    CallerPanicked {
        route: Route,
        caller: usize,
    },
    HostCall {
        route: Route,
        caller: usize,
        source: ThreadedRuntimeError,
    },
    MailboxFull {
        route: Route,
        caller: usize,
    },
    Closed {
        route: Route,
        caller: usize,
    },
    Rejected {
        route: Route,
        caller: usize,
        reason: tina::CallRejectedReason,
    },
    ReplyRouteMismatch {
        requested: Route,
        replied: &'static str,
    },
    RefillOutcome(CallOutcome<GatewayReply>),
    HoldFailed {
        route: Route,
        caller: usize,
        error: CallError,
    },
    CapacityName(CapacityNameError),
}

impl fmt::Display for GatewayWorkloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registration(error) => write!(f, "gateway registration failed: {error}"),
            Self::CallerPanicked { route, caller } => {
                write!(f, "{} caller {caller} panicked", route.label())
            }
            Self::HostCall {
                route,
                caller,
                source,
            } => write!(
                f,
                "{} caller {caller} host call failed: {source}",
                route.label()
            ),
            Self::MailboxFull { route, caller } => {
                write!(
                    f,
                    "{} caller {caller} found the gateway mailbox full",
                    route.label()
                )
            }
            Self::Closed { route, caller } => {
                write!(
                    f,
                    "{} caller {caller} found the gateway closed",
                    route.label()
                )
            }
            Self::Rejected {
                route,
                caller,
                reason,
            } => write!(
                f,
                "{} caller {caller} was rejected: {reason:?}",
                route.label()
            ),
            Self::ReplyRouteMismatch { requested, replied } => {
                write!(f, "{} request received {replied} reply", requested.label())
            }
            Self::RefillOutcome(outcome) => write!(f, "refill call did not succeed: {outcome:?}"),
            Self::HoldFailed {
                route,
                caller,
                error,
            } => write!(
                f,
                "{} caller {caller} hold timer failed: {error:?}",
                route.label()
            ),
            Self::CapacityName(error) => write!(f, "capacity summary failed: {error}"),
        }
    }
}

impl std::error::Error for GatewayWorkloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registration(error) | Self::HostCall { source: error, .. } => Some(error),
            Self::CapacityName(error) => Some(error),
            _ => None,
        }
    }
}

impl AsRef<dyn std::error::Error + Send + Sync + 'static> for GatewayWorkloadError {
    fn as_ref(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }
}

pub type GatewayTerminalError = RunToShutdownError<ReportedWorkloadError<GatewayWorkloadError>>;

#[derive(Debug)]
pub enum RunError {
    InvalidConfig(RunConfigError),
    Startup(StartupError),
    Terminal(Box<GatewayTerminalError>),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => write!(f, "invalid gateway configuration: {error}"),
            Self::Startup(error) => write!(f, "gateway startup failed: {error}"),
            Self::Terminal(error) => write!(f, "gateway run failed: {error}"),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidConfig(error) => Some(error),
            Self::Startup(error) => Some(error),
            Self::Terminal(error) => Some(error.as_ref()),
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
    HoldFailed(CallError),
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
                    sleep(hold).then_service_event(move |result| GatewayEvent::HoldDone {
                        qid,
                        route,
                        result,
                    })
                }
                Err(ConcurrencyGuardedInsertError::Admission { reply, failure, .. }) => {
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
                Err(ConcurrencyGuardedInsertError::DuplicateKey { reply, .. }) => reply_to::<Self>(
                    reply,
                    GatewayReply::Full {
                        filled: "gateway.duplicate".into(),
                        requested: 1,
                        current: 0,
                        max: 0,
                    },
                ),
            }
        })
    }

    fn hold_done(&mut self, qid: u64, route: Route, result: SleepReply) -> Effect<Self> {
        let reply = hold_reply(route, result);
        let Some(effect) = self.pending.reply_by_key::<Self>(&qid, reply) else {
            return noop();
        };
        effect
    }
}

fn hold_reply(route: Route, result: SleepReply) -> GatewayReply {
    match result {
        Ok(()) => GatewayReply::Ok {
            route: route.label(),
        },
        Err(error) => GatewayReply::HoldFailed(error),
    }
}

struct WorkloadReport {
    upload_admitted: usize,
    upload_full: usize,
    upload_timeout: usize,
    list_admitted: usize,
    list_full: usize,
    list_timeout: usize,
    caller_outcomes: Vec<ObservedCallerOutcome>,
    refill_reply: Option<GatewayReply>,
    scope_after_wave: SharedScopeReport,
    body_after_wave: SharedScopeReport,
    scope_pre: SharedScopeReport,
    body_pre: SharedScopeReport,
    scope_line: String,
    body_line: String,
    surface_line: String,
}

pub fn run(config: RunConfig) -> Result<RunReport, RunError> {
    let config = config.validate().map_err(RunError::InvalidConfig)?;
    let runtime = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .map_err(RunError::Startup)?;
    let scope = SharedCapacityScope::new("gateway.in_flight", "weight", config.shared_cap);
    let body_scope = SharedCapacityScope::new("gateway.body_bytes", "bytes", config.body_cap);
    let result = runtime.run_to_shutdown_reported(Duration::from_secs(5), |runtime| {
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
            .map_err(GatewayWorkloadError::Registration)?;

        let timeout = Duration::from_millis(config.call_timeout_ms);
        let total_callers = config.upload_callers + config.list_callers;
        let barrier = Arc::new(Barrier::new(total_callers + 1));
        let outcomes = thread::scope(|thread_scope| {
            let mut threads = Vec::with_capacity(total_callers);
            for caller in 0..config.upload_callers {
                let gate = Arc::clone(&barrier);
                let hold = Duration::from_millis(config.upload_hold_ms);
                let addr = gateway.requests;
                threads.push((
                    Route::Upload,
                    caller,
                    thread_scope.spawn(move || {
                        gate.wait();
                        runtime.call_blocking_request(
                            addr,
                            GatewayRequest::Request {
                                route: Route::Upload,
                                hold,
                            },
                            timeout,
                        )
                    }),
                ));
            }
            for caller in 0..config.list_callers {
                let gate = Arc::clone(&barrier);
                let hold = Duration::from_millis(config.list_hold_ms);
                let addr = gateway.requests;
                threads.push((
                    Route::List,
                    caller,
                    thread_scope.spawn(move || {
                        gate.wait();
                        runtime.call_blocking_request(
                            addr,
                            GatewayRequest::Request {
                                route: Route::List,
                                hold,
                            },
                            timeout,
                        )
                    }),
                ));
            }
            barrier.wait();
            threads
                .into_iter()
                .map(|(route, caller, thread)| {
                    thread
                        .join()
                        .map(|outcome| (route, caller, outcome))
                        .map_err(|_| GatewayWorkloadError::CallerPanicked { route, caller })
                })
                .collect::<Result<Vec<_>, _>>()
        })?;

        let mut upload_admitted = 0usize;
        let mut upload_full = 0usize;
        let mut upload_timeout = 0usize;
        let mut list_admitted = 0usize;
        let mut list_full = 0usize;
        let mut list_timeout = 0usize;
        let mut caller_outcomes = Vec::with_capacity(total_callers);
        for (route, caller, outcome) in outcomes {
            let outcome = outcome.map_err(|source| GatewayWorkloadError::HostCall {
                route,
                caller,
                source,
            })?;
            let observed = match outcome {
                CallOutcome::Replied(reply @ GatewayReply::Ok { route: replied }) => {
                    if replied != route.label() {
                        return Err(GatewayWorkloadError::ReplyRouteMismatch {
                            requested: route,
                            replied,
                        });
                    }
                    match route {
                        Route::Upload => upload_admitted += 1,
                        Route::List => list_admitted += 1,
                    }
                    ObservedCallOutcome::Replied(reply)
                }
                CallOutcome::Replied(reply @ GatewayReply::Full { .. }) => {
                    match route {
                        Route::Upload => upload_full += 1,
                        Route::List => list_full += 1,
                    }
                    ObservedCallOutcome::Replied(reply)
                }
                CallOutcome::Replied(GatewayReply::HoldFailed(error)) => {
                    return Err(GatewayWorkloadError::HoldFailed {
                        route,
                        caller,
                        error,
                    });
                }
                CallOutcome::Timeout => {
                    match route {
                        Route::Upload => upload_timeout += 1,
                        Route::List => list_timeout += 1,
                    }
                    ObservedCallOutcome::Timeout
                }
                CallOutcome::Full => {
                    return Err(GatewayWorkloadError::MailboxFull { route, caller });
                }
                CallOutcome::Closed => {
                    return Err(GatewayWorkloadError::Closed { route, caller });
                }
                CallOutcome::Rejected(reason) => {
                    return Err(GatewayWorkloadError::Rejected {
                        route,
                        caller,
                        reason,
                    });
                }
            };
            caller_outcomes.push(ObservedCallerOutcome {
                route,
                caller,
                outcome: observed,
            });
        }

        let scope_after_wave = scope.snapshot();
        let body_after_wave = body_scope.snapshot();
        let refill_reply = if upload_timeout + list_timeout == 0 {
            let refill_outcome = runtime
                .call_blocking_request(
                    gateway.requests,
                    GatewayRequest::Request {
                        route: Route::List,
                        hold: Duration::from_millis(1),
                    },
                    timeout,
                )
                .map_err(|source| GatewayWorkloadError::HostCall {
                    route: Route::List,
                    caller: total_callers,
                    source,
                })?;
            match refill_outcome {
                CallOutcome::Replied(reply @ GatewayReply::Ok { route: "list" }) => Some(reply),
                other => return Err(GatewayWorkloadError::RefillOutcome(other)),
            }
        } else {
            None
        };

        let scope_pre = scope.snapshot();
        let body_pre = body_scope.snapshot();
        let scope_line = scope.discovery_line();
        let body_line = body_scope.discovery_line();
        let surface_line =
            format_discovery_line(&scope.surface_report(tina::capacity::CapacityMode::Fixed));
        let mut capacity_summary = CapacitySummary::new();
        capacity_summary
            .push(scope.surface_report(tina::capacity::CapacityMode::Fixed))
            .map_err(GatewayWorkloadError::CapacityName)?;
        capacity_summary
            .push(body_scope.surface_report(tina::capacity::CapacityMode::Fixed))
            .map_err(GatewayWorkloadError::CapacityName)?;
        if let Err(errors) = capacity_summary.assert_no_full() {
            for error in &errors {
                eprintln!("{}", format_assertion_failure(error));
            }
        }

        Ok(WorkloadReport {
            upload_admitted,
            upload_full,
            upload_timeout,
            list_admitted,
            list_full,
            list_timeout,
            caller_outcomes,
            refill_reply,
            scope_after_wave,
            body_after_wave,
            scope_pre,
            body_pre,
            scope_line,
            body_line,
            surface_line,
        })
    });

    let snap = scope.snapshot();
    let body_snap = body_scope.snapshot();
    let workload = result.map_err(|error| RunError::Terminal(Box::new(error)))?;

    let summary_line = format!(
        "system=system_api_gateway_limits upload_admitted={} upload_full={} upload_timeout={} list_admitted={} list_full={} list_timeout={} scope_high_water={} scope_full_count={} scope_current_at_drain={} body_high_water={} body_full_count={} body_current_at_drain={}",
        workload.upload_admitted,
        workload.upload_full,
        workload.upload_timeout,
        workload.list_admitted,
        workload.list_full,
        workload.list_timeout,
        snap.high_water,
        snap.full_count,
        snap.current,
        body_snap.high_water,
        body_snap.full_count,
        body_snap.current,
    );

    Ok(RunReport {
        upload_admitted: workload.upload_admitted,
        upload_full: workload.upload_full,
        upload_timeout: workload.upload_timeout,
        list_admitted: workload.list_admitted,
        list_full: workload.list_full,
        list_timeout: workload.list_timeout,
        scope_high_water: workload.scope_pre.high_water,
        scope_full_count: snap.full_count,
        scope_admitted: snap.admitted,
        scope_released: snap.released,
        scope_current_at_drain: snap.current,
        scope_high_water_at_drain: snap.high_water,
        body_high_water: workload.body_pre.high_water,
        body_full_count: body_snap.full_count,
        body_admitted: body_snap.admitted,
        body_released: body_snap.released,
        body_current_at_drain: body_snap.current,
        scope_after_wave: workload.scope_after_wave,
        body_after_wave: workload.body_after_wave,
        caller_outcomes: workload.caller_outcomes,
        refill_reply: workload.refill_reply,
        discovery_lines: vec![
            workload.scope_line,
            workload.body_line,
            workload.surface_line,
        ],
        summary_line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_timer_failure_is_not_reported_as_success() {
        assert_eq!(
            hold_reply(Route::Upload, Err(CallError::TimerFull)),
            GatewayReply::HoldFailed(CallError::TimerFull)
        );
        assert_eq!(
            hold_reply(Route::List, Ok(())),
            GatewayReply::Ok { route: "list" }
        );
    }
}
