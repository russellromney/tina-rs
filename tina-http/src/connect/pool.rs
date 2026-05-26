//! Fixed-endpoint HTTP/2 and gRPC client pools.
//!
//! Boring on purpose: a fixed list of resolved endpoints at construction,
//! round-robin over the healthy ones, a per-connection in-flight stream cap,
//! a pre-connect waiter cap, idle close and stale retire, and
//! `NoHealthyEndpoint` when every endpoint is down. No dynamic membership.
//!
//! The pool keeps HTTP/2 transport truth and gRPC status truth separate: an
//! HTTP/2 reset / GOAWAY / close / ALPN failure marks the endpoint unhealthy,
//! but a gRPC non-OK *status* (the server answered) leaves it healthy. The
//! [`http2_health_signal`] and [`grpc_health_signal`] classifiers make that
//! split explicit so they are never collapsed into one generic error.

use std::collections::VecDeque;

use tina::{Address, Shard};
use tina_runtime::budget::{BudgetCap, BudgetKind, BudgetSurface, BudgetUnit};
use tina_runtime::{
    DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeError,
};

use crate::grpc_client::GrpcUnaryOutcome;
use crate::http2::{
    Http2ClientConnection, Http2ClientLimits, Http2ClientMsg, Http2ClientOutcome, Http2ClientReply,
    Http2Target,
};

/// What a transport outcome says about one endpoint's health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointHealthSignal {
    /// The endpoint answered; keep it healthy.
    Healthy,
    /// The transport failed; mark the endpoint unhealthy and retire it.
    Unhealthy(EndpointDownReason),
    /// The endpoint is healthy but momentarily at capacity (admission full);
    /// do not mark it down.
    Busy,
}

/// Why an endpoint was marked unhealthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointDownReason {
    /// The connection closed or sent GOAWAY.
    Closed,
    /// A stream was reset by the peer.
    Reset,
    /// An HTTP/2 protocol error.
    ProtocolError,
    /// TLS ALPN did not yield h2.
    TlsAlpnMismatch,
    /// A connect/transport I/O failure.
    Transport,
}

/// Classify an HTTP/2 client outcome into a health signal.
///
/// A completed response (`Replied`) or a streaming head is healthy. An
/// admission `Full` is healthy-but-busy. A reset, GOAWAY/close, protocol
/// error, or ALPN mismatch is unhealthy. A local cancel is neutral
/// (treated as healthy: the pool, not the peer, ended the stream).
pub fn http2_health_signal(outcome: &Http2ClientOutcome) -> EndpointHealthSignal {
    match outcome {
        Http2ClientOutcome::Replied(_) | Http2ClientOutcome::ResponseStreaming { .. } => {
            EndpointHealthSignal::Healthy
        }
        Http2ClientOutcome::LocalCancel => EndpointHealthSignal::Healthy,
        Http2ClientOutcome::Full => EndpointHealthSignal::Busy,
        Http2ClientOutcome::Closed => {
            EndpointHealthSignal::Unhealthy(EndpointDownReason::Closed)
        }
        Http2ClientOutcome::Reset(_) => {
            EndpointHealthSignal::Unhealthy(EndpointDownReason::Reset)
        }
        Http2ClientOutcome::ProtocolError(_) => {
            EndpointHealthSignal::Unhealthy(EndpointDownReason::ProtocolError)
        }
        Http2ClientOutcome::TlsAlpnMismatch => {
            EndpointHealthSignal::Unhealthy(EndpointDownReason::TlsAlpnMismatch)
        }
    }
}

/// Classify a gRPC unary outcome into a health signal.
///
/// `Ok` and a non-OK `Status` both mean the server answered — the endpoint
/// stays healthy, because a gRPC status is an application result, not a
/// transport failure. Only a `Transport` outcome (or a malformed response
/// that proves the transport is wrong) marks the endpoint unhealthy.
pub fn grpc_health_signal<R>(outcome: &GrpcUnaryOutcome<R>) -> EndpointHealthSignal {
    match outcome {
        GrpcUnaryOutcome::Ok(_) | GrpcUnaryOutcome::Status(_) => EndpointHealthSignal::Healthy,
        GrpcUnaryOutcome::Transport(transport) => http2_health_signal(transport),
        // A response that reached the gRPC layer but was not well-formed: the
        // peer spoke, but not gRPC — treat as a protocol-level endpoint fault.
        GrpcUnaryOutcome::Malformed(_) => {
            EndpointHealthSignal::Unhealthy(EndpointDownReason::ProtocolError)
        }
    }
}

