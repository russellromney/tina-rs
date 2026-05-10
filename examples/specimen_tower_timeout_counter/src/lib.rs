//! Tokio-vs-Tina counter behind a Tower middleware stack.
//!
//! Both sides run a `Counter` service that takes [`SLOW_HANDLER_MS`]
//! per request, wrap it in a Tower stack with `ConcurrencyLimit(2)`
//! and `Timeout(150ms)`, and drive [`BURST`] requests through it
//! concurrently. The point is to compare how Tower middleware
//! composes over each runtime's notion of pressure.
//!
//! - **Tokio**: the service is `tower::service_fn` calling an
//!   in-process `Arc<Mutex<Counter>>`; sleep is `tokio::sleep`.
//! - **Tina**: the service is a registered Tina isolate behind
//!   `TinaTowerService`. The slow work is `sleep(...).reply(Done)`
//!   inside the isolate; admission backpressure surfaces as
//!   `BridgeError::Full`, drain truth as `BridgeError::Closed`.
//!
//! Both sides bucket each call's outcome into:
//!
//! - 200 OK (`successful_count`),
//! - 503 (`service_unavailable_count`) — `Buffer`/`Concurrency`
//!   admission rejection,
//! - 504 (`gateway_timeout_count`) — Tower `Timeout` layer fired.

pub mod tina_impl;
pub mod tokio_impl;

/// Tower `Timeout` layer deadline. Smaller than [`SLOW_HANDLER_MS`]
/// times the burst, so timeout fires for the late entries.
pub const TIMEOUT_MS: u64 = 150;

/// Per-call work time inside the service. Slow enough that the
/// concurrency limit matters.
pub const SLOW_HANDLER_MS: u64 = 100;

/// Concurrent in-flight calls allowed by the `ConcurrencyLimit` layer.
pub const CONCURRENCY: usize = 2;

/// Number of calls the driver dispatches in parallel.
pub const BURST: u32 = 8;

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub successful_count: u32,
    pub service_unavailable_count: u32,
    pub gateway_timeout_count: u32,
    pub exit_clean: bool,
}

/// Asserts the structural invariants both sides should satisfy. The
/// exact split between successful / 503 / 504 is intentionally
/// different across sides: Tokio's `tower::service_fn` is
/// multi-threaded and pipelines through `ConcurrencyLimit(2)` so most
/// or all calls finish before `Timeout(150ms)` bites; the Tina
/// isolate handles one mailbox message per turn so the same Tower
/// stack fires `Timeout` for the late requests. Both shapes are
/// correct under the same middleware — the README documents why.
pub fn assert_report_invariants(side: &str, report: &Report) {
    let total = report.successful_count
        + report.service_unavailable_count
        + report.gateway_timeout_count;
    assert_eq!(
        total, BURST,
        "{side}: every call should produce one outcome, got {report:?}",
    );
    assert!(
        report.successful_count > 0,
        "{side}: at least one call should succeed, got {report:?}",
    );
    assert!(report.exit_clean, "{side}: expected exit_clean, got {report:?}");
}
