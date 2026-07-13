//! Per-tenant rate limiting with owner-stamped admission time.

use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, RateLimit, RateLimitConfig,
    RateLimitDecision, ReportedWorkloadError, RunToShutdownError, StartupError,
    ThreadedRuntimeError, format_discovery_line,
};

/// Tenant identifier. Static strings keep the request path allocation-free.
pub type TenantId = &'static str;

/// Requests understood by the request-only gateway service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayRequest {
    /// Attempt one admission for a tenant.
    Admit {
        /// Tenant to charge.
        tenant: TenantId,
    },
    /// Read the limiter's current capacity state.
    Snapshot,
    /// Close the policy and probe the resulting terminal decision.
    CloseAndProbe {
        /// Tenant used for the probe.
        tenant: TenantId,
    },
}

/// Reply shape preserving every decision in `RateLimitDecision`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayReply {
    /// One token was admitted and consumed by the decision.
    Admitted {
        /// Tenant charged by the decision.
        tenant: TenantId,
    },
    /// The tenant's token bucket was empty.
    RateLimited {
        /// Tenant refused by the decision.
        tenant: TenantId,
        /// Exact owner-computed delay until another token is available.
        retry_after: Duration,
    },
    /// No key-table slot was available for a new tenant.
    KeyCapacityFull {
        /// Tenant refused by the decision.
        tenant: TenantId,
    },
    /// The rate-limit policy was closed.
    Closed {
        /// Tenant refused by the decision.
        tenant: TenantId,
    },
    /// Capacity state observed by the owner.
    Snapshot(SnapshotReport),
}

/// Snapshot of gateway-owned capacity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotReport {
    /// Live distinct tenants tracked by the fixed table.
    pub live_tenants: usize,
    /// Cumulative rate-limited decisions.
    pub rate_limited_count: u64,
    /// Cumulative key-capacity-full decisions.
    pub full_count: u64,
    /// Grep-friendly capacity discovery line.
    pub discovery_line: String,
}

/// Request-only rate-limit owner.
pub struct Gateway {
    rate: RateLimit<TenantId>,
}

impl Gateway {
    /// Build a gateway around a configured rate limiter.
    pub fn new(rate: RateLimit<TenantId>) -> Self {
        Self { rate }
    }

    fn admit(&mut self, tenant: TenantId, now: std::time::Instant) -> GatewayReply {
        match self.rate.try_admit_at(&tenant, now) {
            RateLimitDecision::Admitted => GatewayReply::Admitted { tenant },
            RateLimitDecision::RateLimited { retry_after, .. } => GatewayReply::RateLimited {
                tenant,
                retry_after,
            },
            RateLimitDecision::KeyCapacityFull(_) => GatewayReply::KeyCapacityFull { tenant },
            RateLimitDecision::Closed(_) => GatewayReply::Closed { tenant },
        }
    }

    fn snapshot(&self) -> SnapshotReport {
        let report = self.rate.report();
        SnapshotReport {
            live_tenants: report.current,
            rate_limited_count: report.rate_limited_count,
            full_count: report.full_count,
            discovery_line: format_discovery_line(&self.rate.capacity_surface()),
        }
    }
}

#[tina_runtime::isolate(request = GatewayRequest, reply = GatewayReply)]
impl Gateway {
    fn handle_request(
        &mut self,
        request: GatewayRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            GatewayRequest::Admit { tenant } => {
                let now = call.now();
                call.reply(self.admit(tenant, now))
            }
            GatewayRequest::Snapshot => call.reply(GatewayReply::Snapshot(self.snapshot())),
            GatewayRequest::CloseAndProbe { tenant } => {
                self.rate.close();
                let now = call.now();
                call.reply(self.admit(tenant, now))
            }
        }
    }
}

const MAX_MAILBOX: usize = 65_536;
const MAX_TENANTS: usize = 65_536;
const MAX_REQUESTS_PER_TENANT: usize = 2_000_000;
const MAX_TOTAL_REQUESTS: usize = 2_000_000;
const MAX_RATE_PER_SEC: u64 = 1_000_000_000;
const MAX_BURST: u32 = 1_000_000;
const MAX_CALL_TIMEOUT_MS: u64 = 60_000;

/// Specimen configuration.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    /// Gateway mailbox capacity.
    pub mailbox: usize,
    /// Maximum number of distinct tenants retained.
    pub max_tenants: usize,
    /// Tokens refilled per tenant per second.
    pub rate_per_sec: u64,
    /// Maximum tokens held per tenant.
    pub burst: u32,
    /// Requests issued for the hot tenant.
    pub hot_requests: usize,
    /// Requests issued for the cold tenant.
    pub cold_requests: usize,
    /// Host deadline for each call.
    pub call_timeout_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            mailbox: 32,
            max_tenants: 4,
            rate_per_sec: 10,
            burst: 3,
            hot_requests: 8,
            cold_requests: 3,
            call_timeout_ms: 1_000,
        }
    }
}

