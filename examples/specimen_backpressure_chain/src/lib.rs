//! Tokio-vs-Tina backpressure chain.
//!
//! A → B → C with one shared deadline. Each request takes a
//! [`TOTAL_DEADLINE_MS`] budget. C's work time is variable: some
//! requests finish in [`FAST_C_MS`] (well under budget), some take
//! [`SLOW_C_MS`] (well over). The driver fires
//! [`REQUEST_COUNT`] requests through the chain and counts how many
//! finished cleanly vs how many timed out — and crucially, *which
//! hop* the timeout was observed at.
//!
//! What we are looking at:
//!
//! - **Tokio**: A wraps the whole chain in
//!   `tokio::time::timeout(TOTAL_DEADLINE)`; B and C don't propagate
//!   anything. When the outer timer fires, the futures are dropped
//!   bottom-up; B and C never know the caller went away.
//! - **Tina**: A computes a deadline at the start, calls B with
//!   `TOTAL_DEADLINE` left, B subtracts its observed wall-clock and
//!   calls C with the *remaining* budget. When the budget elapses,
//!   the failure surfaces at exactly the hop that ran out of time —
//!   `CallOutcome::Timeout` from C reaches B, B reports the partial
//!   shape to A. No invisible drops.
//!
//! The Report counts:
//!
//! - `successful` — A → B → C all completed within budget;
//! - `c_timed_out` — Tina: B observed `CallOutcome::Timeout` from C
//!   (or Tokio: outer timeout fired while we believe C was the slow
//!   hop). Counted from each side's own visible state;
//! - `b_timed_out` — A's call to B expired before B produced a typed reply;
//! - `caller_timeout` — the driver's own wait for A expired;
//! - `full`, `closed`, `rejected`, `domain_failure`, and `runtime_failure` — distinct
//!   terminal truths preserved through every hop.

pub mod tina_impl;
pub mod tokio_impl;

/// Total wall-clock budget for the whole A → B → C chain.
pub const TOTAL_DEADLINE_MS: u64 = 80;

/// Fast C work time. Below the budget by a comfortable margin.
pub const FAST_C_MS: u64 = 20;

/// Slow C work time. Above the budget so the deadline fires.
pub const SLOW_C_MS: u64 = 200;

/// Number of requests the driver fires sequentially through the
/// chain.
pub const REQUEST_COUNT: u32 = 6;

/// Whether the C-hop is fast on this iteration. Both sides walk the
/// same script so each request hits the same C-time.
pub fn c_is_slow(i: u32) -> bool {
    // Pattern: alternating slow/fast, biased so we get a clear mix.
    // 0 fast, 1 slow, 2 fast, 3 slow, 4 fast, 5 slow.
    i % 2 == 1
}

/// One fast request exercises a real service-domain failure.
pub fn c_is_domain_failure(i: u32) -> bool {
    i == 4
}

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Requests where A → B → C all replied within the budget.
    pub successful: u32,
    /// Requests where B can name C as the timed-out hop. Tina reports this
    /// when B observes `CallOutcome::Timeout`; Tokio leaves it zero because
    /// its outer timeout cannot recover hop provenance.
    pub c_timed_out: u32,
    /// A's wait for B expired before B produced a typed reply.
    pub b_timed_out: u32,
    /// The driver's own wait for A expired.
    pub caller_timeout: u32,
    /// A bounded call admission was full.
    pub full: u32,
    /// The destination was closed.
    pub closed: u32,
    /// The runtime rejected the call for another typed reason.
    pub rejected: u32,
    /// The service completed with a domain failure.
    pub domain_failure: u32,
    /// Runtime-owned continuation work failed.
    pub runtime_failure: u32,
    /// Whether each side reached the end of `run` cleanly.
    pub exit_clean: bool,
}

/// Expected Tina counts under the constants above: two successful requests,
/// three typed C-hop timeouts, and one service-domain failure.
pub fn expected_tina_report() -> Report {
    let slow_count = (0..REQUEST_COUNT).filter(|i| c_is_slow(*i)).count() as u32;
    let domain_failure_count = (0..REQUEST_COUNT)
        .filter(|i| c_is_domain_failure(*i))
        .count() as u32;
    let fast_count = REQUEST_COUNT - slow_count - domain_failure_count;
    Report {
        successful: fast_count,
        c_timed_out: slow_count,
        b_timed_out: 0,
        caller_timeout: 0,
        full: 0,
        closed: 0,
        rejected: 0,
        domain_failure: domain_failure_count,
        runtime_failure: 0,
        exit_clean: true,
    }
}

/// Tokio can only name its outer caller timeout, not the C hop.
pub fn expected_tokio_report() -> Report {
    let slow_count = (0..REQUEST_COUNT).filter(|i| c_is_slow(*i)).count() as u32;
    let domain_failure_count = (0..REQUEST_COUNT)
        .filter(|i| c_is_domain_failure(*i))
        .count() as u32;
    let fast_count = REQUEST_COUNT - slow_count - domain_failure_count;
    Report {
        successful: fast_count,
        caller_timeout: slow_count,
        domain_failure: domain_failure_count,
        exit_clean: true,
        ..Report::default()
    }
}