/// Configuration shared by the fixed-endpoint pools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedEndpointPoolConfig {
    /// Maximum in-flight streams per connection.
    pub max_in_flight_per_conn: usize,
    /// Maximum callers queued waiting for a connect/slot.
    pub pre_connect_queue_cap: usize,
    /// Maximum retained idle/stale endpoint reports.
    pub retained_reports: usize,
}

impl FixedEndpointPoolConfig {
    /// A small, boring default.
    pub fn balanced() -> Self {
        Self {
            max_in_flight_per_conn: 64,
            pre_connect_queue_cap: 128,
            retained_reports: 8,
        }
    }

    /// Validate the caps before first use.
    pub fn validate(&self) -> Result<(), FixedEndpointPoolError> {
        if self.max_in_flight_per_conn == 0 {
            return Err(FixedEndpointPoolError::ZeroStreams);
        }
        if self.retained_reports == 0 {
            return Err(FixedEndpointPoolError::ZeroRetainedReports);
        }
        Ok(())
    }

    /// Manifest rows for the pool caps under stable `{prefix}.*` names.
    ///
    /// `endpoints` is the fixed endpoint count; `max_connections` is one
    /// connection per endpoint in this first form.
    pub fn budget_surfaces(&self, prefix: &str, endpoints: usize) -> Vec<BudgetSurface> {
        vec![
            BudgetSurface::new(
                format!("{prefix}.endpoints"),
                BudgetKind::Pool,
                BudgetUnit::Connections,
                BudgetCap::fixed(endpoints.max(1)),
            )
            .owned_by("h2.pool"),
            BudgetSurface::new(
                format!("{prefix}.connections"),
                BudgetKind::Pool,
                BudgetUnit::Connections,
                BudgetCap::fixed(endpoints.max(1)),
            )
            .owned_by("h2.pool"),
            BudgetSurface::new(
                format!("{prefix}.in_flight_streams"),
                BudgetKind::ProtocolSession,
                BudgetUnit::Sessions,
                BudgetCap::fixed(self.max_in_flight_per_conn),
            )
            .owned_by("h2.pool"),
            BudgetSurface::new(
                format!("{prefix}.pre_connect_waiters"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                BudgetCap::fixed(self.pre_connect_queue_cap.max(1)),
            )
            .owned_by("h2.pool"),
            BudgetSurface::new(
                format!("{prefix}.retained_reports"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                BudgetCap::fixed(self.retained_reports),
            )
            .owned_by("h2.pool"),
        ]
    }
}

/// Why a fixed-endpoint pool config or operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedEndpointPoolError {
    /// `max_in_flight_per_conn` was zero.
    ZeroStreams,
    /// `retained_reports` was zero.
    ZeroRetainedReports,
    /// The pool was built with no endpoints.
    NoEndpoints,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EndpointSlot {
    route_key: String,
    healthy: bool,
    retired: bool,
    in_flight: usize,
    opened_streams: u64,
    down_count: u64,
}

/// A retained report for an endpoint that was retired or marked down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredEndpointReport {
    /// Endpoint index in the fixed list.
    pub index: usize,
    /// Endpoint route key.
    pub route_key: String,
    /// Why it was retired.
    pub reason: RetireReason,
}

/// Why an endpoint was retained as retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireReason {
    /// Marked unhealthy after a transport failure.
    Unhealthy(EndpointDownReason),
    /// Closed because it was idle.
    Idle,
    /// Retired because the connection went stale.
    Stale,
}

/// Outcome of asking the pool for an endpoint to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickOutcome {
    /// Use this endpoint index; a stream slot has been taken.
    Picked {
        /// Endpoint index in the fixed list.
        index: usize,
    },
    /// Every endpoint is unhealthy or retired.
    NoHealthyEndpoint,
    /// Healthy endpoints exist but all are at their in-flight stream cap.
    AllAtStreamCap,
}

