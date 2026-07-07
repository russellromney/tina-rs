//! Per-tenant rate-limit specimen for the admission policy layer.
//!
//! A single ingress isolate serves requests for many tenants. Each tenant
//! gets its own token bucket. A hot tenant fills its bucket and the next
//! request comes back as `Limited { retry_after }`; a cold tenant arriving
//! at the same moment still succeeds because each tenant owns its own
//! bucket.
//!
//! Two truths the specimen proves:
//!
//! 1. `retry_after` is a deterministic function of `(rate, burst, now,
//!    key history)`. Two runs with the same script produce byte-identical
//!    `retry_after` values.
//! 2. Cold tenants make progress while a hot tenant is rate-limited.
//!
//! The service itself is plain Tina: bounded mailbox, request/reply with
//! `CallContext`, no hidden retry, no background sweeper. The policy
//! lives in `tina_runtime::RateLimit`.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tina::CallRejectedReason;
use tina::prelude::*;
use tina_runtime::{
    AdmissionDecision, CallOutcome, CapacitySummary, DefaultThreadedMailboxFactory, RateLimit,
    ServiceHandle, ThreadedRuntime, format_discovery_line,
};

/// Tenant identifier. Static strings keep the specimen allocation-free
/// in the hot path.
pub type TenantId = &'static str;

/// One request from a caller to the gateway.
#[derive(Debug)]
pub enum GatewayMsg {
    /// `now` is the caller-supplied admission timestamp. In production
    /// services this is `ctx.now()` on the caller side; the specimen
    /// drives it explicitly so the replay determinism is visible.
    Request {
        /// Tenant to charge against.
        tenant: TenantId,
        /// Admission timestamp.
        now: Instant,
    },
    /// Read the limiter's current capacity surface. Equivalent to a
    /// `GET /debug/capacity` probe in a real edge service.
    Snapshot,
}

/// Reply shape. Honest about which rejection bucket the policy chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayReply {
    /// Admitted. Caller proceeds.
    Ok {
        /// Tenant the request was charged to.
        tenant: TenantId,
    },
    /// Tenant rate-limit empty; caller should sleep at least `retry_after`
    /// and try again. The retry decision itself is caller-owned.
    Limited {
        /// Tenant that hit its bucket.
        tenant: TenantId,
        /// Earliest moment the caller could retry.
        retry_after: Duration,
    },
    /// Key table full — no slot for a fresh tenant.
    TenantTableFull {
        /// Tenant that could not be admitted.
        tenant: TenantId,
    },
    /// Snapshot of the limiter's capacity surface and rejection counts.
    Snapshot(SnapshotReport),
}

/// Snapshot of the limiter as seen by the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotReport {
    /// Live distinct tenants the limiter is tracking.
    pub live_tenants: usize,
    /// Cumulative `RateLimited` decisions.
    pub rate_limited_count: u64,
    /// Cumulative `Full` (table full) decisions.
    pub full_count: u64,
    /// One-line discovery summary for the capacity surface.
    pub discovery_line: String,
}

/// Gateway isolate. Exposed so smoke tests can construct it directly.
pub struct Gateway {
    rate: RateLimit<TenantId>,
}

impl Gateway {
    /// Build a gateway around a pre-configured limiter.
    pub fn new(rate: RateLimit<TenantId>) -> Self {
        Self { rate }
    }
}

#[tina_runtime::isolate(message = GatewayMsg, reply = GatewayReply)]
impl Gateway {
    fn handle(
        &mut self,
        _msg: GatewayMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, msg: GatewayMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            GatewayMsg::Request { tenant, now } => match self.rate.try_admit(&tenant, now) {
                AdmissionDecision::Admitted(_grant) => call.reply(GatewayReply::Ok { tenant }),
                AdmissionDecision::RateLimited { retry_after, .. } => {
                    call.reply(GatewayReply::Limited {
                        tenant,
                        retry_after,
                    })
                }
                AdmissionDecision::Full(_) => call.reply(GatewayReply::TenantTableFull { tenant }),
                AdmissionDecision::Closed(_)
                | AdmissionDecision::Wait { .. }
                | AdmissionDecision::Degrade { .. }
                | AdmissionDecision::TimedOut(_) => {
                    // The policy is configured as Shed; these arms are
                    // unreachable in the default first form. Reject loudly
                    // if a future change rewires the policy.
                    call.reject(CallRejectedReason::UnsupportedMessage)
                }
            },
            GatewayMsg::Snapshot => {
                let report = self.rate.report();
                let mut summary = CapacitySummary::new();
                summary
                    .push(self.rate.capacity_surface())
                    .expect("push surface");
                let line = format_discovery_line(summary.surface("tenant.rate").report().unwrap());
                call.reply(GatewayReply::Snapshot(SnapshotReport {
                    live_tenants: report.current,
                    rate_limited_count: report.rate_limited_count,
                    full_count: report.full_count,
                    discovery_line: line,
                }))
            }
        }
    }
}