/// Invalid configuration rejected before runtime construction or allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunConfigError {
    /// A non-zero bounded field was zero.
    Zero { field: &'static str },
    /// A bounded field exceeded its public limit.
    TooLarge {
        field: &'static str,
        requested: u128,
        max: u128,
    },
    /// The combined request count overflowed or exceeded its public limit.
    TotalRequests { hot: usize, cold: usize },
}

impl fmt::Display for RunConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero { field } => write!(f, "{field} must be greater than zero"),
            Self::TooLarge {
                field,
                requested,
                max,
            } => write!(f, "{field} {requested} exceeds maximum {max}"),
            Self::TotalRequests { hot, cold } => write!(
                f,
                "combined request count for hot={hot} and cold={cold} exceeds {MAX_TOTAL_REQUESTS}"
            ),
        }
    }
}

impl Error for RunConfigError {}

impl RunConfig {
    /// Validate all panic and allocation bounds before starting Tina.
    pub fn validate(self) -> Result<Self, RunConfigError> {
        validate_usize("mailbox", self.mailbox, MAX_MAILBOX)?;
        validate_usize("max_tenants", self.max_tenants, MAX_TENANTS)?;
        validate_u64("rate_per_sec", self.rate_per_sec, MAX_RATE_PER_SEC)?;
        validate_u32("burst", self.burst, MAX_BURST)?;
        validate_usize("hot_requests", self.hot_requests, MAX_REQUESTS_PER_TENANT)?;
        validate_usize("cold_requests", self.cold_requests, MAX_REQUESTS_PER_TENANT)?;
        validate_u64("call_timeout_ms", self.call_timeout_ms, MAX_CALL_TIMEOUT_MS)?;
        let Some(total) = self.hot_requests.checked_add(self.cold_requests) else {
            return Err(RunConfigError::TotalRequests {
                hot: self.hot_requests,
                cold: self.cold_requests,
            });
        };
        if total > MAX_TOTAL_REQUESTS {
            return Err(RunConfigError::TotalRequests {
                hot: self.hot_requests,
                cold: self.cold_requests,
            });
        }
        Ok(self)
    }
}

fn validate_usize(field: &'static str, value: usize, max: usize) -> Result<(), RunConfigError> {
    if value == 0 {
        Err(RunConfigError::Zero { field })
    } else if value > max {
        Err(RunConfigError::TooLarge {
            field,
            requested: value as u128,
            max: max as u128,
        })
    } else {
        Ok(())
    }
}

fn validate_u64(field: &'static str, value: u64, max: u64) -> Result<(), RunConfigError> {
    if value == 0 {
        Err(RunConfigError::Zero { field })
    } else if value > max {
        Err(RunConfigError::TooLarge {
            field,
            requested: u128::from(value),
            max: u128::from(max),
        })
    } else {
        Ok(())
    }
}

fn validate_u32(field: &'static str, value: u32, max: u32) -> Result<(), RunConfigError> {
    validate_u64(field, u64::from(value), u64::from(max))
}

/// What the run observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Hot-tenant admitted replies.
    pub hot_admitted: usize,
    /// Hot-tenant rate-limited replies.
    pub hot_limited: usize,
    /// Cold-tenant admitted replies.
    pub cold_admitted: usize,
    /// Cold-tenant rate-limited replies.
    pub cold_limited: usize,
    /// Hot-tenant retry delays in observation order.
    pub hot_retry_afters_ms: Vec<u128>,
    /// Owner snapshot immediately before shutdown.
    pub snapshot: SnapshotReport,
    /// Grep-friendly outcome summary.
    pub summary_line: String,
}

/// Typed workload failure retaining runtime errors and complete call outcomes.
#[derive(Debug)]
pub enum WorkloadError {
    /// The runtime refused gateway registration.
    Registration(ThreadedRuntimeError),
    /// The host could not complete a runtime call operation.
    HostCall {
        /// Workload phase issuing the call.
        phase: &'static str,
        /// Zero-based call index within the phase.
        index: usize,
        /// Exact runtime/control-plane failure.
        source: ThreadedRuntimeError,
    },
    /// The call completed with a domain-terminal outcome this phase cannot use.
    UnexpectedOutcome {
        /// Workload phase issuing the call.
        phase: &'static str,
        /// Zero-based call index within the phase.
        index: usize,
        /// Complete Tina terminal outcome, including rejection reason.
        outcome: CallOutcome<GatewayReply>,
    },
}