/// The pure round-robin fixed-endpoint pool state.
#[derive(Debug, Clone)]
pub struct FixedEndpointPool {
    endpoints: Vec<EndpointSlot>,
    config: FixedEndpointPoolConfig,
    cursor: usize,
    pre_connect_waiters: usize,
    high_water_in_flight: usize,
    no_healthy_count: u64,
    all_busy_count: u64,
    pre_connect_rejections: u64,
    retained: VecDeque<RetiredEndpointReport>,
}

impl FixedEndpointPool {
    /// Build a pool over a fixed list of endpoint route keys.
    pub fn new(
        route_keys: Vec<String>,
        config: FixedEndpointPoolConfig,
    ) -> Result<Self, FixedEndpointPoolError> {
        config.validate()?;
        if route_keys.is_empty() {
            return Err(FixedEndpointPoolError::NoEndpoints);
        }
        let endpoints = route_keys
            .into_iter()
            .map(|route_key| EndpointSlot {
                route_key,
                healthy: true,
                retired: false,
                in_flight: 0,
                opened_streams: 0,
                down_count: 0,
            })
            .collect();
        Ok(Self {
            endpoints,
            config,
            cursor: 0,
            pre_connect_waiters: 0,
            high_water_in_flight: 0,
            no_healthy_count: 0,
            all_busy_count: 0,
            pre_connect_rejections: 0,
            retained: VecDeque::new(),
        })
    }

    /// Fixed endpoint count.
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// Number of endpoints currently usable (healthy and not retired).
    pub fn healthy_count(&self) -> usize {
        self.endpoints
            .iter()
            .filter(|e| e.healthy && !e.retired)
            .count()
    }

    /// True when no endpoint is usable.
    pub fn is_all_unhealthy(&self) -> bool {
        self.healthy_count() == 0
    }

    /// Round-robin to the next healthy endpoint with stream capacity and
    /// take one in-flight stream slot.
    pub fn pick(&mut self) -> PickOutcome {
        let n = self.endpoints.len();
        if self.is_all_unhealthy() {
            self.no_healthy_count += 1;
            return PickOutcome::NoHealthyEndpoint;
        }
        for step in 0..n {
            let index = (self.cursor + step) % n;
            let slot = &self.endpoints[index];
            if slot.healthy
                && !slot.retired
                && slot.in_flight < self.config.max_in_flight_per_conn
            {
                self.cursor = (index + 1) % n;
                let slot = &mut self.endpoints[index];
                slot.in_flight += 1;
                slot.opened_streams += 1;
                let live = self.total_in_flight();
                self.high_water_in_flight = self.high_water_in_flight.max(live);
                return PickOutcome::Picked { index };
            }
        }
        // Healthy endpoints exist, but all are at the stream cap.
        self.all_busy_count += 1;
        PickOutcome::AllAtStreamCap
    }

    /// Release one in-flight stream slot on an endpoint.
    pub fn release(&mut self, index: usize) {
        if let Some(slot) = self.endpoints.get_mut(index) {
            slot.in_flight = slot.in_flight.saturating_sub(1);
        }
    }

    /// Apply a transport health signal to an endpoint after a stream
    /// completes. Releases the stream slot, and marks the endpoint unhealthy
    /// (retaining a report) on an `Unhealthy` signal.
    pub fn record_signal(&mut self, index: usize, signal: EndpointHealthSignal) {
        self.release(index);
        if let EndpointHealthSignal::Unhealthy(reason) = signal {
            self.mark_unhealthy(index, reason);
        }
    }

    /// Mark an endpoint unhealthy and retire it, retaining a report.
    pub fn mark_unhealthy(&mut self, index: usize, reason: EndpointDownReason) {
        if let Some(slot) = self.endpoints.get_mut(index) {
            if slot.healthy {
                slot.healthy = false;
                slot.retired = true;
                slot.down_count += 1;
                let route_key = slot.route_key.clone();
                self.push_retained(RetiredEndpointReport {
                    index,
                    route_key,
                    reason: RetireReason::Unhealthy(reason),
                });
            }
        }
    }

    /// Bring a retired endpoint back to healthy (e.g. after a successful
    /// reconnect probe). Resets its in-flight count.
    pub fn mark_healthy(&mut self, index: usize) {
        if let Some(slot) = self.endpoints.get_mut(index) {
            slot.healthy = true;
            slot.retired = false;
            slot.in_flight = 0;
        }
    }

