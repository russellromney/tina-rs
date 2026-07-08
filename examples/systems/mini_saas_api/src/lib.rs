//! Copyable Tina production-service skeleton.
//!
//! This system intentionally keeps the glue local. It assembles native
//! `tina-http`, a controller isolate, a SQLite bridge pool consumer,
//! an outbound keepalive pool, readiness, capacity reporting, graceful
//! shutdown, and one live-replay fact without becoming a web framework.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use tina_runtime::budget::BudgetManifestReport;
use tina_runtime::lifecycle::{Health, Lifecycle, ServiceShutdownReport, ServiceTopology};

pub mod budget;
pub mod tina_impl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Smoke,
    Pressure,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Typed startup topology built by the service. Names every isolate,
    /// bridge, pool, and listener the service registered, plus the
    /// pressure surfaces those resources expose.
    pub topology: Option<ServiceTopology>,
    /// Health snapshot taken right before the host begins the shutdown
    /// choreography. Pairs the typed [`tina_runtime::lifecycle::Lifecycle`]
    /// the controller is in with the live pressure report.
    pub health_pre_shutdown: Option<Health>,
    /// Typed shutdown choreography report. Records every step the host
    /// drove (close ingress, drain in-flight, close DB, close pool, stop
    /// listeners, stop runtime), plus per-resource close reports. The
    /// [`tina_runtime::lifecycle::ServiceShutdownReport::clean`] flag is
    /// the typed counterpart to the existing `shutdown_clean` boolean.
    pub shutdown_report: Option<ServiceShutdownReport>,
    /// Lifecycle states the service was observed in, in order. The
    /// canonical sequence is
    /// `[Starting, Ready, Draining, Stopped]`. Built explicitly by the
    /// host so the plan's "service starts NotReady, becomes Ready,
    /// enters Draining, then Stopped" assertion is a typed witness
    /// rather than implied by separate fields.
    pub lifecycle_transitions: Vec<Lifecycle>,
    pub health_ok: bool,
    pub ready_ok: bool,
    pub created_item: bool,
    pub read_item: bool,
    pub notified_item: bool,
    pub notify_after_peer_close: bool,
    pub missing_404: bool,
    pub method_405: bool,
    pub bad_request_400: bool,
    pub body_cap_413: bool,
    pub db_constraint_409: bool,
    pub outbound_pressure_503: bool,
    pub ready_after_db_close_503: bool,
    pub ready_during_shutdown_503: bool,
    pub ingress_rejects_after_close: bool,
    pub shutdown_in_flight_typed: bool,
    pub shutdown_clean: bool,
    pub multi_turn_notify: bool,
    /// Owner-stop scope sweep line: `scopes_drained scopes_cancelled=N
    /// children_cancelled=M unreleased=K`. The graceful shutdown completes
    /// in-flight work first, so this reports zero unreleased capacity.
    pub scopes_drain_line: String,
    /// `true` when the owner-stop sweep reported zero unreleased scope
    /// capacity.
    pub scopes_drain_unreleased_zero: bool,
    pub capacity_before_shutdown_line: String,
    pub capacity_during_shutdown_line: String,
    /// Budget manifest joined with the live pressure report at
    /// shutdown: configured cap + observed use/full per surface.
    pub budget_report: Option<BudgetManifestReport>,
    /// One-line summary of [`Self::budget_report`].
    pub budget_report_line: String,
    /// One-line replay export: schema + hash over replay-affecting caps.
    pub budget_replay_line: String,
    /// `true` if every declared surface matched the live report.
    pub budget_consistent: bool,
    pub terminal_line: String,
    pub startup_summary_line: String,
    pub startup_discovery_lines: Vec<String>,
    pub live_replay_fact: String,
    pub observations: Vec<UserObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserObservation {
    pub label: &'static str,
    pub status: u16,
    pub body: String,
}

