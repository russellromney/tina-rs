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
//!    key history)`. The gateway owns `now` through `call.now()`; simulator
//!    tests provide virtual time through the same policy method.
//! 2. Cold tenants make progress while a hot tenant is rate-limited.
//!
//! The service itself is plain Tina: bounded mailbox, request/reply with
//! `CallContext`, no hidden retry, no background sweeper. The policy
//! lives in `tina_runtime::RateLimit`.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, CapacitySummary, DefaultThreadedMailboxFactory, LocalSystem, RateLimit,
    RateLimitConfig, RateLimitDecision, format_discovery_line,
};

/// Tenant identifier. Static strings keep the specimen allocation-free
/// in the hot path.
pub type TenantId = &'static str;

/// One request from a caller to the gateway.
#[derive(Debug)]
pub enum GatewayMsg {
    /// Attempt one admission for `tenant`. The gateway owner supplies the
    /// logical timestamp from `call.now()`.
    Request {
        /// Tenant to charge against.
        tenant: TenantId,
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
    /// No tracked-key capacity remained for a fresh tenant.
    TenantCapacityFull {
        /// Tenant that could not be admitted.
        tenant: TenantId,
    },
    /// The limiter has been explicitly closed.
    Closed {
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
    /// Cumulative tracked-key-capacity decisions.
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
            GatewayMsg::Request { tenant } => match self.rate.try_admit_at(&tenant, call.now()) {
                RateLimitDecision::Admitted => call.reply(GatewayReply::Ok { tenant }),
                RateLimitDecision::RateLimited { retry_after, .. } => {
                    call.reply(GatewayReply::Limited {
                        tenant,
                        retry_after,
                    })
                }
                RateLimitDecision::KeyCapacityFull(_) => {
                    call.reply(GatewayReply::TenantCapacityFull { tenant })
                }
                RateLimitDecision::Closed(_) => call.reply(GatewayReply::Closed { tenant }),
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
    let runtime = Arc::new(
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?,
    );
    let shutdown = runtime.shutdown_handle();

    let rate = RateLimit::<TenantId>::new(
        "tenant.rate",
        RateLimitConfig {
            max_keys: config.max_tenants,
            rate_per_sec: config.rate_per_sec,
            burst: config.burst,
        },
    );

    let gateway = runtime
        .register_root::<_, Infallible>(Gateway::new(rate), config.mailbox)
        .map_err(|e| anyhow::anyhow!("register gateway: {e:?}"))?;

    let timeout = Duration::from_millis(config.call_timeout_ms);
    let mut hot_admitted = 0usize;
    let mut hot_limited = 0usize;
    let mut hot_retry_afters_ms: Vec<u128> = Vec::with_capacity(config.hot_requests);
    for _ in 0..config.hot_requests {
        let outcome = runtime
            .call_blocking(
                gateway,
                GatewayMsg::Request {
                    tenant: "tenant.hot",
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
            .call_blocking(
                gateway,
                GatewayMsg::Request {
                    tenant: "tenant.cold",
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
        .call_blocking(gateway, GatewayMsg::Snapshot, timeout)
        .map_err(|e| anyhow::anyhow!("snapshot call: {e:?}"))?;
    let snapshot = match snap_outcome {
        CallOutcome::Replied(GatewayReply::Snapshot(s)) => s,
        other => anyhow::bail!("snapshot outcome: {other:?}"),
    };

    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;

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