    /// Retire an endpoint that has gone idle or stale, retaining a report.
    pub fn retire(&mut self, index: usize, reason: RetireReason) {
        if let Some(slot) = self.endpoints.get_mut(index) {
            if !slot.retired {
                slot.retired = true;
                slot.healthy = false;
                let route_key = slot.route_key.clone();
                self.push_retained(RetiredEndpointReport {
                    index,
                    route_key,
                    reason,
                });
            }
        }
    }

    /// Admit one pre-connect waiter, bounded by the queue cap.
    pub fn admit_pre_connect_waiter(&mut self) -> bool {
        if self.pre_connect_waiters >= self.config.pre_connect_queue_cap {
            self.pre_connect_rejections += 1;
            return false;
        }
        self.pre_connect_waiters += 1;
        true
    }

    /// Release one pre-connect waiter.
    pub fn release_pre_connect_waiter(&mut self) {
        self.pre_connect_waiters = self.pre_connect_waiters.saturating_sub(1);
    }

    /// Total in-flight streams across all endpoints.
    pub fn total_in_flight(&self) -> usize {
        self.endpoints.iter().map(|e| e.in_flight).sum()
    }

    /// Retained retired/idle/stale endpoint reports, oldest first.
    pub fn retained(&self) -> impl Iterator<Item = &RetiredEndpointReport> {
        self.retained.iter()
    }

    /// A pool report snapshot.
    pub fn report(&self) -> FixedEndpointPoolReport {
        FixedEndpointPoolReport {
            endpoint_count: self.endpoints.len(),
            healthy: self.healthy_count(),
            total_in_flight: self.total_in_flight(),
            high_water_in_flight: self.high_water_in_flight,
            pre_connect_waiters: self.pre_connect_waiters,
            max_in_flight_per_conn: self.config.max_in_flight_per_conn,
            pre_connect_queue_cap: self.config.pre_connect_queue_cap,
            no_healthy_count: self.no_healthy_count,
            all_busy_count: self.all_busy_count,
            pre_connect_rejections: self.pre_connect_rejections,
            retained: self.retained.iter().cloned().collect(),
        }
    }

    fn push_retained(&mut self, report: RetiredEndpointReport) {
        if self.retained.len() == self.config.retained_reports {
            self.retained.pop_front();
        }
        self.retained.push_back(report);
    }
}

/// A pool report snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedEndpointPoolReport {
    /// Fixed endpoint count.
    pub endpoint_count: usize,
    /// Currently usable endpoints.
    pub healthy: usize,
    /// In-flight streams right now.
    pub total_in_flight: usize,
    /// Peak in-flight streams observed.
    pub high_water_in_flight: usize,
    /// Pre-connect waiters right now.
    pub pre_connect_waiters: usize,
    /// Per-connection in-flight stream cap.
    pub max_in_flight_per_conn: usize,
    /// Pre-connect waiter cap.
    pub pre_connect_queue_cap: usize,
    /// Times `NoHealthyEndpoint` was returned.
    pub no_healthy_count: u64,
    /// Times every healthy endpoint was at the stream cap.
    pub all_busy_count: u64,
    /// Pre-connect waiters rejected by the queue cap.
    pub pre_connect_rejections: u64,
    /// Retained retired endpoint reports, oldest first.
    pub retained: Vec<RetiredEndpointReport>,
}

/// Outcome of picking an HTTP/2 connection from the pool.
#[derive(Debug, Clone, Copy)]
pub enum Http2PickOutcome {
    /// Route to this endpoint's connection.
    Picked {
        /// Endpoint index in the fixed list.
        index: usize,
        /// The connection isolate address to call.
        connection: Address<Http2ClientMsg, Http2ClientReply>,
    },
    /// Every endpoint is unhealthy.
    NoHealthyEndpoint,
    /// Healthy endpoints exist but all are at the in-flight stream cap.
    AllAtStreamCap,
}

/// A fixed-endpoint HTTP/2 client pool over pre-registered connection
/// isolates.
///
/// The pool selects (round-robin over healthy endpoints) and tracks health;
/// the caller issues the actual [`Http2ClientMsg`] call against the picked
/// connection and feeds the outcome back via [`record_outcome`].
///
/// [`record_outcome`]: Http2ClientPool::record_outcome
pub struct Http2ClientPool {
    connections: Vec<Address<Http2ClientMsg, Http2ClientReply>>,
    state: FixedEndpointPool,
}