/// Specimen configuration. Each field is grep-friendly so changing the
/// load shape stays explicit.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    /// Gateway mailbox capacity (call admissions + replies).
    pub mailbox: usize,
    /// Maximum number of distinct tenants the limiter remembers at once.
    pub max_tenants: usize,
    /// Per-tenant rate in tokens per second.
    pub rate_per_sec: u64,
    /// Per-tenant burst (max tokens the bucket holds).
    pub burst: u32,
    /// Number of requests the hot tenant fires.
    pub hot_requests: usize,
    /// Number of requests the cold tenant fires.
    pub cold_requests: usize,
    /// Caller timeout for each request.
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

/// What the run observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Hot-tenant admitted count.
    pub hot_admitted: usize,
    /// Hot-tenant Limited count.
    pub hot_limited: usize,
    /// Cold-tenant admitted count.
    pub cold_admitted: usize,
    /// Cold-tenant Limited count (zero under the default config).
    pub cold_limited: usize,
    /// Hot-tenant `retry_after` values, in order, in milliseconds.
    /// Deterministic for fixed `(rate, burst, now-sequence)` inputs.
    pub hot_retry_afters_ms: Vec<u128>,
    /// Snapshot of the limiter at the end of the run, before shutdown.
    pub snapshot: SnapshotReport,
    /// One-line grep-friendly summary.
    pub summary_line: String,
}

/// Drive the specimen with `config`.
pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let rate = RateLimit::<TenantId>::new(
        "tenant.rate",
        config.max_tenants,
        config.rate_per_sec,
        config.burst,
    );

    let gateway: ServiceHandle<GatewayMsg, GatewayReply> = runtime
        .register_service::<_, Infallible>(Gateway::new(rate), config.mailbox)
        .map_err(|e| anyhow::anyhow!("register gateway: {e:?}"))?;

    let timeout = Duration::from_millis(config.call_timeout_ms);
    let base = Instant::now();

    let mut hot_admitted = 0usize;
    let mut hot_limited = 0usize;
    let mut hot_retry_afters_ms: Vec<u128> = Vec::with_capacity(config.hot_requests);
    for _ in 0..config.hot_requests {
        let outcome = runtime
            .call_blocking_typed(
                gateway.call,
                GatewayMsg::Request {
                    tenant: "tenant.hot",
                    now: base,
                },
                timeout,
            )
            .map_err(|e| anyhow::anyhow!("hot call: {e:?}"))?;
        match outcome {
            CallOutcome::Replied(GatewayReply::Ok { .. }) => hot_admitted += 1,
            CallOutcome::Replied(GatewayReply::Limited { retry_after, .. }) => {
                hot_limited += 1;
                hot_retry_afters_ms.push(retry_after.as_millis());
            }
            CallOutcome::Replied(other) => anyhow::bail!("hot reply: {other:?}"),
            other => anyhow::bail!("hot outcome: {other:?}"),
        }
    }

    let mut cold_admitted = 0usize;
    let mut cold_limited = 0usize;
    for _ in 0..config.cold_requests {
        let outcome = runtime
            .call_blocking_typed(
                gateway.call,
                GatewayMsg::Request {
                    tenant: "tenant.cold",
                    now: base,
                },
                timeout,
            )
            .map_err(|e| anyhow::anyhow!("cold call: {e:?}"))?;
        match outcome {
            CallOutcome::Replied(GatewayReply::Ok { .. }) => cold_admitted += 1,
            CallOutcome::Replied(GatewayReply::Limited { .. }) => cold_limited += 1,
            CallOutcome::Replied(other) => anyhow::bail!("cold reply: {other:?}"),
            other => anyhow::bail!("cold outcome: {other:?}"),
        }
    }

    let snap_outcome = runtime
        .call_blocking_typed(gateway.call, GatewayMsg::Snapshot, timeout)
        .map_err(|e| anyhow::anyhow!("snapshot call: {e:?}"))?;
    let snapshot = match snap_outcome {
        CallOutcome::Replied(GatewayReply::Snapshot(s)) => s,
        other => anyhow::bail!("snapshot outcome: {other:?}"),
    };

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }

    let summary_line = format!(
        "system=system_tenant_rate_limiter hot_admitted={hot_admitted} hot_limited={hot_limited} \
         cold_admitted={cold_admitted} cold_limited={cold_limited} \
         live_tenants={live} rate_limited_count={rl}",
        live = snapshot.live_tenants,
        rl = snapshot.rate_limited_count,
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