impl fmt::Display for WorkloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registration(error) => write!(f, "gateway registration failed: {error}"),
            Self::HostCall {
                phase,
                index,
                source,
            } => write!(f, "{phase} call {index} failed: {source}"),
            Self::UnexpectedOutcome {
                phase,
                index,
                outcome,
            } => write!(f, "{phase} call {index} returned {outcome:?}"),
        }
    }
}

impl Error for WorkloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Registration(error) | Self::HostCall { source: error, .. } => Some(error),
            Self::UnexpectedOutcome { .. } => None,
        }
    }
}

impl AsRef<dyn Error + Send + Sync + 'static> for WorkloadError {
    fn as_ref(&self) -> &(dyn Error + Send + Sync + 'static) {
        self
    }
}

pub type TerminalError = RunToShutdownError<ReportedWorkloadError<WorkloadError>>;

/// Top-level run failure with configuration, startup, and terminal truth intact.
#[derive(Debug)]
pub enum RunError {
    /// Configuration failed bounded preflight validation.
    InvalidConfig(RunConfigError),
    /// The local threaded runtime could not start.
    Startup(StartupError),
    /// Workload failure, shutdown failure, or both.
    Terminal(Box<TerminalError>),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => write!(f, "invalid tenant limiter config: {error}"),
            Self::Startup(error) => write!(f, "tenant limiter startup failed: {error}"),
            Self::Terminal(error) => write!(f, "tenant limiter run failed: {error}"),
        }
    }
}

impl Error for RunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidConfig(error) => Some(error),
            Self::Startup(error) => Some(error),
            Self::Terminal(error) => Some(error.as_ref()),
        }
    }
}

/// Run the live specimen with bounded, consuming shutdown.
pub fn run(config: RunConfig) -> Result<RunReport, RunError> {
    let config = config.validate().map_err(RunError::InvalidConfig)?;
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .map_err(RunError::Startup)?;
    app.run_to_shutdown_reported(Duration::from_secs(5), |app| run_workload(app, config))
        .map_err(|error| RunError::Terminal(Box::new(error)))
}

fn run_workload(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    config: RunConfig,
) -> Result<RunReport, WorkloadError> {
    let rate = RateLimit::<TenantId>::new(
        "tenant.rate",
        RateLimitConfig {
            max_keys: config.max_tenants,
            rate_per_sec: config.rate_per_sec,
            burst: config.burst,
        },
    );
    let gateway = app
        .register_request_service::<Gateway, GatewayRequest, Infallible>(
            Gateway::new(rate),
            config.mailbox,
        )
        .map_err(WorkloadError::Registration)?;
    let timeout = Duration::from_millis(config.call_timeout_ms);

    let mut hot_admitted = 0;
    let mut hot_limited = 0;
    let mut hot_retry_afters_ms = Vec::with_capacity(config.hot_requests);
    for index in 0..config.hot_requests {
        match call(app, gateway, "hot", index, "tenant.hot", timeout)? {
            GatewayReply::Admitted { .. } => hot_admitted += 1,
            GatewayReply::RateLimited { retry_after, .. } => {
                hot_limited += 1;
                hot_retry_afters_ms.push(retry_after.as_millis());
            }
            reply => {
                return Err(WorkloadError::UnexpectedOutcome {
                    phase: "hot",
                    index,
                    outcome: CallOutcome::Replied(reply),
                });
            }
        }
    }

    let mut cold_admitted = 0;
    let mut cold_limited = 0;
    for index in 0..config.cold_requests {
        match call(app, gateway, "cold", index, "tenant.cold", timeout)? {
            GatewayReply::Admitted { .. } => cold_admitted += 1,
            GatewayReply::RateLimited { .. } => cold_limited += 1,
            reply => {
                return Err(WorkloadError::UnexpectedOutcome {
                    phase: "cold",
                    index,
                    outcome: CallOutcome::Replied(reply),
                });
            }
        }
    }

    let snapshot =
        match call_outcome(app, gateway, GatewayRequest::Snapshot, timeout).map_err(|source| {
            WorkloadError::HostCall {
                phase: "snapshot",
                index: 0,
                source,
            }
        })? {
            CallOutcome::Replied(GatewayReply::Snapshot(snapshot)) => snapshot,
            outcome => {
                return Err(WorkloadError::UnexpectedOutcome {
                    phase: "snapshot",
                    index: 0,
                    outcome,
                });
            }
        };

    let summary_line = format!(
        "system=system_tenant_rate_limiter hot_admitted={hot_admitted} hot_limited={hot_limited} \
         cold_admitted={cold_admitted} cold_limited={cold_limited} live_tenants={} \
         rate_limited_count={}",
        snapshot.live_tenants, snapshot.rate_limited_count,
    );
    Ok(RunReport {
        hot_admitted,
        hot_limited,
        cold_admitted,
        cold_limited,
        hot_retry_afters_ms,
        snapshot,
        summary_line,
    })
}