impl Http2ClientPool {
    /// Build a pool over a fixed list of targets and their pre-registered
    /// connection addresses (parallel vectors, one connection per target).
    pub fn new(
        targets: &[Http2Target],
        connections: Vec<Address<Http2ClientMsg, Http2ClientReply>>,
        config: FixedEndpointPoolConfig,
    ) -> Result<Self, FixedEndpointPoolError> {
        if targets.len() != connections.len() {
            return Err(FixedEndpointPoolError::NoEndpoints);
        }
        let route_keys = targets.iter().map(Http2Target::route_key).collect();
        Ok(Self {
            connections,
            state: FixedEndpointPool::new(route_keys, config)?,
        })
    }

    /// Pick a healthy connection round-robin, taking one stream slot.
    pub fn pick(&mut self) -> Http2PickOutcome {
        match self.state.pick() {
            PickOutcome::Picked { index } => Http2PickOutcome::Picked {
                index,
                connection: self.connections[index],
            },
            PickOutcome::NoHealthyEndpoint => Http2PickOutcome::NoHealthyEndpoint,
            PickOutcome::AllAtStreamCap => Http2PickOutcome::AllAtStreamCap,
        }
    }

    /// Feed one HTTP/2 outcome back: releases the stream slot and applies the
    /// transport health signal.
    pub fn record_outcome(&mut self, index: usize, outcome: &Http2ClientOutcome) {
        self.state.record_signal(index, http2_health_signal(outcome));
    }

    /// The shared pool state (health, caps, reports).
    pub fn state(&self) -> &FixedEndpointPool {
        &self.state
    }

    /// Mutable access to the shared pool state.
    pub fn state_mut(&mut self) -> &mut FixedEndpointPool {
        &mut self.state
    }

    /// A pool report snapshot.
    pub fn report(&self) -> FixedEndpointPoolReport {
        self.state.report()
    }
}

/// A fixed-endpoint gRPC client pool.
///
/// Wraps an [`Http2ClientPool`]: the same round-robin and health, but the
/// gRPC status is kept first-class. Feed a [`GrpcUnaryOutcome`] back via
/// [`record_unary_outcome`] — a non-OK gRPC *status* keeps the endpoint
/// healthy (the server answered); only a transport failure marks it down.
///
/// [`record_unary_outcome`]: GrpcClientPool::record_unary_outcome
pub struct GrpcClientPool {
    inner: Http2ClientPool,
}

impl GrpcClientPool {
    /// Build a gRPC pool over a fixed list of HTTP/2 targets.
    pub fn new(
        targets: &[Http2Target],
        connections: Vec<Address<Http2ClientMsg, Http2ClientReply>>,
        config: FixedEndpointPoolConfig,
    ) -> Result<Self, FixedEndpointPoolError> {
        Ok(Self {
            inner: Http2ClientPool::new(targets, connections, config)?,
        })
    }

    /// Pick a healthy connection round-robin.
    pub fn pick(&mut self) -> Http2PickOutcome {
        self.inner.pick()
    }

    /// Feed one gRPC unary outcome back: releases the stream slot and applies
    /// the gRPC-aware health signal (status stays healthy, transport down).
    pub fn record_unary_outcome<R>(&mut self, index: usize, outcome: &GrpcUnaryOutcome<R>) {
        self.inner
            .state
            .record_signal(index, grpc_health_signal(outcome));
    }

    /// The shared pool state.
    pub fn state(&self) -> &FixedEndpointPool {
        self.inner.state()
    }

    /// A pool report snapshot.
    pub fn report(&self) -> FixedEndpointPoolReport {
        self.inner.report()
    }
}