impl RunReport {
    pub fn summary_line(&self) -> String {
        format!(
            "system=mini_saas_api health_ok={} ready_ok={} created_item={} read_item={} \
             notified_item={} notify_after_peer_close={} missing_404={} method_405={} bad_request_400={} body_cap_413={} \
             db_constraint_409={} outbound_pressure_503={} ready_after_db_close_503={} \
             ready_during_shutdown_503={} ingress_rejects_after_close={} \
             shutdown_in_flight_typed={} shutdown_clean={} multi_turn_notify={}",
            self.health_ok,
            self.ready_ok,
            self.created_item,
            self.read_item,
            self.notified_item,
            self.notify_after_peer_close,
            self.missing_404,
            self.method_405,
            self.bad_request_400,
            self.body_cap_413,
            self.db_constraint_409,
            self.outbound_pressure_503,
            self.ready_after_db_close_503,
            self.ready_during_shutdown_503,
            self.ingress_rejects_after_close,
            self.shutdown_in_flight_typed,
            self.shutdown_clean,
            self.multi_turn_notify,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseParts {
    pub status: u16,
    pub body: String,
}

pub fn run(mode: RunMode) -> anyhow::Result<RunReport> {
    tina_impl::run(mode)
}

/// Bind the HTTP server on `addr` and run until SIGINT/SIGTERM, then drain
/// gracefully and return. The copyable run-forever entrypoint: same service
/// assembly and shutdown choreography as [`run`], with no scripted traffic.
pub fn serve(addr: SocketAddr) -> anyhow::Result<()> {
    tina_impl::serve(addr)
}

/// Typed outcome of [`prove_graceful_drain_completes_in_flight`].
#[derive(Debug, Clone)]
pub struct GracefulDrainProof {
    /// Status the held-mid-flight notify request received once the drain ran.
    pub in_flight_status: u16,
    /// Its trimmed response body.
    pub in_flight_body: String,
    /// `true` when the in-flight request completed with its real `notified`
    /// answer — i.e. the drain let it finish instead of dropping it.
    pub in_flight_notified: bool,
    /// `true` when the whole drain (including the outbound pool) closed clean.
    pub shutdown_clean: bool,
    /// Terminal shutdown line (same shape as `RunReport::terminal_line`).
    pub terminal_line: String,
    /// Owner-stop sweep line the drain reported.
    pub scopes_drain_line: String,
}

/// Hold one notify request mid-outbound, then run the graceful drain with
/// `drain_deadline`. A deadline longer than the in-flight work proves the
/// request completes and its pool lease is released; a shorter one proves the
/// straggler is force-cancelled (unclean). Regression witness for the drain.
pub fn prove_graceful_drain_completes_in_flight(
    drain_deadline: std::time::Duration,
) -> anyhow::Result<GracefulDrainProof> {
    tina_impl::prove_graceful_drain_completes_in_flight(drain_deadline)
}

/// Typed outcome of [`prove_drain_cancels_active_scope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainActiveReport {
    /// Scopes cancelled by the owner-stop sweep (one active notify).
    pub scopes_cancelled: usize,
    /// Child rails still pending at sweep time and cancelled.
    pub children_cancelled: usize,
    /// Scope-set slots left occupied after the sweep.
    pub unreleased: usize,
    /// The stranded slow notify got an error / non-`notified` answer.
    pub slow_notify_aborted: bool,
    /// Raw `scopes_drained …` line the sweep replied.
    pub drain_line: String,
}

/// Drive a notify request held mid-outbound, then drain the scope set so
/// the still-pending outbound child is cancelled. Proves the request scope
/// in the copied service is functional, not decorative.
pub fn prove_drain_cancels_active_scope() -> anyhow::Result<DrainActiveReport> {
    tina_impl::prove_drain_cancels_active_scope()
}

/// Tunables for [`run_soak`].
#[derive(Debug, Clone, Copy)]
pub struct SoakConfig {
    /// Concurrent client workers hitting the service.
    pub workers: usize,
    /// Total number of ops to drive across all workers.
    pub op_count: u64,
    /// Per-request connect timeout for the harness clients.
    pub connect_timeout: std::time::Duration,
}

impl Default for SoakConfig {
    fn default() -> Self {
        Self {
            workers: 4,
            op_count: 200,
            connect_timeout: std::time::Duration::from_secs(2),
        }
    }
}

/// Aggregate report from one [`run_soak`] invocation.
#[derive(Debug, Clone)]
pub struct SoakReport {
    /// Typed load summary from the harness (latency, ok/err/timeout, leak).
    pub load: tina_proof_harness::LoadReport,
    /// `/debug/capacity` line captured after the load run, before shutdown.
    pub capacity_after_load_line: String,
    /// Terminal shutdown line (same shape as `RunReport::terminal_line`).
    pub terminal_line: String,
    /// `true` if the runtime drained cleanly (no in-flight leaks, no
    /// outbound failures, capacity surfaces accounted for).
    pub shutdown_clean: bool,
}

impl SoakReport {
    pub fn summary_line(&self) -> String {
        format!(
            "soak {} shutdown_clean={} capacity={{{}}}",
            self.load.summary_line(),
            self.shutdown_clean,
            self.capacity_after_load_line,
        )
    }
}

/// Drive a small load/soak workload against the service and report a
/// typed [`SoakReport`].
pub fn run_soak(config: SoakConfig) -> anyhow::Result<SoakReport> {
    tina_impl::run_soak(config)
}

pub fn one_request(addr: SocketAddr, request: &[u8]) -> anyhow::Result<ResponseParts> {
    one_request_with_timeout(addr, request, Duration::from_secs(2))
}

/// Like [`one_request`] but with an explicit connect/read timeout.
/// Used by [`run_soak`] so the harness inherits `SoakConfig::connect_timeout`.
pub fn one_request_with_timeout(
    addr: SocketAddr,
    request: &[u8],
    timeout: Duration,
) -> anyhow::Result<ResponseParts> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    // Read timeout is the same window; we never want a stuck server
    // to wedge a soak worker.
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(request)?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    parse_response(&response)
}

pub fn parse_response(response: &[u8]) -> anyhow::Result<ResponseParts> {
    let separator = b"\r\n\r\n";
    let header_end = response
        .windows(separator.len())
        .position(|w| w == separator)
        .ok_or_else(|| anyhow::anyhow!("response missing CRLFCRLF: {response:?}"))?;
    let head = std::str::from_utf8(&response[..header_end])?;
    let line = head
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("response missing status line"))?;
    let status = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("response missing status code: {line:?}"))?
        .parse::<u16>()?;
    let body = std::str::from_utf8(&response[header_end + separator.len()..])
        .unwrap_or("")
        .to_owned();
    Ok(ResponseParts { status, body })
}

pub fn get(addr: SocketAddr, path: &str) -> anyhow::Result<ResponseParts> {
    one_request(
        addr,
        format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
    )
}

pub fn post(addr: SocketAddr, path: &str, body: &str) -> anyhow::Result<ResponseParts> {
    one_request(
        addr,
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .as_bytes(),
    )
}

pub fn put(addr: SocketAddr, path: &str) -> anyhow::Result<ResponseParts> {
    one_request(
        addr,
        format!("PUT {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
    )
}
