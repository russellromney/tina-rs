//! Webhook fanout. One event delivers to N endpoints in parallel
//! through `tina-reqwest-bridge` (Tina) or `reqwest` directly
//! (Tokio). The report buckets each endpoint's outcome.
//!
//! Endpoints in this specimen:
//!
//! - 2 healthy (return `200`),
//! - 1 always returns `503`,
//! - 1 sleeps 500 ms (per-call timeout fires).

pub mod tina_impl;
pub mod tokio_impl;

pub mod upstream;

pub const HEALTHY: u32 = 2;
pub const UNAVAILABLE: u32 = 1;
pub const SLOW: u32 = 1;
pub const TOTAL_ENDPOINTS: u32 = HEALTHY + UNAVAILABLE + SLOW;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub delivered: u32,
    pub server_unavailable: u32,
    pub timed_out: u32,
    pub other: u32,
    pub exit_clean: bool,
}

pub fn assert_report_invariants(side: &str, r: &Report) {
    let total = r.delivered + r.server_unavailable + r.timed_out + r.other;
    assert_eq!(total, TOTAL_ENDPOINTS, "{side}: {r:?}");
    assert_eq!(r.delivered, HEALTHY, "{side}: {r:?}");
    assert_eq!(r.server_unavailable, UNAVAILABLE, "{side}: {r:?}");
    assert_eq!(r.timed_out, SLOW, "{side}: {r:?}");
    assert!(r.exit_clean, "{side}: {r:?}");
}