/// Register one [`Http2ClientConnection`] per target and bundle them into an
/// [`Http2ClientPool`].
pub fn build_http2_client_pool<S>(
    runtime: &ThreadedRuntime<S, DefaultThreadedMailboxFactory>,
    targets: Vec<Http2Target>,
    limits: Http2ClientLimits,
    config: FixedEndpointPoolConfig,
    connection_mailbox_capacity: usize,
) -> Result<Http2ClientPool, Http2PoolBuildError>
where
    S: Shard + Send + 'static,
{
    let mut connections = Vec::with_capacity(targets.len());
    for target in &targets {
        let conn = Http2ClientConnection::<S>::new(target.clone(), limits);
        let address = runtime
            .register_with_capacity::<Http2ClientConnection<S>, std::convert::Infallible>(
                conn,
                connection_mailbox_capacity,
            )
            .map_err(Http2PoolBuildError::Runtime)?;
        connections.push(address);
    }
    Http2ClientPool::new(&targets, connections, config).map_err(Http2PoolBuildError::Pool)
}

/// Why building an HTTP/2 pool failed.
#[derive(Debug)]
pub enum Http2PoolBuildError {
    /// The runtime failed to register a connection isolate.
    Runtime(ThreadedRuntimeError),
    /// The pool config or endpoint list was invalid.
    Pool(FixedEndpointPoolError),
}

impl std::fmt::Display for Http2PoolBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "runtime register failed: {e:?}"),
            Self::Pool(e) => write!(f, "pool config invalid: {e:?}"),
        }
    }
}