fn call(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    gateway: tina_runtime::RequestServiceHandle<GatewayRequest, GatewayReply>,
    phase: &'static str,
    index: usize,
    tenant: TenantId,
    timeout: Duration,
) -> Result<GatewayReply, WorkloadError> {
    let outcome = call_outcome(app, gateway, GatewayRequest::Admit { tenant }, timeout).map_err(
        |source| WorkloadError::HostCall {
            phase,
            index,
            source,
        },
    )?;
    expect_reply(phase, index, outcome)
}

fn expect_reply(
    phase: &'static str,
    index: usize,
    outcome: CallOutcome<GatewayReply>,
) -> Result<GatewayReply, WorkloadError> {
    match outcome {
        CallOutcome::Replied(reply) => Ok(reply),
        outcome => Err(WorkloadError::UnexpectedOutcome {
            phase,
            index,
            outcome,
        }),
    }
}

fn call_outcome(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    gateway: tina_runtime::RequestServiceHandle<GatewayRequest, GatewayReply>,
    request: GatewayRequest,
    timeout: Duration,
) -> Result<CallOutcome<GatewayReply>, ThreadedRuntimeError> {
    app.call_blocking_request(gateway, request, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn maps_all_rate_limit_decisions() {
        let now = Instant::now();
        let mut gateway = Gateway::new(RateLimit::new(
            "test",
            RateLimitConfig {
                max_keys: 1,
                rate_per_sec: 10,
                burst: 1,
            },
        ));
        assert!(matches!(
            gateway.admit("hot", now),
            GatewayReply::Admitted { tenant: "hot" }
        ));
        assert!(matches!(
            gateway.admit("hot", now),
            GatewayReply::RateLimited { tenant: "hot", .. }
        ));
        assert!(matches!(
            gateway.admit("cold", now),
            GatewayReply::KeyCapacityFull { tenant: "cold" }
        ));
        gateway.rate.close();
        assert!(matches!(
            gateway.admit("hot", now),
            GatewayReply::Closed { tenant: "hot" }
        ));
        let snapshot = gateway.snapshot();
        assert_eq!(snapshot.rate_limited_count, 1);
        assert_eq!(snapshot.full_count, 1);
    }

    #[test]
    fn refill_uses_monotonic_owner_time() {
        let now = Instant::now();
        let mut gateway = Gateway::new(RateLimit::new(
            "test",
            RateLimitConfig {
                max_keys: 1,
                rate_per_sec: 10,
                burst: 1,
            },
        ));
        assert!(matches!(
            gateway.admit("hot", now),
            GatewayReply::Admitted { .. }
        ));
        assert!(matches!(
            gateway.admit("hot", now),
            GatewayReply::RateLimited { .. }
        ));
        assert!(matches!(
            gateway.admit("hot", now + Duration::from_millis(100)),
            GatewayReply::Admitted { .. }
        ));
        assert_eq!(gateway.snapshot().rate_limited_count, 1);
    }

    #[test]
    fn host_terminal_vocabulary_is_retained_without_collapse() {
        use tina::CallRejectedReason;

        let outcomes = [
            CallOutcome::Full,
            CallOutcome::Closed,
            CallOutcome::Timeout,
            CallOutcome::Rejected(CallRejectedReason::UnsupportedMessage),
        ];
        for outcome in outcomes {
            let error = expect_reply("probe", 7, outcome).expect_err("must retain terminal");
            match error {
                WorkloadError::UnexpectedOutcome {
                    phase: "probe",
                    index: 7,
                    outcome: actual,
                } => match actual {
                    CallOutcome::Full
                    | CallOutcome::Closed
                    | CallOutcome::Timeout
                    | CallOutcome::Rejected(CallRejectedReason::UnsupportedMessage) => {}
                    other => panic!("collapsed outcome: {other:?}"),
                },
                other => panic!("wrong error: {other:?}"),
            }
        }

        let error = WorkloadError::HostCall {
            phase: "probe",
            index: 9,
            source: ThreadedRuntimeError::WorkerUnresponsive,
        };
        assert!(matches!(
            error,
            WorkloadError::HostCall {
                source: ThreadedRuntimeError::WorkerUnresponsive,
                ..
            }
        ));
    }
}