impl std::error::Error for Http2PoolBuildError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::{GrpcStatus, GrpcStatusCode};
    use tina::capacity::CapacityPolicy;
    use tina_runtime::Http2ResetReason;
    use tina_runtime::budget::ServiceBudgetManifest;

    fn pool(n: usize) -> FixedEndpointPool {
        let keys = (0..n).map(|i| format!("ep{i}")).collect();
        let mut cfg = FixedEndpointPoolConfig::balanced();
        cfg.max_in_flight_per_conn = 2;
        cfg.retained_reports = 3;
        FixedEndpointPool::new(keys, cfg).unwrap()
    }

    #[test]
    fn round_robin_spreads_across_healthy_endpoints() {
        let mut p = pool(3);
        let picks: Vec<usize> = (0..3)
            .map(|_| match p.pick() {
                PickOutcome::Picked { index } => index,
                other => panic!("expected pick, got {other:?}"),
            })
            .collect();
        assert_eq!(picks, vec![0, 1, 2], "round robin visits each endpoint");
    }

    #[test]
    fn no_healthy_endpoint_when_all_marked_down() {
        let mut p = pool(2);
        p.mark_unhealthy(0, EndpointDownReason::Closed);
        p.mark_unhealthy(1, EndpointDownReason::Reset);
        assert!(p.is_all_unhealthy());
        assert_eq!(p.pick(), PickOutcome::NoHealthyEndpoint);
        assert_eq!(p.report().no_healthy_count, 1);
        assert_eq!(p.report().retained.len(), 2);
    }

    #[test]
    fn stream_cap_returns_all_busy_then_frees_on_release() {
        let mut p = pool(1); // one endpoint, cap 2
        assert!(matches!(p.pick(), PickOutcome::Picked { index: 0 }));
        assert!(matches!(p.pick(), PickOutcome::Picked { index: 0 }));
        // Both slots taken on the only endpoint.
        assert_eq!(p.pick(), PickOutcome::AllAtStreamCap);
        p.release(0);
        assert!(matches!(p.pick(), PickOutcome::Picked { index: 0 }));
    }

    #[test]
    fn unhealthy_endpoint_is_skipped_by_round_robin() {
        let mut p = pool(3);
        p.mark_unhealthy(1, EndpointDownReason::Transport);
        let picks: Vec<usize> = (0..4)
            .map(|_| match p.pick() {
                PickOutcome::Picked { index } => index,
                other => panic!("got {other:?}"),
            })
            .collect();
        assert!(!picks.contains(&1), "endpoint 1 is down and skipped");
    }

    #[test]
    fn http2_reset_marks_unhealthy_but_grpc_status_keeps_healthy() {
        // A gRPC non-OK status: the server answered. Endpoint stays healthy.
        let mut g = pool(1);
        let status_outcome: GrpcUnaryOutcome<u8> =
            GrpcUnaryOutcome::Status(GrpcStatus::new(GrpcStatusCode::NotFound));
        assert_eq!(
            grpc_health_signal(&status_outcome),
            EndpointHealthSignal::Healthy
        );
        g.record_signal(0, grpc_health_signal(&status_outcome));
        assert_eq!(g.healthy_count(), 1, "gRPC status does not down the endpoint");

        // An HTTP/2 reset: a transport failure. Endpoint goes down.
        let mut h = pool(1);
        let reset = Http2ClientOutcome::Reset(Http2ResetReason::Cancel);
        assert!(matches!(
            http2_health_signal(&reset),
            EndpointHealthSignal::Unhealthy(EndpointDownReason::Reset)
        ));
        h.record_signal(0, http2_health_signal(&reset));
        assert_eq!(h.healthy_count(), 0, "an HTTP/2 reset downs the endpoint");

        // A transport-level gRPC outcome also downs the endpoint.
        let transport: GrpcUnaryOutcome<u8> =
            GrpcUnaryOutcome::Transport(Http2ClientOutcome::Closed);
        assert!(matches!(
            grpc_health_signal(&transport),
            EndpointHealthSignal::Unhealthy(EndpointDownReason::Closed)
        ));
    }

    #[test]
    fn admission_full_is_busy_not_unhealthy() {
        let mut p = pool(1);
        let full = Http2ClientOutcome::Full;
        assert_eq!(http2_health_signal(&full), EndpointHealthSignal::Busy);
        let idx = match p.pick() {
            PickOutcome::Picked { index } => index,
            other => panic!("{other:?}"),
        };
        p.record_signal(idx, http2_health_signal(&full));
        assert_eq!(p.healthy_count(), 1, "admission-full keeps the endpoint up");
    }

    #[test]
    fn pre_connect_waiter_cap_is_bounded() {
        let keys = vec!["a".to_string()];
        let mut cfg = FixedEndpointPoolConfig::balanced();
        cfg.pre_connect_queue_cap = 2;
        let mut p = FixedEndpointPool::new(keys, cfg).unwrap();
        assert!(p.admit_pre_connect_waiter());
        assert!(p.admit_pre_connect_waiter());
        assert!(!p.admit_pre_connect_waiter(), "third waiter rejected");
        assert_eq!(p.report().pre_connect_rejections, 1);
        p.release_pre_connect_waiter();
        assert!(p.admit_pre_connect_waiter());
    }

    #[test]
    fn retained_reports_are_bounded() {
        let keys = (0..5).map(|i| format!("ep{i}")).collect();
        let mut cfg = FixedEndpointPoolConfig::balanced();
        cfg.retained_reports = 2;
        let mut p = FixedEndpointPool::new(keys, cfg).unwrap();
        for i in 0..4 {
            p.mark_unhealthy(i, EndpointDownReason::Transport);
        }
        assert_eq!(p.report().retained.len(), 2);
        assert_eq!(p.report().retained[0].index, 2, "oldest evicted");
    }

    #[test]
    fn idle_and_stale_retire_are_retained_with_reasons() {
        let mut p = pool(3);
        p.retire(0, RetireReason::Idle);
        p.retire(1, RetireReason::Stale);
        let report = p.report();
        assert_eq!(report.healthy, 1);
        assert!(report.retained.iter().any(|r| r.reason == RetireReason::Idle));
        assert!(report.retained.iter().any(|r| r.reason == RetireReason::Stale));
    }

    #[test]
    fn config_rejects_zero_caps() {
        let mut cfg = FixedEndpointPoolConfig::balanced();
        cfg.max_in_flight_per_conn = 0;
        assert_eq!(cfg.validate(), Err(FixedEndpointPoolError::ZeroStreams));
    }

    #[test]
    fn empty_endpoint_list_is_rejected() {
        assert_eq!(
            FixedEndpointPool::new(vec![], FixedEndpointPoolConfig::balanced()).unwrap_err(),
            FixedEndpointPoolError::NoEndpoints
        );
    }

    #[test]
    fn budget_surfaces_name_pool_caps_and_validate() {
        let cfg = FixedEndpointPoolConfig::balanced();
        let surfaces = cfg.budget_surfaces("svc.h2", 3);
        let names: Vec<&str> = surfaces.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"svc.h2.endpoints"));
        assert!(names.contains(&"svc.h2.connections"));
        assert!(names.contains(&"svc.h2.in_flight_streams"));
        assert!(names.contains(&"svc.h2.pre_connect_waiters"));
        assert!(names.contains(&"svc.h2.retained_reports"));
        let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Production);
        m.extend(surfaces);
        m.validate().unwrap();
    }
}
